pub mod client;
pub mod codec;
pub mod error;
pub mod manifest;
pub mod mutation;
pub mod token;
pub mod transport;

pub use client::{FetchedOciArtifact, OciClient, UploadConfig, UploadStrategy};
pub use codec::{DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec};
pub use error::{OciError, TransportError};
pub use manifest::{
    CacheLayerMediaType, OCI_IMAGE_CONFIG_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE,
    OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciDescriptor, OciImageIndex, OciImageManifest, OciPlatform,
    build_arch_index_manifest, build_arch_session_manifest, build_image_index,
};
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

#[cfg(feature = "reqwest")]
pub use transport::ReqwestTransport;

#[cfg(test)]
mod tests {
    use super::{
        IndexEntry, NarDigest, NarInfoMeta, OciClient, OciError, ReqwestTransport,
        SessionMutationRequest, StoreHash, SystemArch, TokenManager, TransportError,
    };

    use async_trait::async_trait;
    use bytes::Bytes;
    use http::{HeaderMap, StatusCode};
    use sha2::{Digest, Sha256};
    use std::{
        collections::HashMap,
        io::Write,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tempfile::NamedTempFile;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    #[tokio::test]
    async fn test_oci_client_token_exchange_mock() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param(
                "scope",
                "repository:test/repo/nix-cache:pull,push",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "token": "mocked-jwt-token" })),
            )
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "secret-gh-token", true);
        let token = client.get_token().await.expect("Failed to fetch token");
        assert_eq!(token, "mocked-jwt-token");

        // 验证缓存命中，第二次直接从内存返回
        let cached_token = client
            .get_token()
            .await
            .expect("Failed to get cached token");
        assert_eq!(cached_token, "mocked-jwt-token");
    }

    #[tokio::test]
    async fn test_token_manager_singleflight_concurrency() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(100))
                    .set_body_json(serde_json::json!({ "token": "singleflight-jwt-token" })),
            )
            .expect(1) // 期望并发 10 个请求最终只发起 1 次 HTTP 网络调用！
            .mount(&server)
            .await;

        let transport = ReqwestTransport::default();
        let token_manager = Arc::new(TokenManager::new(
            &host,
            "test/repo",
            "secret-gh-token",
            true,
        ));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let tm = token_manager.clone();
            let tr = transport.clone();
            handles.push(tokio::spawn(async move { tm.get_token(&tr).await }));
        }

        for handle in handles {
            let res = handle.await.unwrap().unwrap();
            assert_eq!(res, "singleflight-jwt-token");
        }
    }

    #[tokio::test]
    async fn test_oci_client_token_fallback() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "fallback-token", true);
        let token = client
            .get_token()
            .await
            .expect("Should fallback to github_token");
        assert_eq!(token, "fallback-token");
    }

    #[tokio::test]
    async fn test_head_blob_mock() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("HEAD"))
            .and(path("/v2/test/repo/nix-cache/blobs/sha256:exists"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("HEAD"))
            .and(path("/v2/test/repo/nix-cache/blobs/sha256:missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "", false);
        assert!(client.head_blob("sha256:exists").await.unwrap());
        assert!(!client.head_blob("sha256:missing").await.unwrap());
    }

    #[tokio::test]
    async fn test_get_blob_mock() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("GET"))
            .and(path("/v2/test/repo/nix-cache/blobs/sha256:data123"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello blob content"))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "", false);
        let bytes = client.get_blob("sha256:data123").await.unwrap();
        assert_eq!(&bytes[..], b"hello blob content");

        Mock::given(method("GET"))
            .and(path("/v2/test/repo/nix-cache/blobs/sha256:notfound"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = client.get_blob("sha256:notfound").await.unwrap_err();
        assert!(matches!(err, OciError::BlobNotFound(_)));
    }

    #[tokio::test]
    async fn test_get_and_push_manifest_mock() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        let manifest_content =
            r#"{"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json"}"#;

        Mock::given(method("GET"))
            .and(path("/v2/test/repo/nix-cache/manifests/cache-index"))
            .respond_with(ResponseTemplate::new(200).set_body_string(manifest_content))
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/v2/test/repo/nix-cache/manifests/cache-index"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "", true);
        let fetched = client.get_manifest("cache-index").await.unwrap();
        assert_eq!(fetched, Some(manifest_content.to_string()));

        let push_res = client.push_manifest("cache-index", manifest_content).await;
        assert!(push_res.is_ok());

        Mock::given(method("PUT"))
            .and(path("/v2/test/repo/nix-cache/manifests/fail-tag"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let fail_res = client.push_manifest("fail-tag", manifest_content).await;
        assert!(matches!(
            fail_res,
            Err(OciError::ManifestPushFailed(StatusCode::FORBIDDEN))
        ));
    }

    #[tokio::test]
    async fn test_blob_layer_digest_computation_and_upload() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file
            .write_all(b"test nix nar blob data payload")
            .unwrap();
        let file_path = temp_file.path();

        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .respond_with(ResponseTemplate::new(202).insert_header(
                "Location",
                "/v2/test/repo/nix-cache/blobs/uploads/upload-session-1",
            ))
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path(
                "/v2/test/repo/nix-cache/blobs/uploads/upload-session-1",
            ))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "", true);
        let digest = client.push_blob(file_path).await.unwrap();
        assert!(digest.starts_with("sha256:"));

        // 测试已存在的情况跳过重复上传
        let server2 = MockServer::start().await;
        let host2 = server2.address().to_string();
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server2)
            .await;

        let client2 = OciClient::new(&host2, "test/repo", "", true);
        let digest2 = client2.push_blob(file_path).await.unwrap();
        assert_eq!(digest, digest2);
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
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("PUT"))
            .and(path("/v2/test/repo/nix-cache/manifests/run-123"))
            .respond_with(ResponseTemplate::new(412))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "", true);
        let err = client
            .put_manifest_conditional("run-123", "{}", Some("sha256:old"))
            .await
            .unwrap_err();

        assert!(matches!(err, OciError::CasConflict(t) if t == "run-123"));
    }

    #[tokio::test]
    async fn test_delete_manifest_mock() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("DELETE"))
            .and(path("/v2/test/repo/nix-cache/manifests/run-old"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        Mock::given(method("DELETE"))
            .and(path("/v2/test/repo/nix-cache/manifests/run-missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "", true);
        assert!(client.delete_manifest("run-old").await.unwrap());
        assert!(!client.delete_manifest("run-missing").await.unwrap());
    }

    #[tokio::test]
    async fn test_update_run_session_with_cas_mock() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .respond_with(ResponseTemplate::new(202).insert_header(
                "Location",
                "/v2/test/repo/nix-cache/blobs/uploads/session-upload",
            ))
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/session-upload"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(
                "/v2/test/repo/nix-cache/manifests/run-12345-x86_64-linux",
            ))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path(
                "/v2/test/repo/nix-cache/manifests/run-12345-x86_64-linux",
            ))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "", true);
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
        use super::{OciDescriptor, OciImageIndex, OciPlatform, build_image_index};

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

    #[derive(Clone, Default)]
    struct MockCustomTransport {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl super::OciTransport for MockCustomTransport {
        type BodyStream = super::BoxBodyStream;

        async fn head(
            &self,
            _url: &str,
            _headers: HeaderMap,
        ) -> Result<StatusCode, TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(StatusCode::OK)
        }

        async fn get(
            &self,
            _url: &str,
            _headers: HeaderMap,
        ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok((
                StatusCode::OK,
                HeaderMap::new(),
                Bytes::from_static(b"custom transport data"),
            ))
        }

        async fn stream(
            &self,
            _url: &str,
            _headers: HeaderMap,
        ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let stream = Box::pin(futures_util::stream::once(async {
                Ok(Bytes::from_static(b"stream"))
            }));
            Ok((StatusCode::OK, HeaderMap::new(), stream))
        }

        async fn post(
            &self,
            _url: &str,
            _headers: HeaderMap,
        ) -> Result<(StatusCode, HeaderMap), TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok((StatusCode::ACCEPTED, HeaderMap::new()))
        }

        async fn post_bytes(
            &self,
            _url: &str,
            _headers: HeaderMap,
            _body: Bytes,
        ) -> Result<(StatusCode, HeaderMap), TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok((StatusCode::CREATED, HeaderMap::new()))
        }

        async fn post_stream(
            &self,
            _url: &str,
            _headers: HeaderMap,
            _stream: Self::BodyStream,
            _content_len: u64,
        ) -> Result<(StatusCode, HeaderMap), TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok((StatusCode::CREATED, HeaderMap::new()))
        }

        async fn patch_chunk(
            &self,
            _url: &str,
            _headers: HeaderMap,
            _chunk: Bytes,
            byte_range: (u64, u64),
        ) -> Result<super::UploadChunkResponse, TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(super::UploadChunkResponse {
                status: StatusCode::ACCEPTED,
                headers: HeaderMap::new(),
                location: None,
                range: Some(byte_range),
            })
        }

        async fn patch_chunk_stream(
            &self,
            _url: &str,
            _headers: HeaderMap,
            _stream: Self::BodyStream,
            byte_range: (u64, u64),
        ) -> Result<super::UploadChunkResponse, TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(super::UploadChunkResponse {
                status: StatusCode::ACCEPTED,
                headers: HeaderMap::new(),
                location: None,
                range: Some(byte_range),
            })
        }

        async fn probe_upload_session(
            &self,
            _url: &str,
            _headers: HeaderMap,
        ) -> Result<Option<u64>, TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }

        async fn put_chunk_finish(
            &self,
            _url: &str,
            _headers: HeaderMap,
            _final_chunk: Option<(Bytes, (u64, u64))>,
        ) -> Result<StatusCode, TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(StatusCode::CREATED)
        }

        async fn put_bytes(
            &self,
            _url: &str,
            _headers: HeaderMap,
            _body: Bytes,
        ) -> Result<StatusCode, TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(StatusCode::CREATED)
        }

        async fn put_stream(
            &self,
            _url: &str,
            _headers: HeaderMap,
            _stream: Self::BodyStream,
            _content_len: u64,
        ) -> Result<StatusCode, TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(StatusCode::CREATED)
        }

        async fn delete(
            &self,
            _url: &str,
            _headers: HeaderMap,
        ) -> Result<StatusCode, TransportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(StatusCode::ACCEPTED)
        }

        async fn sleep(&self, _duration: std::time::Duration) {}
    }

    #[tokio::test]
    async fn test_custom_mock_transport() {
        let transport = MockCustomTransport::default();
        let client = OciClient::with_transport(
            "example.com",
            "test/repo",
            "token123",
            false,
            transport.clone(),
        );

        let exists = client.head_blob("sha256:custom").await.unwrap();
        assert!(exists);

        let data = client.get_blob("sha256:custom").await.unwrap();
        assert_eq!(&data[..], b"custom transport data");

        assert!(transport.call_count.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn test_hashing_stream_computation_and_byte_counting() {
        use super::HashingStream;
        use futures_util::StreamExt;
        use sha2::{Digest, Sha256};

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
        use super::parse_range_header;

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

    fn compute_test_digest(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        format!(
            "sha256:{}",
            hasher
                .finalize()
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        )
    }

    #[tokio::test]
    async fn test_monolithic_post_1rtt_success() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        let data = Bytes::from_static(b"fast monolithic payload");
        let digest = compute_test_digest(&data);

        Mock::given(method("HEAD"))
            .and(path(format!("/v2/test/repo/nix-cache/blobs/{}", digest)))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        // 验证 1-RTT POST 直传请求
        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .and(query_param("digest", digest.as_str()))
            .respond_with(
                ResponseTemplate::new(201)
                    .insert_header(
                        "Location",
                        format!("/v2/test/repo/nix-cache/blobs/{}", digest),
                    )
                    .insert_header("Docker-Content-Digest", digest.as_str()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "token123", true);
        let pushed_digest = client
            .push_blob_bytes_with_digest(&digest, data)
            .await
            .unwrap();

        assert_eq!(pushed_digest, digest);
    }

    #[tokio::test]
    async fn test_monolithic_post_fallback_to_twostep() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        let data = Bytes::from_static(b"fallback payload");
        let digest = compute_test_digest(&data);

        Mock::given(method("HEAD"))
            .and(path(format!("/v2/test/repo/nix-cache/blobs/{}", digest)))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        // 1. Monolithic POST 返回 405 Method Not Allowed
        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .and(query_param("digest", digest.as_str()))
            .respond_with(ResponseTemplate::new(405))
            .expect(1)
            .mount(&server)
            .await;

        // 2. 回退到会话创建 POST
        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .respond_with(ResponseTemplate::new(202).insert_header(
                "Location",
                "/v2/test/repo/nix-cache/blobs/uploads/session-fb",
            ))
            .expect(1)
            .mount(&server)
            .await;

        // 3. 两阶段 PUT
        Mock::given(method("PUT"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/session-fb"))
            .and(query_param("digest", digest.as_str()))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "token123", true);
        let pushed = client
            .push_blob_bytes_with_digest(&digest, data)
            .await
            .unwrap();

        assert_eq!(pushed, digest);
    }

    #[tokio::test]
    async fn test_chunked_resumable_upload_with_retry_and_range_probe() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        let chunk1_data = vec![1u8; 1024 * 1024]; // 1MB
        let chunk2_data = vec![2u8; 1024 * 1024]; // 1MB
        let mut full_data = Vec::new();
        full_data.extend_from_slice(&chunk1_data);
        full_data.extend_from_slice(&chunk2_data);

        let digest = compute_test_digest(&full_data);

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&full_data).unwrap();

        Mock::given(method("HEAD"))
            .and(path(format!("/v2/test/repo/nix-cache/blobs/{}", digest)))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        // 1. 初始化上传会话
        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .respond_with(ResponseTemplate::new(202).insert_header(
                "Location",
                "/v2/test/repo/nix-cache/blobs/uploads/resumable-sess",
            ))
            .mount(&server)
            .await;

        // 2. 第 1 块 PATCH (0-1048575)
        Mock::given(method("PATCH"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/resumable-sess"))
            .and(wiremock::matchers::header("Content-Range", "0-1048575"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Range", "0-1048575")
                    .insert_header(
                        "Location",
                        "/v2/test/repo/nix-cache/blobs/uploads/resumable-sess-2",
                    ),
            )
            .mount(&server)
            .await;

        // 3. 第 2 块首次 PATCH (1048576-2097151) 模拟临时失败 (500)
        Mock::given(method("PATCH"))
            .and(path(
                "/v2/test/repo/nix-cache/blobs/uploads/resumable-sess-2",
            ))
            .and(wiremock::matchers::header(
                "Content-Range",
                "1048576-2097151",
            ))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // 4. 探测断点会话状态 GET
        Mock::given(method("GET"))
            .and(path(
                "/v2/test/repo/nix-cache/blobs/uploads/resumable-sess-2",
            ))
            .respond_with(ResponseTemplate::new(200).insert_header("Range", "0-1048575"))
            .mount(&server)
            .await;

        // 5. 重试第 2 块 PATCH 成功
        Mock::given(method("PATCH"))
            .and(path(
                "/v2/test/repo/nix-cache/blobs/uploads/resumable-sess-2",
            ))
            .and(wiremock::matchers::header(
                "Content-Range",
                "1048576-2097151",
            ))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Range", "0-2097151")
                    .insert_header(
                        "Location",
                        "/v2/test/repo/nix-cache/blobs/uploads/resumable-sess-3",
                    ),
            )
            .mount(&server)
            .await;

        // 6. 完成 PUT 提交
        Mock::given(method("PUT"))
            .and(path(
                "/v2/test/repo/nix-cache/blobs/uploads/resumable-sess-3",
            ))
            .and(query_param("digest", digest.as_str()))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "token123", true);
        let config = super::UploadConfig {
            chunk_threshold_bytes: 512 * 1024,
            chunk_size_bytes: 1024 * 1024,
            max_retry_attempts: 3,
            strategy: super::UploadStrategy::ForceChunked,
        };

        let result = client
            .push_blob_file_resumable(temp_file.path(), &digest, &config)
            .await
            .unwrap();

        assert_eq!(result, digest);
    }

    #[tokio::test]
    async fn test_streaming_resumable_upload_pipeline() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        let chunk1_data = Bytes::from_static(b"streaming-chunk-1-");
        let chunk2_data = Bytes::from_static(b"streaming-chunk-2-final");
        let mut full_data = Vec::new();
        full_data.extend_from_slice(&chunk1_data);
        full_data.extend_from_slice(&chunk2_data);

        let digest = compute_test_digest(&full_data);
        let total_size = full_data.len() as u64;

        // 初始化会话
        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .respond_with(ResponseTemplate::new(202).insert_header(
                "Location",
                "/v2/test/repo/nix-cache/blobs/uploads/stream-sess",
            ))
            .mount(&server)
            .await;

        // PATCH 推流分块
        Mock::given(method("PATCH"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/stream-sess"))
            .respond_with(ResponseTemplate::new(202).insert_header(
                "Location",
                "/v2/test/repo/nix-cache/blobs/uploads/stream-sess-fin",
            ))
            .mount(&server)
            .await;

        // 提交最终 PUT
        Mock::given(method("PUT"))
            .and(path(
                "/v2/test/repo/nix-cache/blobs/uploads/stream-sess-fin",
            ))
            .and(query_param("digest", digest.as_str()))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let client = OciClient::new(&host, "test/repo", "token123", true);
        let stream = Box::pin(futures_util::stream::iter(vec![
            Ok(chunk1_data),
            Ok(chunk2_data),
        ]));

        let config = super::UploadConfig {
            chunk_threshold_bytes: 10,
            chunk_size_bytes: 1024 * 1024,
            max_retry_attempts: 3,
            strategy: super::UploadStrategy::Auto,
        };

        let (res_digest, res_size) = client
            .push_blob_streaming_resumable(stream, &config)
            .await
            .unwrap();

        assert_eq!(res_digest, digest);
        assert_eq!(res_size, total_size);
    }
}
