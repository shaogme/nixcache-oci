pub mod backend;
pub mod client;
pub mod codec;
pub mod error;
pub mod manifest;
pub mod mock;
pub mod mutation;
pub mod token;
pub mod transport;
pub mod upload;

pub use backend::{
    AwsEcrDriver, AzureAcrDriver, BlobUploadStrategy, DockerHubDriver, GcpArtifactRegistryDriver,
    GenericOciDriver, GhcrDriver, GitHubContainerMetadata, GitHubPackageVersion,
    GitHubPackageVersionMetadata, GitHubPackagesClient, OciBackendDriver, OciDriver,
    RegistryCapabilities, RegistryDeletionStrategy, RegistryKind, detect_driver, driver_for_kind,
};
pub use client::{DeletionSummary, FetchedOciArtifact, OciClient};
pub use codec::{DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec};
pub use error::{OciError, TokenError, TransportError};
pub use manifest::{
    CacheLayerMediaType, CacheLayerMediaTypeV5, EMPTY_CONFIG_DIGEST, EMPTY_CONFIG_SIZE,
    OCI_IMAGE_CONFIG_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MANIFEST_MEDIA_TYPE,
    OciArtifactManifest, OciDescriptor, OciImageIndex, OciImageManifest, OciPlatform,
    ShardedArchIndexManifestParams, build_delta_patch_manifest, build_image_index,
    build_sharded_arch_index_manifest,
};
pub use mock::{MockResponse, MockRouterTransport};
pub use mutation::SessionMutationRequest;
pub use nixcache_core::{
    BloomFilter, BloomFilterManifest, BuildReceipt, BuildStats, CACHE_INDEX_VERSION,
    DeltaPatchData, FastBlockedBloomFilter, IndexEntry, JobSummaryMetadata, NUM_SHARDS, NarDigest,
    NarInfo, NarInfoMeta, RECEIPT_VERSION, RUN_SESSION_VERSION, SCHEMA_VERSION, SCHEMA_VERSION_V5,
    ShardDataPayload, ShardDescriptor, ShardedArchCacheIndexData, StoreHash, SystemArch,
    build_nar_lookup_map, calculate_shard_id, compute_merkle_root, compute_shard_merkle_hash,
    diff_shard_descriptors, evaluate_multi_arch_gc, extract_nar_basename, extract_store_hash,
    extract_store_hash_str, partition_entries_by_shard, shard_id_to_prefix,
};
pub use token::TokenManager;
pub use transport::{
    BoxBodyStream, HashingStream, OciBlobStream, OciTransport, StreamHashState,
    UploadChunkResponse, UploadSessionInfo, parse_range_header,
};
pub use upload::{BlobPayload, UploadConfig};

#[cfg(test)]
mod tests {
    use super::{
        AwsEcrDriver, BlobUploadStrategy, BloomFilterManifest, DockerHubDriver,
        EMPTY_CONFIG_DIGEST, FastBlockedBloomFilter, GenericOciDriver, GhcrDriver, HashingStream,
        IndexEntry, MockResponse, MockRouterTransport, NarDigest, NarInfoMeta, OciClient,
        OciDescriptor, OciError, OciImageIndex, OciPlatform, RegistryDeletionStrategy,
        RegistryKind, SessionMutationRequest, ShardDataPayload, ShardedArchCacheIndexData,
        StoreHash, StreamHashState, SystemArch, TransportError, UploadConfig, build_image_index,
        parse_range_header,
    };
    use bytes::Bytes;
    use futures_util::StreamExt;
    use http::{HeaderMap, StatusCode};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;

    #[test]
    fn test_driver_capabilities_and_canonicalization() {
        let ghcr = GhcrDriver;
        assert_eq!(ghcr.kind(), RegistryKind::Ghcr);
        assert!(!ghcr.capabilities().supports_chunked_patch);
        assert_eq!(
            ghcr.capabilities().fixed_upload_strategy,
            BlobUploadStrategy::FixedTwoStepPut
        );
        assert_eq!(
            ghcr.capabilities().deletion_strategy,
            RegistryDeletionStrategy::GitHubPackagesRestApi
        );
        assert!(!ghcr.capabilities().supports_blob_physical_deletion);
        assert!(ghcr.capabilities().supports_package_deletion);
        assert_eq!(ghcr.canonicalize_endpoint("  GHCR.IO "), "ghcr.io");
        assert_eq!(ghcr.canonicalize_repository("/Owner/Repo/"), "owner/repo");
        assert_eq!(
            ghcr.format_auth_scope("owner/repo", true),
            "repository:owner/repo/nix-cache:pull,push"
        );
        assert_eq!(
            ghcr.resolve_token_endpoint("ghcr.io", "owner/repo", true),
            "https://ghcr.io/token?service=ghcr.io&scope=repository:owner/repo/nix-cache:pull,push"
        );

        let docker = DockerHubDriver;
        assert_eq!(docker.kind(), RegistryKind::DockerHub);
        assert!(docker.capabilities().supports_chunked_patch);
        assert_eq!(
            docker.capabilities().fixed_upload_strategy,
            BlobUploadStrategy::PreferMonolithicPost
        );
        assert_eq!(
            docker.capabilities().deletion_strategy,
            RegistryDeletionStrategy::DockerHubRestApi
        );
        assert!(!docker.capabilities().supports_blob_physical_deletion);
        assert_eq!(
            docker.canonicalize_endpoint("docker.io"),
            "registry-1.docker.io"
        );
        assert_eq!(
            docker.canonicalize_endpoint("index.docker.io"),
            "registry-1.docker.io"
        );
        assert_eq!(docker.canonicalize_repository("ubuntu"), "library/ubuntu");
        assert_eq!(docker.canonicalize_repository("user/repo"), "user/repo");
        assert_eq!(
            docker.format_auth_scope("ubuntu", false),
            "repository:library/ubuntu/nix-cache:pull"
        );
        assert_eq!(
            docker.resolve_token_endpoint("docker.io", "ubuntu", false),
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/ubuntu/nix-cache:pull"
        );

        let generic = GenericOciDriver;
        assert_eq!(generic.kind(), RegistryKind::GenericOci);
        assert_eq!(
            generic.capabilities().deletion_strategy,
            RegistryDeletionStrategy::StandardOciDelete
        );
        assert!(generic.capabilities().supports_blob_physical_deletion);
        assert_eq!(
            generic.resolve_token_endpoint("localhost:5000", "myrepo", true),
            "http://localhost:5000/token?service=localhost:5000&scope=repository:myrepo/nix-cache:pull,push"
        );

        let aws = AwsEcrDriver;
        assert_eq!(aws.kind(), RegistryKind::AwsEcr);
        assert!(!aws.capabilities().supports_chunked_patch);
        assert_eq!(
            aws.capabilities().deletion_strategy,
            RegistryDeletionStrategy::AwsEcrApi
        );
        assert!(!aws.capabilities().supports_blob_physical_deletion);
    }

    #[tokio::test]
    async fn test_oci_client_token_fallback() {
        let transport = MockRouterTransport::default();
        transport.add_route(
            "GET",
            "/token",
            MockResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );

        let client = OciClient::with_transport(
            "example.com",
            "test/repo",
            "fallback-token",
            true,
            transport,
        );
        let token = client
            .get_token()
            .await
            .expect("Should fallback to github_token");
        assert_eq!(token.as_ref(), "fallback-token");
    }

    #[tokio::test]
    async fn test_head_and_get_blob_mock() {
        let transport = MockRouterTransport::default();
        transport.add_route(
            "HEAD",
            "/blobs/sha256:exists",
            MockResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );
        transport.add_route(
            "GET",
            "/blobs/sha256:data123",
            MockResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::from_static(b"hello blob content"),
            },
        );

        let client = OciClient::with_transport("example.com", "test/repo", "", false, transport);

        assert!(client.head_blob("sha256:exists").await.unwrap());
        assert!(!client.head_blob("sha256:missing").await.unwrap());

        let bytes = client.get_blob("sha256:data123").await.unwrap();
        assert_eq!(&bytes[..], b"hello blob content");

        let err = client.get_blob("sha256:notfound").await.unwrap_err();
        assert!(matches!(err, OciError::BlobNotFound { .. }));
    }

    #[tokio::test]
    async fn test_get_and_push_manifest_mock() {
        let manifest_content =
            r#"{"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json"}"#;

        let transport = MockRouterTransport::default();
        transport.add_route(
            "GET",
            "/manifests/cache-index",
            MockResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::from(manifest_content),
            },
        );
        transport.add_route(
            "PUT",
            "/manifests/fail-tag",
            MockResponse {
                status: StatusCode::FORBIDDEN,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );

        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);
        let fetched = client.get_manifest("cache-index").await.unwrap();
        assert_eq!(fetched, Some(manifest_content.to_string()));

        let push_res = client.push_manifest("cache-index", manifest_content).await;
        assert!(push_res.is_ok());

        let fail_res = client.push_manifest("fail-tag", manifest_content).await;
        assert!(matches!(
            fail_res,
            Err(OciError::ManifestPushFailed(StatusCode::FORBIDDEN))
        ));
    }

    #[tokio::test]
    async fn test_push_manifest_ensures_empty_config_blob_mock() {
        let manifest_content = format!(
            r#"{{"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "config": {{"digest": "{}"}}}}"#,
            EMPTY_CONFIG_DIGEST
        );

        let transport = MockRouterTransport::default();
        // 初始状态下 HEAD empty config blob 返回 404
        transport.add_route(
            "HEAD",
            &format!("/blobs/{}", EMPTY_CONFIG_DIGEST),
            MockResponse {
                status: StatusCode::NOT_FOUND,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );
        // 上传 empty config blob 的 1-RTT monolithic post
        transport.add_route(
            "POST",
            &format!("/blobs/uploads/?digest={}", EMPTY_CONFIG_DIGEST),
            MockResponse {
                status: StatusCode::CREATED,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );
        // 上传 manifest
        transport.add_route(
            "PUT",
            "/manifests/cache-index-x86",
            MockResponse {
                status: StatusCode::CREATED,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );

        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);
        let push_res = client
            .push_manifest("cache-index-x86", &manifest_content)
            .await;
        assert!(push_res.is_ok());
    }

    #[test]
    fn test_oci_error_display() {
        let upload_err = OciError::BlobUploadFailed(StatusCode::BAD_REQUEST);
        assert!(format!("{}", upload_err).contains("400"));

        let download_err = OciError::BlobDownloadFailed(StatusCode::INTERNAL_SERVER_ERROR);
        assert!(format!("{}", download_err).contains("500"));

        let not_found_err = OciError::BlobNotFound {
            digest: "sha256:123".to_string(),
        };
        assert_eq!(
            format!("{}", not_found_err),
            "Target blob 'sha256:123' not found on registry"
        );

        let manifest_err = OciError::ManifestPushFailed(StatusCode::INTERNAL_SERVER_ERROR);
        assert!(format!("{}", manifest_err).contains("500"));

        let cas_err = OciError::CasPreconditionFailed {
            tag: "run-123".to_string(),
            expected: Some("sha256:old".to_string()),
            actual: None,
        };
        assert!(
            format!("{}", cas_err)
                .contains("CAS optimistic concurrency precondition failed on tag 'run-123'")
        );
    }

    #[tokio::test]
    async fn test_put_manifest_conditional_cas_conflict() {
        let transport = MockRouterTransport::default();
        transport.add_route(
            "PUT",
            "/manifests/run-123",
            MockResponse {
                status: StatusCode::PRECONDITION_FAILED,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );

        // 使用支持 CAS If-Match 的 Generic 驱动
        let client = OciClient::new(
            "example.com",
            "test/repo",
            "",
            true,
            GenericOciDriver,
            transport,
        );
        let err = client
            .put_manifest_conditional("run-123", "{}", Some("sha256:old"))
            .await
            .unwrap_err();

        assert!(matches!(err, OciError::CasPreconditionFailed { tag, .. } if tag == "run-123"));
    }

    #[tokio::test]
    async fn test_delete_manifest_mock() {
        let transport = MockRouterTransport::default();
        transport.add_route(
            "DELETE",
            "/manifests/run-old",
            MockResponse {
                status: StatusCode::ACCEPTED,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );
        transport.add_route(
            "DELETE",
            "/manifests/run-missing",
            MockResponse {
                status: StatusCode::NOT_FOUND,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );

        let client = OciClient::new(
            "example.com",
            "test/repo",
            "",
            true,
            GenericOciDriver,
            transport,
        );
        assert!(client.delete_manifest_strict("run-old").await.is_ok());
        assert!(client.delete_manifest_strict("run-missing").await.is_ok());
    }

    #[tokio::test]
    async fn test_delete_blob_and_batch_delete_mock() {
        let transport = MockRouterTransport::default();
        transport.add_route(
            "DELETE",
            "/blobs/sha256:blob1",
            MockResponse {
                status: StatusCode::ACCEPTED,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );
        transport.add_route(
            "DELETE",
            "/blobs/sha256:blob2",
            MockResponse {
                status: StatusCode::NOT_FOUND,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );
        transport.add_route(
            "DELETE",
            "/blobs/sha256:blob3",
            MockResponse {
                status: StatusCode::METHOD_NOT_ALLOWED,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );

        let client = OciClient::new(
            "example.com",
            "test/repo",
            "",
            true,
            GenericOciDriver,
            transport,
        );
        assert!(client.delete_blob_strict("sha256:blob1").await.is_ok());
        assert!(client.delete_blob_strict("sha256:blob2").await.is_ok());
        let err3 = client.delete_blob_strict("sha256:blob3").await.unwrap_err();
        assert!(matches!(err3, OciError::OperationNotSupported { .. }));

        let digests = vec![
            NarDigest::new_unchecked("sha256:blob1"),
            NarDigest::new_unchecked("sha256:blob2"),
            NarDigest::new_unchecked("sha256:blob3"),
        ];
        let summary = client
            .batch_delete_blobs_strict(&digests, 2, false)
            .await
            .unwrap();
        assert_eq!(summary.deleted_count, 2); // blob1 and blob2 (404 idempotent)
        assert_eq!(summary.failed_count, 1); // blob3 (405 error)

        let strict_err = client
            .batch_delete_blobs_strict(&digests, 2, true)
            .await
            .unwrap_err();
        assert!(matches!(strict_err, OciError::OperationNotSupported { .. }));
    }

    #[tokio::test]
    async fn test_update_run_session_with_cas_mock() {
        let transport = MockRouterTransport::default();
        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);

        let mut entries = HashMap::new();
        let hash_x86 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        entries.insert(
            hash_x86.clone(),
            IndexEntry {
                name: "pkg-x86".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-pkg", hash_x86),
                    nar_basename: "pkg-x86.nar.xz".to_string(),
                    nar_hash:
                        "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                            .to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_sha256(
                    "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
                )
                .unwrap(),
                nar_size: 1024,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: Some("job:vm-tests".to_string()),
            },
        );

        let request = SessionMutationRequest::new(12345, "vm-tests", SystemArch::X86_64Linux)
            .with_entries(entries)
            .with_roots(vec![hash_x86])
            .with_git_info(
                Some("commit-sha-123".to_string()),
                Some("refs/heads/main".to_string()),
            )
            .with_public_key(Some("key:pub".to_string()))
            .with_upload_stats(1, 1024)
            .with_max_retries(3);

        let res = client.update_run_session_with_cas(request).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_sharded_root_index_and_shard_data_flow_mock() {
        let transport = MockRouterTransport::default();
        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);

        // 1. ShardDataPayload
        let mut shard_payload = ShardDataPayload::new(42);
        let h1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        shard_payload
            .entries
            .insert(h1.clone(), IndexEntry::default());

        let (shard_digest, comp_size, uncomp_size) = client
            .push_shard_data(&shard_payload)
            .await
            .expect("Push shard data should succeed");
        assert!(shard_digest.starts_with("sha256:"));
        assert!(comp_size > 0);
        assert!(uncomp_size > 0);

        let retrieved_shard = client
            .get_shard_data(&shard_digest)
            .await
            .expect("Get shard data should succeed");
        assert_eq!(retrieved_shard.shard_id, 42);
        assert_eq!(retrieved_shard.entries.len(), 1);

        // 2. Bloom filter
        let mut bloom = FastBlockedBloomFilter::new_with_defaults(100);
        bloom.insert(&h1);
        let bf_manifest: BloomFilterManifest = client
            .push_bloom_filter(&bloom)
            .await
            .expect("Push bloom filter should succeed");

        let retrieved_bloom = client
            .get_bloom_filter(
                &bf_manifest.blob_digest,
                bf_manifest.num_entries,
                bf_manifest.num_hashes,
            )
            .await
            .expect("Get bloom filter should succeed");
        assert!(retrieved_bloom.contains(&h1));

        // 3. ShardedArchCacheIndexData
        let mut root_index =
            ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "example.com");
        root_index.bloom_filter = bf_manifest.clone();
        root_index.shards[42].blob_digest = shard_digest.clone();
        root_index.shards[42].entry_count = 1;
        root_index.shards[42].merkle_hash = shard_payload.compute_merkle_hash();
        root_index.recalculate_merkle_root();

        let manifest_digest = client
            .push_sharded_root_index(
                "cache-index-x86_64-linux",
                &root_index,
                &bf_manifest.blob_digest,
                bf_manifest.compressed_size,
                None,
            )
            .await
            .expect("Push sharded root index should succeed");
        assert!(manifest_digest.starts_with("sha256:"));

        let (fetched_root, fetched_digest) = client
            .get_sharded_root_index("cache-index", &SystemArch::X86_64Linux)
            .await
            .expect("Get sharded root index should succeed")
            .expect("Sharded root index should exist");

        assert_eq!(fetched_digest, manifest_digest);
        assert_eq!(fetched_root.system, SystemArch::X86_64Linux);
        assert_eq!(fetched_root.shards[42].blob_digest, shard_digest);
        assert_eq!(fetched_root.total_entries(), 1);
    }

    #[tokio::test]
    async fn test_image_index_serialization_and_routing() {
        let desc_x86 = OciDescriptor {
            media_type: super::OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .to_string(),
            size: 1024,
            platform: Some(OciPlatform::from_system(&SystemArch::X86_64Linux)),
            annotations: Some(HashMap::from([(
                "org.nixos.nixcache.system".to_string(),
                "x86_64-linux".to_string(),
            )])),
        };

        let desc_arm = OciDescriptor {
            media_type: super::OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
            digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                .to_string(),
            size: 1024,
            platform: Some(OciPlatform::from_system(&SystemArch::Aarch64Linux)),
            annotations: Some(HashMap::from([(
                "org.nixos.nixcache.system".to_string(),
                "aarch64-linux".to_string(),
            )])),
        };

        let index = build_image_index(vec![desc_x86, desc_arm], "NixCache Multi-Arch Index");
        let json_str = index
            .to_json_string()
            .expect("Serialization should succeed");
        let parsed: OciImageIndex = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.media_type, super::OCI_IMAGE_INDEX_MEDIA_TYPE);
        assert_eq!(parsed.manifests.len(), 2);

        let found_x86 = parsed.find_manifest_for_system(&SystemArch::X86_64Linux);
        assert!(found_x86.is_some());
        assert_eq!(
            found_x86.unwrap().digest,
            "sha256:1111111111111111111111111111111111111111111111111111111111111111"
        );

        let found_darwin = parsed.find_manifest_for_system(&SystemArch::Aarch64Darwin);
        assert!(found_darwin.is_none());
    }

    #[tokio::test]
    async fn test_hashing_stream_computation_and_byte_counting() {
        let chunk1 = Bytes::from_static(b"Hello, ");
        let chunk2 = Bytes::from_static(b"High-Performance ");
        let chunk3 = Bytes::from_static(b"Streaming Pipeline!");

        let input_stream = futures_util::stream::iter(vec![
            Ok::<Bytes, TransportError>(chunk1.clone()),
            Ok::<Bytes, TransportError>(chunk2.clone()),
            Ok::<Bytes, TransportError>(chunk3.clone()),
        ]);

        let (mut hashing_stream, hash_state) = HashingStream::new(input_stream);

        let mut collected = Vec::new();
        while let Some(item) = hashing_stream.next().await {
            collected.extend_from_slice(&item.unwrap());
        }

        assert_eq!(
            collected,
            b"Hello, High-Performance Streaming Pipeline!".to_vec()
        );
        assert_eq!(hash_state.bytes_streamed(), collected.len() as u64);

        let digest = hash_state
            .digest()
            .expect("Digest should be finalized at EOF");
        let mut full_hasher = Sha256::new();
        full_hasher.update(&collected);
        let expected_digest = format!(
            "sha256:{}",
            full_hasher
                .finalize()
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        );
        assert_eq!(digest, expected_digest);
    }

    #[test]
    fn test_parse_range_header_formats() {
        assert_eq!(parse_range_header("0-100"), Some((0, 100)));
        assert_eq!(parse_range_header("bytes=0-100"), Some((0, 100)));
        assert_eq!(parse_range_header("bytes 0-100"), Some((0, 100)));
        assert_eq!(parse_range_header("bytes 0-100/500"), Some((0, 100)));
        assert_eq!(
            parse_range_header("bytes=1048576-2097151"),
            Some((1048576, 2097151))
        );
        assert_eq!(parse_range_header("invalid"), None);
    }

    #[tokio::test]
    async fn test_monolithic_post_1rtt_success() {
        let transport = MockRouterTransport::default();
        let data = Bytes::from_static(b"fast monolithic payload");
        let digest = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

        // Docker Hub 支持 PreferMonolithicPost
        let client = OciClient::new(
            "docker.io",
            "test/repo",
            "token123",
            true,
            DockerHubDriver,
            transport,
        );
        let pushed_digest = client
            .push_blob_bytes_with_digest(digest, data)
            .await
            .unwrap();

        assert_eq!(pushed_digest, digest);
    }

    #[tokio::test]
    async fn test_ghcr_driver_deterministic_two_step_put_without_patch() {
        let transport = MockRouterTransport::default();
        // 如果发送 PATCH 请求则返回 416
        transport.add_route(
            "PATCH",
            "/uploads/",
            MockResponse {
                status: StatusCode::RANGE_NOT_SATISFIABLE,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );

        let client = OciClient::new(
            "ghcr.io",
            "test/repo",
            "token123",
            true,
            GhcrDriver,
            transport,
        );

        let data = Bytes::from_static(b"streamed nar xz chunk data for test");
        let input_stream =
            futures_util::stream::iter(vec![Ok::<Bytes, TransportError>(data.clone())]);
        let boxed_stream = Box::pin(input_stream);

        let config = UploadConfig::default();
        let (pushed_digest, total_size) = client
            .push_blob_streaming_resumable(boxed_stream, &config)
            .await
            .expect("GHCR upload must succeed deterministically without ever sending PATCH");

        assert_eq!(total_size, data.len() as u64);
        assert!(pushed_digest.starts_with("sha256:"));
    }

    #[tokio::test]
    async fn test_stream_hash_state_single_arc_inner() {
        let state = StreamHashState::new();
        assert_eq!(state.bytes_streamed(), 0);
        assert_eq!(state.digest(), None);

        let chunk1 = Bytes::from_static(b"chunk1 ");
        let chunk2 = Bytes::from_static(b"chunk2");
        let input_stream = futures_util::stream::iter(vec![
            Ok::<Bytes, TransportError>(chunk1),
            Ok::<Bytes, TransportError>(chunk2),
        ]);

        let (mut stream, stream_state) = HashingStream::new(input_stream);
        let state_clone = stream_state.clone();

        while let Some(item) = stream.next().await {
            assert!(item.is_ok());
        }

        assert_eq!(stream_state.bytes_streamed(), 13);
        assert_eq!(state_clone.bytes_streamed(), 13);
        assert!(stream_state.digest().is_some());
        assert_eq!(stream_state.digest(), state_clone.digest());
    }

    #[tokio::test]
    async fn test_push_blob_streaming_resumable_chunked_bytesmut() {
        let transport = MockRouterTransport::default();
        let client = OciClient::new(
            "generic.registry",
            "test/repo",
            "token",
            true,
            GenericOciDriver,
            transport,
        );

        let data = vec![0x42u8; 3 * 1024 * 1024]; // 3MB
        let chunks: Vec<Result<Bytes, TransportError>> = data
            .chunks(512 * 1024)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let input_stream = futures_util::stream::iter(chunks);
        let boxed_stream = Box::pin(input_stream);

        let config = UploadConfig {
            chunk_size_bytes: 1024 * 1024,
            chunk_threshold_bytes: 1024 * 1024,
            max_retry_attempts: 3,
        };

        let (pushed_digest, total_size) = client
            .push_blob_streaming_resumable(boxed_stream, &config)
            .await
            .expect("Resumable chunked upload should succeed");

        assert_eq!(total_size, 3 * 1024 * 1024);
        assert!(pushed_digest.starts_with("sha256:"));
    }

    #[tokio::test]
    async fn test_get_manifest_with_digest_utf8_validation() {
        let transport = MockRouterTransport::default();
        // 1. 合法 UTF-8
        transport.add_route(
            "GET",
            "/manifests/valid-tag",
            MockResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::from_static(b"{\"schemaVersion\":2}"),
            },
        );
        // 2. 非法 UTF-8
        transport.add_route(
            "GET",
            "/manifests/invalid-utf8",
            MockResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::from_static(&[0xFF, 0xFE, 0xFD]),
            },
        );

        let client = OciClient::with_transport("example.com", "test/repo", "", false, transport);

        let valid = client.get_manifest_with_digest("valid-tag").await.unwrap();
        assert!(valid.is_some());
        let (body, digest) = valid.unwrap();
        assert_eq!(body, "{\"schemaVersion\":2}");
        assert!(digest.starts_with("sha256:"));

        let invalid_err = client
            .get_manifest_with_digest("invalid-utf8")
            .await
            .unwrap_err();
        assert!(matches!(invalid_err, OciError::InvalidUtf8Manifest(_)));
    }

    #[tokio::test]
    async fn test_delta_patch_manifest_roundtrip_mock() {
        let transport = MockRouterTransport::default();
        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);

        let mut delta = super::DeltaPatchData::new(8888, "build-job", SystemArch::Aarch64Linux);
        let h1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        delta.new_entries.insert(h1.clone(), IndexEntry::default());
        delta.active_gc_roots.push(h1.clone());

        let manifest_digest = client
            .push_delta_patch_manifest("run-8888-aarch64-linux", &delta, None)
            .await
            .expect("Push delta patch manifest should succeed");
        assert!(manifest_digest.starts_with("sha256:"));

        let (fetched_delta, fetched_digest) = client
            .get_delta_patch_manifest("run-8888-aarch64-linux")
            .await
            .expect("Get delta patch manifest should succeed")
            .expect("Delta patch should exist");

        assert_eq!(fetched_digest, manifest_digest);
        assert_eq!(fetched_delta.run_id, 8888);
        assert_eq!(fetched_delta.job_id, "build-job");
        assert_eq!(fetched_delta.system, SystemArch::Aarch64Linux);
        assert_eq!(fetched_delta.new_entries.len(), 1);
        assert_eq!(fetched_delta.active_gc_roots, vec![h1]);
    }

    #[tokio::test]
    async fn test_update_sharded_arch_index_cas_mock() {
        let transport = MockRouterTransport::default();
        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);

        let h1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let mut filter = FastBlockedBloomFilter::new_with_defaults(10);
        filter.insert(&h1);
        let bf_manifest = client.push_bloom_filter(&filter).await.unwrap();

        let res = client
            .update_sharded_arch_index_cas(
                "cache-index-x86_64-linux",
                &SystemArch::X86_64Linux,
                3,
                |existing| {
                    let mut root = existing.unwrap_or_else(|| {
                        ShardedArchCacheIndexData::new(
                            SystemArch::X86_64Linux,
                            "test/repo",
                            "example.com",
                        )
                    });
                    root.bloom_filter = bf_manifest.clone();
                    root.shards[42].entry_count = 5;
                    root.recalculate_merkle_root();
                    Ok((
                        root,
                        bf_manifest.blob_digest.clone(),
                        bf_manifest.compressed_size,
                    ))
                },
            )
            .await;

        assert!(res.is_ok());

        let (fetched_root, _) = client
            .get_sharded_root_index("cache-index", &SystemArch::X86_64Linux)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched_root.shards[42].entry_count, 5);
    }

    #[tokio::test]
    async fn test_sharded_root_index_multi_arch_index_routing_mock() {
        let transport = MockRouterTransport::default();
        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);

        // 1. x86_64-linux root
        let mut filter_x86 = FastBlockedBloomFilter::new_with_defaults(10);
        let h_x86 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        filter_x86.insert(&h_x86);
        let bf_x86 = client.push_bloom_filter(&filter_x86).await.unwrap();

        let mut root_x86 =
            ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "example.com");
        root_x86.bloom_filter = bf_x86.clone();
        let digest_x86 = client
            .push_sharded_root_index(
                "sub-manifest-x86",
                &root_x86,
                &bf_x86.blob_digest,
                bf_x86.compressed_size,
                None,
            )
            .await
            .unwrap();

        // 2. aarch64-linux root
        let mut filter_arm = FastBlockedBloomFilter::new_with_defaults(10);
        let h_arm = StoreHash::parse("00000000000000000000000000000001").unwrap();
        filter_arm.insert(&h_arm);
        let bf_arm = client.push_bloom_filter(&filter_arm).await.unwrap();

        let mut root_arm =
            ShardedArchCacheIndexData::new(SystemArch::Aarch64Linux, "test/repo", "example.com");
        root_arm.bloom_filter = bf_arm.clone();
        let digest_arm = client
            .push_sharded_root_index(
                "sub-manifest-arm",
                &root_arm,
                &bf_arm.blob_digest,
                bf_arm.compressed_size,
                None,
            )
            .await
            .unwrap();

        // 3. 构造 top-level Image Index
        let desc_x86 = OciDescriptor {
            media_type: super::OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
            digest: digest_x86.clone(),
            size: 1024,
            platform: Some(OciPlatform::from_system(&SystemArch::X86_64Linux)),
            annotations: Some(HashMap::from([(
                "org.nixos.nixcache.system".to_string(),
                "x86_64-linux".to_string(),
            )])),
        };
        let desc_arm = OciDescriptor {
            media_type: super::OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
            digest: digest_arm.clone(),
            size: 1024,
            platform: Some(OciPlatform::from_system(&SystemArch::Aarch64Linux)),
            annotations: Some(HashMap::from([(
                "org.nixos.nixcache.system".to_string(),
                "aarch64-linux".to_string(),
            )])),
        };

        let index = build_image_index(vec![desc_x86, desc_arm], "Multi-Arch Baseline Index");
        client
            .push_image_index("cache-index", &index)
            .await
            .unwrap();

        // 4. 查询 x86_64-linux
        let (resolved_x86, res_digest_x86) = client
            .get_sharded_root_index("cache-index", &SystemArch::X86_64Linux)
            .await
            .unwrap()
            .expect("Should resolve x86 root");
        assert_eq!(resolved_x86.system, SystemArch::X86_64Linux);
        assert_eq!(res_digest_x86.len(), 71); // sha256:...

        // 5. 查询 aarch64-linux
        let (resolved_arm, _) = client
            .get_sharded_root_index("cache-index", &SystemArch::Aarch64Linux)
            .await
            .unwrap()
            .expect("Should resolve arm root");
        assert_eq!(resolved_arm.system, SystemArch::Aarch64Linux);

        // 6. 查询不存在的 aarch64-darwin
        let resolved_darwin = client
            .get_sharded_root_index("cache-index", &SystemArch::Aarch64Darwin)
            .await
            .unwrap();
        assert!(resolved_darwin.is_none());
    }
}
