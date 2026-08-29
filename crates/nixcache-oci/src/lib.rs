pub mod client;
pub mod error;
pub mod manifest;
pub mod mutation;
pub mod token;
pub mod transport;

pub use client::OciClient;
pub use error::{OciError, TransportError};
pub use manifest::{
    NIX_CACHE_INDEX_MEDIA_TYPE, NIX_CACHE_SESSION_MEDIA_TYPE, OCI_IMAGE_CONFIG_MEDIA_TYPE,
    OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciDescriptor, OciImageManifest, build_index_manifest,
    build_index_oci_manifest, build_session_manifest, build_session_oci_manifest,
};
pub use mutation::SessionMutationRequest;
pub use nixcache_core::{
    BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, JobSummaryMetadata,
    NarDigest, NarInfo, NarInfoMeta, RECEIPT_VERSION, RUN_SESSION_VERSION, RunSessionManifest,
    SCHEMA_VERSION, StoreHash, SystemArch, build_nar_lookup_map, evaluate_multi_arch_gc,
    extract_nar_basename, extract_store_hash, extract_store_hash_str,
};
pub use token::TokenManager;
pub use transport::{BoxBodyStream, OciBlobStream, OciTransport, ReqwestTransport};

#[cfg(test)]
mod tests {
    use super::{
        IndexEntry, NarDigest, NarInfoMeta, OciClient, OciError, ReqwestTransport,
        SessionMutationRequest, StoreHash, SystemArch, TokenManager, TransportError,
    };
    use async_trait::async_trait;
    use bytes::Bytes;
    use http::{HeaderMap, StatusCode};
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
            .and(path("/v2/test/repo/nix-cache/manifests/run-12345"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/v2/test/repo/nix-cache/manifests/run-12345"))
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
}
