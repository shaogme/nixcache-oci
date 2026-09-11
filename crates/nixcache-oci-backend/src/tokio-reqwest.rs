use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE},
};
use nixcache_oci::{
    BlobUploadStrategy, OciClient, OciDriver, OciError, OciTransport, RegistryCredentials,
    RegistryKind, TransportError, UploadChunkResponse, UploadConfig, parse_range_header,
    parse_www_authenticate,
};
use reqwest::Client;
use std::{
    io::{Error as IoError, SeekFrom},
    path::Path,
    time::Duration,
};
use tokio::{
    fs::{File, metadata, read},
    io::{AsyncReadExt, AsyncSeekExt},
    time::sleep,
};
use tracing::{info, warn};

fn map_reqwest_error(err: reqwest::Error) -> TransportError {
    if err.is_timeout() {
        TransportError::Timeout {
            duration: Duration::from_secs(0),
        }
    } else if let Some(status) = err.status() {
        TransportError::HttpStatus {
            status,
            message: Some(err.to_string()),
        }
    } else if err.is_builder() || err.is_redirect() {
        TransportError::InvalidUri {
            url: err.url().map(|u| u.to_string()).unwrap_or_default(),
            reason: "Reqwest builder or redirect error",
        }
    } else {
        let endpoint = err
            .url()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        TransportError::ConnectionFailed {
            endpoint,
            source: IoError::other(err.to_string()),
        }
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

async fn challenge_retry_headers<T: OciTransport + Clone>(
    client: &OciClient<T>,
    operation: &'static str,
    response_headers: &HeaderMap,
) -> Result<HeaderMap, OciError> {
    let challenge = parse_www_authenticate(response_headers)?.ok_or_else(|| {
        OciError::AuthenticationFailed {
            operation,
            status: StatusCode::UNAUTHORIZED,
            details: "registry returned 401 without a Bearer challenge".to_string(),
        }
    })?;
    let token = client
        .token_manager()
        .get_token_for_challenge(client.transport(), &challenge, true)
        .await?;
    let mut headers = client.get_auth_headers().await?;
    let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        OciError::AuthChallengeInvalid {
            details: "invalid Bearer authentication header".to_string(),
        }
    })?;
    headers.insert("Authorization", value);
    Ok(headers)
}

fn merge_request_headers(base: &mut HeaderMap, original: &HeaderMap) {
    for (name, value) in original {
        if name.as_str() != "authorization" {
            base.insert(name.clone(), value.clone());
        }
    }
}

async fn post_empty_with_auth_retry<T: OciTransport + Clone>(
    client: &OciClient<T>,
    url: &str,
    headers: HeaderMap,
    operation: &'static str,
) -> Result<(StatusCode, HeaderMap), OciError> {
    let first = client.transport().post(url, headers.clone()).await?;
    if first.0 != StatusCode::UNAUTHORIZED {
        return Ok(first);
    }
    let mut retry_headers = challenge_retry_headers(client, operation, &first.1).await?;
    merge_request_headers(&mut retry_headers, &headers);
    let second = client.transport().post(url, retry_headers).await?;
    if second.0 == StatusCode::UNAUTHORIZED {
        return Err(OciError::AuthenticationFailed {
            operation,
            status: second.0,
            details: "Bearer challenge retry was rejected".to_string(),
        });
    }
    Ok(second)
}

async fn patch_chunk_with_auth_retry<T: OciTransport + Clone>(
    client: &OciClient<T>,
    url: &str,
    headers: HeaderMap,
    chunk: Bytes,
    byte_range: (u64, u64),
    operation: &'static str,
) -> Result<UploadChunkResponse, OciError> {
    let first = client
        .transport()
        .patch_chunk(url, headers.clone(), chunk.clone(), byte_range)
        .await?;
    if first.status != StatusCode::UNAUTHORIZED {
        return Ok(first);
    }
    let mut retry_headers = challenge_retry_headers(client, operation, &first.headers).await?;
    merge_request_headers(&mut retry_headers, &headers);
    let second = client
        .transport()
        .patch_chunk(url, retry_headers, chunk, byte_range)
        .await?;
    if second.status == StatusCode::UNAUTHORIZED {
        return Err(OciError::AuthenticationFailed {
            operation,
            status: second.status,
            details: "Bearer challenge retry was rejected".to_string(),
        });
    }
    Ok(second)
}

async fn finish_chunk_with_auth_retry<T: OciTransport + Clone>(
    client: &OciClient<T>,
    url: &str,
    headers: HeaderMap,
    operation: &'static str,
) -> Result<StatusCode, OciError> {
    let first = client
        .transport()
        .put_chunk_finish_with_headers(url, headers.clone(), None)
        .await?;
    if first.0 != StatusCode::UNAUTHORIZED {
        return Ok(first.0);
    }
    let mut retry_headers = challenge_retry_headers(client, operation, &first.1).await?;
    merge_request_headers(&mut retry_headers, &headers);
    let second = client
        .transport()
        .put_chunk_finish_with_headers(url, retry_headers, None)
        .await?;
    if second.0 == StatusCode::UNAUTHORIZED {
        return Err(OciError::AuthenticationFailed {
            operation,
            status: second.0,
            details: "Bearer challenge retry was rejected".to_string(),
        });
    }
    Ok(second.0)
}

impl OciTransport for ReqwestTransport {
    type BodyStream = BoxStream<'static, Result<Bytes, TransportError>>;

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        self.head_with_headers(url, headers).await.map(|(s, _)| s)
    }

    async fn head_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let resp = self
            .client
            .head(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
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
        headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<StatusCode, TransportError> {
        self.put_chunk_finish_with_headers(url, headers, final_chunk)
            .await
            .map(|(status, _)| status)
    }

    async fn put_chunk_finish_with_headers(
        &self,
        url: &str,
        mut headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
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
            Ok((resp.status(), resp.headers().clone()))
        } else {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(0u64));
            let resp = self
                .client
                .put(url)
                .headers(headers)
                .send()
                .await
                .map_err(map_reqwest_error)?;
            Ok((resp.status(), resp.headers().clone()))
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

    async fn put_bytes_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let resp = self
            .client
            .put(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn put_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<StatusCode, TransportError> {
        self.put_stream_with_headers(url, headers, stream, content_len)
            .await
            .map(|(status, _)| status)
    }

    async fn put_stream_with_headers(
        &self,
        url: &str,
        mut headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
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
        Ok((resp.status(), resp.headers().clone()))
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

    async fn delete_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let resp = self
            .client
            .delete(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
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

#[allow(async_fn_in_trait)]
pub trait OciClientExt {
    async fn push_blob_file(&self, file_path: &Path) -> Result<String, OciError>;

    async fn push_blob_file_resumable(
        &self,
        file_path: &Path,
        digest: &str,
        config: &UploadConfig,
    ) -> Result<String, OciError>;
}

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
        let (status, resp_headers) =
            post_empty_with_auth_retry(self, &upload_init_url, headers, "initialize file upload")
                .await?;
        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }

        let location = resp_headers
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .ok_or(OciError::UploadLocationMissing)?;

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
            let mut last_err: Option<OciError> = None;

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
                        last_err = Some(e);
                        continue;
                    }
                };

                match patch_chunk_with_auth_retry(
                    self,
                    &session_url,
                    headers,
                    Bytes::from(buf),
                    (current_offset, end_offset),
                    "upload file chunk",
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
                        last_err = Some(OciError::BlobUploadFailed(resp.status));
                    }
                    Err(e) => {
                        last_err = Some(e);
                    }
                }

                warn!(
                    "Chunk upload [{}-{}] failed on attempt {}/{}: {:?}. Probing range...",
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
                    source: Box::new(last_err.unwrap_or(OciError::BlobUploadFailed(
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ))),
                });
            }
        }

        let separator = if session_url.contains('?') { "&" } else { "?" };
        let finish_url = format!("{}{}digest={}", session_url, separator, digest);
        let headers = self.get_auth_headers().await?;
        let finish_status =
            finish_chunk_with_auth_retry(self, &finish_url, headers, "finish file upload").await?;

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
    credentials: impl Into<RegistryCredentials>,
    write_access: bool,
) -> OciClient<ReqwestTransport> {
    let transport = ReqwestTransport::default();
    OciClient::with_transport(registry, repo, credentials, write_access, transport)
}

/// 基于指定 Driver 创建 Tokio Reqwest OCI 客户端
pub fn create_tokio_reqwest_client_with_driver(
    registry: &str,
    repo: &str,
    credentials: impl Into<RegistryCredentials>,
    write_access: bool,
    driver: impl Into<OciDriver>,
) -> OciClient<ReqwestTransport> {
    let transport = ReqwestTransport::default();
    OciClient::new(registry, repo, credentials, write_access, driver, transport)
}

/// 基于指定 RegistryKind 创建 Tokio Reqwest OCI 客户端
pub fn create_tokio_reqwest_client_from_kind(
    kind: RegistryKind,
    registry: &str,
    repo: &str,
    credentials: impl Into<RegistryCredentials>,
    write_access: bool,
) -> OciClient<ReqwestTransport> {
    let transport = ReqwestTransport::default();
    OciClient::from_kind(kind, registry, repo, credentials, write_access, transport)
}

#[cfg(test)]
mod tests {
    use super::{
        OciClientExt, create_tokio_reqwest_client, create_tokio_reqwest_client_with_driver,
    };
    use nixcache_oci::{
        BearerChallenge, GenericOciDriver, GhcrDriver, RegistryCredentials, UploadConfig,
    };
    use serde_json::json;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };

    #[tokio::test]
    async fn test_bearer_challenge_uses_registry_realm_and_replays_request() {
        let server = MockServer::start().await;
        let host = server.address().to_string();
        let realm = format!("http://{host}/auth/exchange?existing=1");
        let challenge = format!(
            "Bearer realm=\"{realm}\", service=\"{host}\", scope=\"repository:test/repo/nix-cache:pull\""
        );

        Mock::given(method("GET"))
            .and(path("/v2/test/repo/nix-cache/manifests/cache-index"))
            .respond_with(ResponseTemplate::new(401).insert_header("WWW-Authenticate", challenge))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/auth/exchange"))
            .and(query_param("existing", "1"))
            .and(query_param("service", &host))
            .and(query_param("scope", "repository:test/repo/nix-cache:pull"))
            .and(header("Authorization", "Basic Y3VzdG9tOnNlY3JldA=="))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "realm-token", "expires_in": 120})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/test/repo/nix-cache/manifests/cache-index"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","size":2},"layers":[]}"#,
            ))
            .mount(&server)
            .await;

        let credentials = nixcache_oci::RegistryCredentials::with_username("custom", "secret");
        let client = super::create_tokio_reqwest_client(&host, "test/repo", credentials, false);
        let artifact = client.get_manifest("cache-index").await.unwrap();
        assert!(artifact.is_some());
    }

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

        let credentials = RegistryCredentials::with_username("custom", "secret-gh-token");
        let client = create_tokio_reqwest_client(&host, "test/repo", credentials, true);
        let challenge = BearerChallenge::new(
            format!("http://{host}/token"),
            Some(host.clone()),
            Some("repository:test/repo/nix-cache:pull,push".to_string()),
        )
        .unwrap();
        let token = client
            .get_token(&challenge)
            .await
            .expect("Failed to fetch token");
        assert_eq!(token.as_ref(), "mocked-jwt-token");

        let cached_token = client
            .get_token(&challenge)
            .await
            .expect("Failed to get cached token");
        assert_eq!(cached_token.as_ref(), "mocked-jwt-token");
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

        let client =
            create_tokio_reqwest_client_with_driver(&host, "test/repo", "", true, GenericOciDriver);
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
        let client =
            create_tokio_reqwest_client_with_driver(&host, "test/repo", "", true, GhcrDriver);
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
