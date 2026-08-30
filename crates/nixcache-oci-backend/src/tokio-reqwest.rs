use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE},
};
use nixcache_oci::{
    BlobUploadStrategy, BoxBodyStream, OciBackendDriver, OciClient, OciError, OciTransport,
    RegistryKind, TransportError, UploadChunkResponse, UploadConfig, parse_range_header,
};
use reqwest::Client;
use std::{io::SeekFrom, path::Path, sync::Arc, time::Duration};
use tokio::{
    fs::{File, metadata, read},
    io::{AsyncReadExt, AsyncSeekExt},
    time::sleep,
};
use tracing::{info, warn};

fn map_reqwest_error(err: reqwest::Error) -> TransportError {
    if err.is_builder() || err.is_redirect() {
        TransportError::InvalidUrl(err.to_string())
    } else if err.is_status() {
        TransportError::Http(err.to_string())
    } else {
        TransportError::Network(err.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client }
    }
}

impl ReqwestTransport {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl OciTransport for ReqwestTransport {
    type BodyStream = BoxBodyStream;

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        let resp = self
            .client
            .head(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok(resp.status())
    }

    async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError> {
        let resp = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.map_err(map_reqwest_error)?;
        Ok((status, headers, bytes))
    }

    async fn stream(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError> {
        let resp = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let stream: BoxStream<'static, Result<Bytes, TransportError>> = Box::pin(
            resp.bytes_stream()
                .map(|res| res.map_err(map_reqwest_error)),
        );
        Ok((status, headers, stream))
    }

    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let resp = self
            .client
            .post(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn post_bytes(
        &self,
        url: &str,
        mut headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(body.len() as u64));
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
        let resp = self
            .client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn post_stream(
        &self,
        url: &str,
        mut headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(content_len));
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
        let body = reqwest::Body::wrap_stream(stream);
        let resp = self
            .client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn patch_chunk(
        &self,
        url: &str,
        mut headers: HeaderMap,
        chunk: Bytes,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError> {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(chunk.len() as u64));
        let range_str = format!("{}-{}", byte_range.0, byte_range.1);
        if let Ok(val) = HeaderValue::from_str(&range_str) {
            headers.insert(CONTENT_RANGE, val);
        }
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }

        let resp = self
            .client
            .patch(url)
            .headers(headers)
            .body(chunk)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let location = resp_headers
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let range = resp_headers
            .get(RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range_header);

        Ok(UploadChunkResponse {
            status,
            headers: resp_headers,
            location,
            range,
        })
    }

    async fn patch_chunk_stream(
        &self,
        url: &str,
        mut headers: HeaderMap,
        stream: Self::BodyStream,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError> {
        let chunk_len = byte_range.1.saturating_sub(byte_range.0) + 1;
        headers.insert(CONTENT_LENGTH, HeaderValue::from(chunk_len));
        let range_str = format!("{}-{}", byte_range.0, byte_range.1);
        if let Ok(val) = HeaderValue::from_str(&range_str) {
            headers.insert(CONTENT_RANGE, val);
        }
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }

        let body = reqwest::Body::wrap_stream(stream);
        let resp = self
            .client
            .patch(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let location = resp_headers
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let range = resp_headers
            .get(RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range_header);

        Ok(UploadChunkResponse {
            status,
            headers: resp_headers,
            location,
            range,
        })
    }

    async fn probe_upload_session(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<Option<u64>, TransportError> {
        let resp = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let headers = resp.headers();

        if let Some(range_val) = headers.get(RANGE).and_then(|v| v.to_str().ok())
            && let Some((_start, end)) = parse_range_header(range_val)
        {
            return Ok(Some(end));
        }
        Ok(None)
    }

    async fn put_chunk_finish(
        &self,
        url: &str,
        mut headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<StatusCode, TransportError> {
        if let Some((bytes, byte_range)) = final_chunk {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(bytes.len() as u64));
            let range_str = format!("{}-{}", byte_range.0, byte_range.1);
            if let Ok(val) = HeaderValue::from_str(&range_str) {
                headers.insert(CONTENT_RANGE, val);
            }
            if !headers.contains_key(CONTENT_TYPE) {
                headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
            }
            let resp = self
                .client
                .put(url)
                .headers(headers)
                .body(bytes)
                .send()
                .await
                .map_err(map_reqwest_error)?;
            Ok(resp.status())
        } else {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(0u64));
            let resp = self
                .client
                .put(url)
                .headers(headers)
                .send()
                .await
                .map_err(map_reqwest_error)?;
            Ok(resp.status())
        }
    }

    async fn put_bytes(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<StatusCode, TransportError> {
        let resp = self
            .client
            .put(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok(resp.status())
    }

    async fn put_stream(
        &self,
        url: &str,
        mut headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<StatusCode, TransportError> {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(content_len));
        let body = reqwest::Body::wrap_stream(stream);
        let resp = self
            .client
            .put(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok(resp.status())
    }

    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        let resp = self
            .client
            .delete(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok(resp.status())
    }

    async fn sleep(&self, duration: Duration) {
        sleep(duration).await;
    }
}

use sha2::{Digest, Sha256};

fn compute_sha256_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    format!(
        "sha256:{}",
        hash.iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    )
}

#[async_trait]
pub trait OciClientExt {
    async fn push_blob_file(&self, file_path: &Path) -> Result<String, OciError>;

    async fn push_blob_file_resumable(
        &self,
        file_path: &Path,
        digest: &str,
        config: &UploadConfig,
    ) -> Result<String, OciError>;
}

#[async_trait]
impl OciClientExt for OciClient<ReqwestTransport> {
    async fn push_blob_file(&self, file_path: &Path) -> Result<String, OciError> {
        let data = read(file_path).await?;
        let digest = compute_sha256_digest(&data);
        self.push_blob_file_resumable(file_path, &digest, &UploadConfig::default())
            .await
    }

    async fn push_blob_file_resumable(
        &self,
        file_path: &Path,
        digest: &str,
        config: &UploadConfig,
    ) -> Result<String, OciError> {
        if self.head_blob(digest).await? {
            info!("Blob {} already exists, skipping upload.", digest);
            return Ok(digest.to_string());
        }

        let file_meta = metadata(file_path).await?;
        let file_size = file_meta.len();

        let capabilities = self.driver().capabilities();
        let strategy = capabilities.fixed_upload_strategy;

        let allow_chunked = capabilities.supports_chunked_patch
            && strategy == BlobUploadStrategy::ResumableChunkedPatch;

        // 若当前后端不支持分块或文件小于阈值，确定性直传（两阶段 PUT 或单阶段 POST）
        if !allow_chunked || file_size < config.chunk_threshold_bytes {
            let data = read(file_path).await?;
            return self
                .push_blob_bytes_with_digest(digest, Bytes::from(data))
                .await;
        }

        info!(
            "Initiating standard chunked upload for blob {} (size: {} bytes, chunk: {} bytes)",
            digest, file_size, config.chunk_size_bytes
        );

        let upload_init_url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/uploads/",
            self.url_scheme(),
            self.registry(),
            self.repo()
        );

        let headers = self.get_auth_headers().await?;
        let (status, resp_headers) = self.transport().post(&upload_init_url, headers).await?;
        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }

        let location = resp_headers
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| OciError::Other("Location header missing".to_string()))?;

        let mut session_url = if location.starts_with('/') {
            format!("{}://{}{}", self.url_scheme(), self.registry(), location)
        } else {
            location.to_string()
        };

        let mut file = File::open(file_path).await?;
        let mut current_offset = 0u64;
        let chunk_size = config.chunk_size_bytes.max(1024 * 1024) as u64;

        while current_offset < file_size {
            let end_offset = (current_offset + chunk_size).min(file_size) - 1;
            let block_len = (end_offset - current_offset + 1) as usize;

            let mut attempts = 0;
            let mut chunk_succeeded = false;
            let mut last_err = String::new();

            while attempts < config.max_retry_attempts {
                attempts += 1;
                if let Err(e) = file.seek(SeekFrom::Start(current_offset)).await {
                    return Err(OciError::Io(e));
                }

                let mut buf = vec![0u8; block_len];
                if let Err(e) = file.read_exact(&mut buf).await {
                    return Err(OciError::Io(e));
                }

                let headers = match self.get_auth_headers().await {
                    Ok(h) => h,
                    Err(e) => {
                        last_err = e.to_string();
                        continue;
                    }
                };

                match self
                    .transport()
                    .patch_chunk(
                        &session_url,
                        headers,
                        Bytes::from(buf),
                        (current_offset, end_offset),
                    )
                    .await
                {
                    Ok(resp)
                        if resp.status == StatusCode::ACCEPTED
                            || resp.status == StatusCode::OK
                            || resp.status == StatusCode::NO_CONTENT =>
                    {
                        if let Some(new_loc) = resp.location {
                            session_url = if new_loc.starts_with('/') {
                                format!("{}://{}{}", self.url_scheme(), self.registry(), new_loc)
                            } else {
                                new_loc
                            };
                        }
                        current_offset = end_offset + 1;
                        chunk_succeeded = true;
                        break;
                    }
                    Ok(resp) => {
                        last_err = format!("Server returned unexpected status {}", resp.status);
                    }
                    Err(e) => {
                        last_err = e.to_string();
                    }
                }

                warn!(
                    "Chunk upload [{}-{}] failed on attempt {}/{}: {}. Probing range...",
                    current_offset, end_offset, attempts, config.max_retry_attempts, last_err
                );

                let backoff_ms = 100 * (1 << attempts.min(5));
                self.transport()
                    .sleep(Duration::from_millis(backoff_ms))
                    .await;

                if let Ok(probe_headers) = self.get_auth_headers().await
                    && let Ok(Some(last_byte)) = self
                        .transport()
                        .probe_upload_session(&session_url, probe_headers)
                        .await
                    && last_byte + 1 > current_offset
                {
                    info!(
                        "Range probe adjusted current offset from {} to {}",
                        current_offset,
                        last_byte + 1
                    );
                    current_offset = last_byte + 1;
                    if current_offset > end_offset {
                        chunk_succeeded = true;
                        break;
                    }
                }
            }

            if !chunk_succeeded {
                return Err(OciError::ResumableUploadFailed {
                    attempts: config.max_retry_attempts,
                    last_error: last_err,
                });
            }
        }

        let separator = if session_url.contains('?') { "&" } else { "?" };
        let finish_url = format!("{}{}digest={}", session_url, separator, digest);
        let headers = self.get_auth_headers().await?;
        let finish_status = self
            .transport()
            .put_chunk_finish(&finish_url, headers, None)
            .await?;

        if finish_status == StatusCode::CREATED
            || finish_status == StatusCode::OK
            || finish_status == StatusCode::ACCEPTED
        {
            info!(
                "Successfully committed resumable upload for blob {}",
                digest
            );
            Ok(digest.to_string())
        } else {
            Err(OciError::BlobUploadFailed(finish_status))
        }
    }
}

/// 自动根据 registry 域名探测驱动并创建 Tokio Reqwest OCI 客户端
pub fn create_tokio_reqwest_client(
    registry: &str,
    repo: &str,
    github_token: &str,
    write_access: bool,
) -> OciClient<ReqwestTransport> {
    let transport = ReqwestTransport::default();
    OciClient::with_transport(registry, repo, github_token, write_access, transport)
}

/// 基于指定 Driver 创建 Tokio Reqwest OCI 客户端
pub fn create_tokio_reqwest_client_with_driver(
    registry: &str,
    repo: &str,
    github_token: &str,
    write_access: bool,
    driver: Arc<dyn OciBackendDriver>,
) -> OciClient<ReqwestTransport> {
    let transport = ReqwestTransport::default();
    OciClient::new(
        registry,
        repo,
        github_token,
        write_access,
        driver,
        transport,
    )
}

/// 基于指定 RegistryKind 创建 Tokio Reqwest OCI 客户端
pub fn create_tokio_reqwest_client_from_kind(
    kind: RegistryKind,
    registry: &str,
    repo: &str,
    github_token: &str,
    write_access: bool,
) -> OciClient<ReqwestTransport> {
    let transport = ReqwestTransport::default();
    OciClient::from_kind(kind, registry, repo, github_token, write_access, transport)
}

#[cfg(test)]
mod tests {
    use super::{
        OciClientExt, create_tokio_reqwest_client, create_tokio_reqwest_client_with_driver,
    };
    use nixcache_oci::{BlobUploadStrategy, GhcrDriver, RegistryCapabilities, UploadConfig};
    use serde_json::json;
    use std::{io::Write, sync::Arc};
    use tempfile::NamedTempFile;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    #[tokio::test]
    async fn test_reqwest_transport_token_exchange_mock() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param(
                "scope",
                "repository:test/repo/nix-cache:pull,push",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "token": "mocked-jwt-token" })),
            )
            .mount(&server)
            .await;

        let client = create_tokio_reqwest_client(&host, "test/repo", "secret-gh-token", true);
        let token = client.get_token().await.expect("Failed to fetch token");
        assert_eq!(token, "mocked-jwt-token");

        let cached_token = client
            .get_token()
            .await
            .expect("Failed to get cached token");
        assert_eq!(cached_token, "mocked-jwt-token");
    }

    #[tokio::test]
    async fn test_oci_client_ext_push_blob_file() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file
            .write_all(b"test nix nar blob file payload")
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
                "/v2/test/repo/nix-cache/blobs/uploads/upload-session-file",
            ))
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path(
                "/v2/test/repo/nix-cache/blobs/uploads/upload-session-file",
            ))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let client = create_tokio_reqwest_client(&host, "test/repo", "", true);
        let digest = client.push_blob_file(file_path).await.unwrap();
        assert!(digest.starts_with("sha256:"));

        // 重复上传应当命中 HEAD 200 直接返回
        let server2 = MockServer::start().await;
        let host2 = server2.address().to_string();
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server2)
            .await;

        let client2 = create_tokio_reqwest_client(&host2, "test/repo", "", true);
        let digest2 = client2.push_blob_file(file_path).await.unwrap();
        assert_eq!(digest, digest2);
    }

    #[tokio::test]
    async fn test_oci_client_ext_push_blob_file_resumable_chunked() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        let mut temp_file = NamedTempFile::new().unwrap();
        let payload = vec![0xABu8; 2 * 1024 * 1024]; // 2MB
        temp_file.write_all(&payload).unwrap();
        let file_path = temp_file.path();

        let digest = super::compute_sha256_digest(&payload);

        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .respond_with(ResponseTemplate::new(202).insert_header(
                "Location",
                "/v2/test/repo/nix-cache/blobs/uploads/chunked-session",
            ))
            .mount(&server)
            .await;

        Mock::given(method("PATCH"))
            .and(path(
                "/v2/test/repo/nix-cache/blobs/uploads/chunked-session",
            ))
            .respond_with(ResponseTemplate::new(202).insert_header("Range", "0-1048575"))
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path(
                "/v2/test/repo/nix-cache/blobs/uploads/chunked-session",
            ))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        // 构造一个配置了 ResumableChunkedPatch 能力的 Mock 驱动
        #[derive(Debug, Clone)]
        struct MockChunkedDriver;
        static MOCK_CHUNKED_CAPS: RegistryCapabilities = RegistryCapabilities {
            supports_chunked_patch: true,
            supports_monolithic_post_1rtt: false,
            supports_manifest_cas_if_match: true,
            requires_library_namespace_expansion: false,
            fixed_upload_strategy: BlobUploadStrategy::ResumableChunkedPatch,
            custom_auth_endpoint: None,
        };
        impl nixcache_oci::OciBackendDriver for MockChunkedDriver {
            fn kind(&self) -> nixcache_oci::RegistryKind {
                nixcache_oci::RegistryKind::GenericOci
            }
            fn capabilities(&self) -> &'static RegistryCapabilities {
                &MOCK_CHUNKED_CAPS
            }
            fn canonicalize_endpoint(&self, r: &str) -> String {
                r.to_string()
            }
            fn canonicalize_repository(&self, r: &str) -> String {
                r.to_string()
            }
            fn format_auth_scope(&self, repo: &str, write: bool) -> String {
                let action = if write { "pull,push" } else { "pull" };
                format!("repository:{}/nix-cache:{}", repo, action)
            }
            fn resolve_token_endpoint(&self, reg: &str, repo: &str, write: bool) -> String {
                let scope = self.format_auth_scope(repo, write);
                format!("http://{}/token?scope={}", reg, scope)
            }
        }

        let client = create_tokio_reqwest_client_with_driver(
            &host,
            "test/repo",
            "",
            true,
            Arc::new(MockChunkedDriver),
        );
        let config = UploadConfig {
            chunk_threshold_bytes: 1024 * 1024,
            chunk_size_bytes: 1024 * 1024,
            max_retry_attempts: 3,
        };

        let res_digest = client
            .push_blob_file_resumable(file_path, &digest, &config)
            .await
            .unwrap();
        assert_eq!(res_digest, digest);
    }

    #[tokio::test]
    async fn test_oci_client_ext_ghcr_driver_deterministic_without_patch() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        let mut temp_file = NamedTempFile::new().unwrap();
        let payload = vec![0xCDu8; 2 * 1024 * 1024]; // 2MB
        temp_file.write_all(&payload).unwrap();
        let file_path = temp_file.path();

        let digest = super::compute_sha256_digest(&payload);

        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .respond_with(ResponseTemplate::new(202).insert_header(
                "Location",
                "/v2/test/repo/nix-cache/blobs/uploads/ghcr-session",
            ))
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/ghcr-session"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        // 如果发送 PATCH，直接返回 416 报错
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(416))
            .mount(&server)
            .await;

        // 使用 GhcrDriver
        let client = create_tokio_reqwest_client_with_driver(
            &host,
            "test/repo",
            "",
            true,
            Arc::new(GhcrDriver),
        );
        let config = UploadConfig {
            chunk_threshold_bytes: 1024 * 1024,
            chunk_size_bytes: 1024 * 1024,
            max_retry_attempts: 3,
        };

        let res_digest = client
            .push_blob_file_resumable(file_path, &digest, &config)
            .await
            .expect(
                "GhcrDriver must succeed deterministically with two-step PUT without sending PATCH",
            );

        assert_eq!(res_digest, digest);
    }
}
