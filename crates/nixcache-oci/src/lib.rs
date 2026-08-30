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
    GenericOciDriver, GhcrDriver, OciBackendDriver, OciDriver, RegistryCapabilities, RegistryKind,
    detect_driver, driver_for_kind,
};
pub use client::{FetchedOciArtifact, OciClient};
pub use codec::{DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec};
pub use error::{OciError, TransportError};
pub use manifest::{
    CacheLayerMediaType, EMPTY_CONFIG_DIGEST, EMPTY_CONFIG_SIZE, OCI_IMAGE_CONFIG_MEDIA_TYPE,
    OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest, OciDescriptor,
    OciImageIndex, OciImageManifest, OciPlatform, build_arch_index_manifest,
    build_arch_session_manifest, build_image_index,
};
pub use mock::{MockResponse, MockRouterTransport};
pub use mutation::SessionMutationRequest;
pub use nixcache_core::{
    ArchCacheIndexData, ArchRunSessionManifest, BuildReceipt, BuildStats, CACHE_INDEX_VERSION,
    CacheIndexData, IndexEntry, JobSummaryMetadata, NarDigest, NarInfo, NarInfoMeta,
    RECEIPT_VERSION, RUN_SESSION_VERSION, RunSessionManifest, SCHEMA_VERSION, StoreHash,
    SystemArch, build_nar_lookup_map, evaluate_multi_arch_gc, extract_nar_basename,
    extract_store_hash, extract_store_hash_str,
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
        AwsEcrDriver, BlobUploadStrategy, DockerHubDriver, EMPTY_CONFIG_DIGEST, GenericOciDriver,
        GhcrDriver, HashingStream, IndexEntry, MockResponse, MockRouterTransport, NarDigest,
        NarInfoMeta, OciClient, OciDescriptor, OciError, OciImageIndex, OciPlatform, RegistryKind,
        SessionMutationRequest, StoreHash, StreamHashState, SystemArch, TransportError,
        UploadConfig, build_image_index, parse_range_header,
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
            generic.resolve_token_endpoint("localhost:5000", "myrepo", true),
            "http://localhost:5000/token?service=localhost:5000&scope=repository:myrepo/nix-cache:pull,push"
        );

        let aws = AwsEcrDriver;
        assert_eq!(aws.kind(), RegistryKind::AwsEcr);
        assert!(!aws.capabilities().supports_chunked_patch);
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
        assert!(matches!(err, OciError::BlobNotFound(_)));
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
        let err = OciError::AuthFailed;
        assert_eq!(format!("{}", err), "Registry authentication failed");

        let upload_err = OciError::BlobUploadFailed(StatusCode::BAD_REQUEST);
        assert!(format!("{}", upload_err).contains("400"));

        let download_err = OciError::BlobDownloadFailed(StatusCode::INTERNAL_SERVER_ERROR);
        assert!(format!("{}", download_err).contains("500"));

        let not_found_err = OciError::BlobNotFound("sha256:123".to_string());
        assert_eq!(format!("{}", not_found_err), "Blob not found: sha256:123");

        let manifest_err = OciError::ManifestPushFailed(StatusCode::INTERNAL_SERVER_ERROR);
        assert!(format!("{}", manifest_err).contains("500"));

        let custom_err = OciError::Other("custom failure".to_string());
        assert_eq!(format!("{}", custom_err), "Other error: custom failure");

        let url_err = OciError::InvalidUrl("invalid".to_string());
        assert_eq!(format!("{}", url_err), "Invalid URL: invalid");

        let cas_err = OciError::CasConflict("run-123".to_string());
        assert_eq!(
            format!("{}", cas_err),
            "CAS optimistic concurrency conflict on tag run-123"
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

        assert!(matches!(err, OciError::CasConflict(t) if t == "run-123"));
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

        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);
        assert!(client.delete_manifest("run-old").await.unwrap());
        assert!(!client.delete_manifest("run-missing").await.unwrap());
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

        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);
        assert!(client.delete_blob("sha256:blob1").await.unwrap());
        assert!(!client.delete_blob("sha256:blob2").await.unwrap());
        assert!(!client.delete_blob("sha256:blob3").await.unwrap());

        let digests = vec![
            NarDigest::new_unchecked("sha256:blob1"),
            NarDigest::new_unchecked("sha256:blob2"),
            NarDigest::new_unchecked("sha256:blob3"),
        ];
        let (deleted, skipped) = client.batch_delete_blobs(&digests, 2).await.unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(skipped, 2);
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
        assert!(format!("{}", invalid_err).contains("Invalid UTF-8 manifest"));
    }
}
