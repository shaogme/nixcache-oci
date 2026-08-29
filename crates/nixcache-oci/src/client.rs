use crate::{
    error::OciError,
    manifest::{
        OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciImageIndex, OciImageManifest,
        build_arch_session_manifest, build_index_manifest,
    },
    mutation::SessionMutationRequest,
    token::TokenManager,
    transport::{HashingStream, OciBlobStream, OciTransport},
};
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode, header::IF_MATCH};
use nixcache_core::{
    ArchCacheIndexData, ArchRunSessionManifest, CACHE_INDEX_VERSION, CacheIndexData,
    RUN_SESSION_VERSION, RunSessionManifest, SystemArch,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tracing::{error, info, warn};

#[cfg(feature = "reqwest")]
use crate::transport::ReqwestTransport;

#[cfg(feature = "tokio-fs")]
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UploadStrategy {
    /// 自动决策：< 64MB 且已知 Digest 时采用 Monolithic 1-RTT，否则启用 Chunked Resumable
    #[default]
    Auto,
    /// 强制单阶段 1-RTT POST 直传
    ForceMonolithic,
    /// 强制两阶段 PUT
    ForceTwoStepPut,
    /// 强制分块断点续传
    ForceChunked,
}

#[derive(Debug, Clone)]
pub struct UploadConfig {
    /// 分块阈值，超过此大小触发分块上传（默认 64MB）
    pub chunk_threshold_bytes: u64,
    /// 单个分块大小（默认 32MB，最小 1MB）
    pub chunk_size_bytes: usize,
    /// 最大网络中断重试次数（默认 5 次）
    pub max_retry_attempts: usize,
    /// 上传策略
    pub strategy: UploadStrategy,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            chunk_threshold_bytes: 64 * 1024 * 1024,
            chunk_size_bytes: 32 * 1024 * 1024,
            max_retry_attempts: 5,
            strategy: UploadStrategy::Auto,
        }
    }
}

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

#[derive(Debug, Clone)]
pub struct FetchedOciArtifact {
    pub index: OciImageIndex,
    pub digest: String,
}

#[cfg(feature = "reqwest")]
#[derive(Clone)]
pub struct OciClient<T: OciTransport = ReqwestTransport> {
    registry: String,
    repo: String,
    token_manager: TokenManager,
    transport: T,
}

#[cfg(not(feature = "reqwest"))]
#[derive(Clone)]
pub struct OciClient<T: OciTransport> {
    registry: String,
    repo: String,
    token_manager: TokenManager,
    transport: T,
}

#[cfg(feature = "reqwest")]
impl OciClient<ReqwestTransport> {
    pub fn new(registry: &str, repo: &str, github_token: &str, write_access: bool) -> Self {
        let transport = ReqwestTransport::default();
        Self::with_transport(registry, repo, github_token, write_access, transport)
    }
}

impl<T: OciTransport> OciClient<T> {
    pub fn with_transport(
        registry: &str,
        repo: &str,
        github_token: &str,
        write_access: bool,
        transport: T,
    ) -> Self {
        let token_manager = TokenManager::new(registry, repo, github_token, write_access);
        Self {
            registry: registry.to_string(),
            repo: repo.to_string(),
            token_manager,
            transport,
        }
    }

    pub fn registry(&self) -> &str {
        &self.registry
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn token_manager(&self) -> &TokenManager {
        &self.token_manager
    }

    fn url_scheme(&self) -> &str {
        if self.registry.starts_with("localhost:")
            || self.registry.starts_with("127.0.0.1:")
            || self.registry == "localhost"
            || self.registry == "127.0.0.1"
        {
            "http"
        } else {
            "https"
        }
    }

    pub async fn get_token(&self) -> Result<String, OciError> {
        self.token_manager.get_token(&self.transport).await
    }

    async fn get_auth_headers(&self) -> Result<HeaderMap, OciError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Accept",
            HeaderValue::from_static(
                "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json",
            ),
        );

        let token = self.get_token().await?;
        if !token.is_empty() {
            let auth_val = format!("Bearer {}", token);
            if let Ok(val) = HeaderValue::from_str(&auth_val) {
                headers.insert("Authorization", val);
            }
        }
        Ok(headers)
    }

    pub async fn head_blob(&self, digest: &str) -> Result<bool, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let headers = self.get_auth_headers().await?;
        let status = self.transport.head(&url, headers).await?;

        if status == StatusCode::OK {
            Ok(true)
        } else if status == StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            warn!(
                "Unexpected status when checking blob {}: HTTP {}",
                digest, status
            );
            Err(OciError::BlobCheckFailed(status))
        }
    }

    pub async fn push_blob_bytes_with_digest(
        &self,
        digest: &str,
        bytes: Bytes,
    ) -> Result<String, OciError> {
        if self.head_blob(digest).await? {
            info!("Blob {} already exists, skipping upload.", digest);
            return Ok(digest.to_string());
        }

        info!("Initiating 1-RTT / session upload for blob {}", digest);

        // 1. 尝试 1-RTT Monolithic POST 直传
        let monolithic_url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/uploads/?digest={}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let mut headers = self.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static("application/octet-stream"),
        );

        match self
            .transport
            .post_bytes(&monolithic_url, headers.clone(), bytes.clone())
            .await
        {
            Ok((status, _resp_headers))
                if status == StatusCode::CREATED || status == StatusCode::OK =>
            {
                info!(
                    "Successfully uploaded blob via 1-RTT Monolithic POST: {}",
                    digest
                );
                return Ok(digest.to_string());
            }
            Ok((status, _)) => {
                warn!(
                    "Monolithic POST returned status {}, falling back to two-step upload for blob {}",
                    status, digest
                );
            }
            Err(e) => {
                warn!(
                    "Monolithic POST failed ({}), falling back to two-step upload for blob {}",
                    e, digest
                );
            }
        }

        // 2. 回退到标准两阶段会话上传
        let upload_init_url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/uploads/",
            self.url_scheme(),
            self.registry,
            self.repo
        );

        let headers = self.get_auth_headers().await?;
        let (status, resp_headers) = self.transport.post(&upload_init_url, headers).await?;

        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }

        let location = resp_headers
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| OciError::Other("Location header missing".to_string()))?;

        let mut put_url = if location.starts_with('/') {
            format!("{}://{}{}", self.url_scheme(), self.registry, location)
        } else {
            location.to_string()
        };

        let separator = if put_url.contains('?') { "&" } else { "?" };
        put_url = format!("{}{}digest={}", put_url, separator, digest);

        let mut headers = self.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static("application/octet-stream"),
        );

        let put_status = self.transport.put_bytes(&put_url, headers, bytes).await?;

        if put_status == StatusCode::CREATED
            || put_status == StatusCode::ACCEPTED
            || put_status == StatusCode::OK
        {
            info!("Successfully uploaded blob via two-step PUT: {}", digest);
            Ok(digest.to_string())
        } else {
            Err(OciError::BlobUploadFailed(put_status))
        }
    }

    pub async fn push_blob_bytes(&self, bytes: Bytes) -> Result<String, OciError> {
        let digest = compute_sha256_digest(&bytes);
        self.push_blob_bytes_with_digest(&digest, bytes).await
    }

    pub async fn push_blob_stream(
        &self,
        digest: &str,
        stream: T::BodyStream,
        content_len: u64,
    ) -> Result<String, OciError> {
        if self.head_blob(digest).await? {
            info!("Blob {} already exists, skipping upload.", digest);
            return Ok(digest.to_string());
        }

        info!("Initiating stream upload for blob {}", digest);
        let upload_init_url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/uploads/",
            self.url_scheme(),
            self.registry,
            self.repo
        );

        let headers = self.get_auth_headers().await?;
        let (status, resp_headers) = self.transport.post(&upload_init_url, headers).await?;

        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }

        let location = resp_headers
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| OciError::Other("Location header missing".to_string()))?;

        let mut put_url = if location.starts_with('/') {
            format!("{}://{}{}", self.url_scheme(), self.registry, location)
        } else {
            location.to_string()
        };

        let separator = if put_url.contains('?') { "&" } else { "?" };
        put_url = format!("{}{}digest={}", put_url, separator, digest);

        let mut headers = self.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static("application/octet-stream"),
        );

        let put_status = self
            .transport
            .put_stream(&put_url, headers, stream, content_len)
            .await?;

        if put_status == StatusCode::CREATED
            || put_status == StatusCode::ACCEPTED
            || put_status == StatusCode::OK
        {
            info!("Successfully uploaded blob stream: {}", digest);
            Ok(digest.to_string())
        } else {
            Err(OciError::BlobUploadFailed(put_status))
        }
    }

    #[cfg(feature = "tokio-fs")]
    pub async fn push_blob_file_resumable(
        &self,
        file_path: &Path,
        digest: &str,
        config: &UploadConfig,
    ) -> Result<String, OciError> {
        if self.head_blob(digest).await? {
            info!("Blob {} already exists, skipping upload.", digest);
            return Ok(digest.to_string());
        }

        let file_meta = tokio::fs::metadata(file_path).await?;
        let file_size = file_meta.len();

        if file_size < config.chunk_threshold_bytes
            && config.strategy != UploadStrategy::ForceChunked
        {
            let data = tokio::fs::read(file_path).await?;
            return self
                .push_blob_bytes_with_digest(digest, Bytes::from(data))
                .await;
        }

        info!(
            "Initiating resumable chunked upload for blob {} (size: {} bytes, chunk: {} bytes)",
            digest, file_size, config.chunk_size_bytes
        );

        let upload_init_url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/uploads/",
            self.url_scheme(),
            self.registry,
            self.repo
        );

        let headers = self.get_auth_headers().await?;
        let (status, resp_headers) = self.transport.post(&upload_init_url, headers).await?;
        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }

        let location = resp_headers
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| OciError::Other("Location header missing".to_string()))?;

        let mut session_url = if location.starts_with('/') {
            format!("{}://{}{}", self.url_scheme(), self.registry, location)
        } else {
            location.to_string()
        };

        let mut file = tokio::fs::File::open(file_path).await?;
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
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                if let Err(e) = file.seek(std::io::SeekFrom::Start(current_offset)).await {
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
                    .transport
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
                                format!("{}://{}{}", self.url_scheme(), self.registry, new_loc)
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
                self.transport
                    .sleep(Duration::from_millis(backoff_ms))
                    .await;

                if let Ok(probe_headers) = self.get_auth_headers().await
                    && let Ok(Some(last_byte)) = self
                        .transport
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
            .transport
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

    pub async fn push_blob_streaming_resumable(
        &self,
        stream: T::BodyStream,
        config: &UploadConfig,
    ) -> Result<(String, u64), OciError> {
        let (hashing_stream, hash_state) = HashingStream::new(stream);
        let mut pinned_stream = std::pin::pin!(hashing_stream);

        info!("Initiating streaming resumable chunked upload");
        let upload_init_url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/uploads/",
            self.url_scheme(),
            self.registry,
            self.repo
        );

        let headers = self.get_auth_headers().await?;
        let (status, resp_headers) = self.transport.post(&upload_init_url, headers).await?;
        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }

        let location = resp_headers
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| OciError::Other("Location header missing".to_string()))?;

        let mut session_url = if location.starts_with('/') {
            format!("{}://{}{}", self.url_scheme(), self.registry, location)
        } else {
            location.to_string()
        };

        let chunk_limit = config.chunk_size_bytes.max(1024 * 1024);
        let mut current_offset = 0u64;
        let mut chunk_buf = Vec::with_capacity(chunk_limit);

        while let Some(item) = pinned_stream.next().await {
            let bytes: Bytes = item?;
            chunk_buf.extend_from_slice(&bytes);

            if chunk_buf.len() >= chunk_limit {
                let send_bytes = Bytes::from(std::mem::replace(
                    &mut chunk_buf,
                    Vec::with_capacity(chunk_limit),
                ));
                let end_offset = current_offset + send_bytes.len() as u64 - 1;
                let headers = self.get_auth_headers().await?;
                let resp = self
                    .transport
                    .patch_chunk(
                        &session_url,
                        headers,
                        send_bytes,
                        (current_offset, end_offset),
                    )
                    .await?;

                if !resp.status.is_success()
                    && resp.status != StatusCode::ACCEPTED
                    && resp.status != StatusCode::NO_CONTENT
                {
                    return Err(OciError::BlobUploadFailed(resp.status));
                }

                if let Some(new_loc) = resp.location {
                    session_url = if new_loc.starts_with('/') {
                        format!("{}://{}{}", self.url_scheme(), self.registry, new_loc)
                    } else {
                        new_loc
                    };
                }
                current_offset = end_offset + 1;
            }
        }

        if !chunk_buf.is_empty() {
            let send_bytes = Bytes::from(chunk_buf);
            let end_offset = current_offset + send_bytes.len() as u64 - 1;
            let headers = self.get_auth_headers().await?;
            let resp = self
                .transport
                .patch_chunk(
                    &session_url,
                    headers,
                    send_bytes,
                    (current_offset, end_offset),
                )
                .await?;

            if !resp.status.is_success()
                && resp.status != StatusCode::ACCEPTED
                && resp.status != StatusCode::NO_CONTENT
            {
                return Err(OciError::BlobUploadFailed(resp.status));
            }

            if let Some(new_loc) = resp.location {
                session_url = if new_loc.starts_with('/') {
                    format!("{}://{}{}", self.url_scheme(), self.registry, new_loc)
                } else {
                    new_loc
                };
            }
        }

        let final_digest = hash_state.force_finalize();
        let total_size = hash_state.bytes_streamed();

        let separator = if session_url.contains('?') { "&" } else { "?" };
        let finish_url = format!("{}{}digest={}", session_url, separator, final_digest);
        let headers = self.get_auth_headers().await?;
        let finish_status = self
            .transport
            .put_chunk_finish(&finish_url, headers, None)
            .await?;

        if finish_status == StatusCode::CREATED
            || finish_status == StatusCode::OK
            || finish_status == StatusCode::ACCEPTED
        {
            info!(
                "Successfully committed streaming blob {} ({} bytes)",
                final_digest, total_size
            );
            Ok((final_digest, total_size))
        } else {
            Err(OciError::BlobUploadFailed(finish_status))
        }
    }

    #[cfg(feature = "tokio-fs")]
    pub async fn push_blob(&self, file_path: &Path) -> Result<String, OciError> {
        let meta = tokio::fs::metadata(file_path).await?;
        let size = meta.len();
        if size < 64 * 1024 * 1024 {
            let data = tokio::fs::read(file_path).await?;
            self.push_blob_bytes(Bytes::from(data)).await
        } else {
            let data = tokio::fs::read(file_path).await?;
            let digest = compute_sha256_digest(&data);
            self.push_blob_file_resumable(file_path, &digest, &UploadConfig::default())
                .await
        }
    }

    pub async fn push_json_blob<S: Serialize>(&self, data: &S) -> Result<(String, u64), OciError> {
        let json_bytes = serde_json::to_vec_pretty(data)?;
        let size = json_bytes.len() as u64;

        let digest = compute_sha256_digest(&json_bytes);

        if self.head_blob(&digest).await? {
            return Ok((digest, size));
        }

        let bytes = Bytes::from(json_bytes);
        let pushed_digest = self.push_blob_bytes_with_digest(&digest, bytes).await?;
        Ok((pushed_digest, size))
    }

    pub async fn get_manifest_with_digest(
        &self,
        tag: &str,
    ) -> Result<Option<(String, String)>, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            tag
        );

        let headers = self.get_auth_headers().await?;
        let (status, resp_headers, bytes) = self.transport.get(&url, headers).await?;

        if status == StatusCode::OK {
            let digest_header = resp_headers
                .get("Docker-Content-Digest")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            let body = String::from_utf8(bytes.to_vec())
                .map_err(|e| OciError::Other(format!("Invalid UTF-8 manifest: {}", e)))?;

            let digest = digest_header.unwrap_or_else(|| compute_sha256_digest(body.as_bytes()));

            Ok(Some((body, digest)))
        } else if status == StatusCode::NOT_FOUND {
            Ok(None)
        } else {
            Err(OciError::ManifestFetchFailed(status))
        }
    }

    pub async fn get_manifest(&self, tag: &str) -> Result<Option<String>, OciError> {
        self.get_manifest_with_digest(tag)
            .await
            .map(|opt| opt.map(|(body, _)| body))
    }

    /// 拉取并解析 OCI Image Index
    pub async fn fetch_artifact(
        &self,
        tag_or_digest: &str,
    ) -> Result<Option<FetchedOciArtifact>, OciError> {
        let (body, digest) = match self.get_manifest_with_digest(tag_or_digest).await? {
            Some(res) => res,
            None => return Ok(None),
        };

        let index: OciImageIndex = serde_json::from_str(&body)?;
        Ok(Some(FetchedOciArtifact { index, digest }))
    }

    pub async fn get_image_manifest(
        &self,
        tag: &str,
    ) -> Result<Option<(OciImageManifest, String)>, OciError> {
        match self.get_manifest_with_digest(tag).await? {
            Some((json_str, digest)) => {
                let manifest = serde_json::from_str::<OciImageManifest>(&json_str)?;
                Ok(Some((manifest, digest)))
            }
            None => Ok(None),
        }
    }

    pub async fn get_image_index(
        &self,
        tag: &str,
    ) -> Result<Option<(OciImageIndex, String)>, OciError> {
        match self.get_manifest_with_digest(tag).await? {
            Some((json_str, digest)) => {
                let index = serde_json::from_str::<OciImageIndex>(&json_str)?;
                Ok(Some((index, digest)))
            }
            None => Ok(None),
        }
    }

    /// 针对特定系统架构，按需拉取单架构基线索引数据
    pub async fn get_arch_cache_index(
        &self,
        tag: &str,
        system: &SystemArch,
    ) -> Result<Option<(ArchCacheIndexData, String)>, OciError> {
        let arch_tag = format!("{}-{}", tag, system.as_str());
        if let Some((sub_manifest, sub_digest)) = self.get_image_manifest(&arch_tag).await?
            && let Some(blob_digest) = sub_manifest.first_layer_digest()
        {
            let blob_bytes = self.get_blob(blob_digest).await?;
            let arch_data: ArchCacheIndexData = serde_json::from_slice(&blob_bytes)?;
            return Ok(Some((arch_data, sub_digest)));
        }

        let artifact = match self.fetch_artifact(tag).await? {
            Some(a) => a,
            None => return Ok(None),
        };

        let descriptor = match artifact.index.find_manifest_for_system(system) {
            Some(d) => d,
            None => return Ok(None),
        };

        let (sub_manifest_json, _) = self
            .get_manifest_with_digest(&descriptor.digest)
            .await?
            .ok_or_else(|| {
                OciError::Other(format!("Sub-manifest {} missing", descriptor.digest))
            })?;

        let sub_manifest: OciImageManifest = serde_json::from_str(&sub_manifest_json)?;
        let blob_digest = sub_manifest
            .first_layer_digest()
            .ok_or_else(|| OciError::Other("Sub-manifest missing layer digest".to_string()))?;

        let blob_bytes = self.get_blob(blob_digest).await?;
        let arch_data: ArchCacheIndexData = serde_json::from_slice(&blob_bytes)?;
        Ok(Some((arch_data, artifact.digest)))
    }

    /// 针对特定系统架构，按需拉取单架构会话清单数据
    pub async fn get_arch_session_manifest(
        &self,
        tag: &str,
        system: &SystemArch,
    ) -> Result<Option<(ArchRunSessionManifest, String)>, OciError> {
        let arch_tag = format!("{}-{}", tag, system.as_str());
        if let Some((sub_manifest, sub_digest)) = self.get_image_manifest(&arch_tag).await?
            && let Some(blob_digest) = sub_manifest.first_layer_digest()
        {
            let blob_bytes = self.get_blob(blob_digest).await?;
            let arch_session: ArchRunSessionManifest = serde_json::from_slice(&blob_bytes)?;
            return Ok(Some((arch_session, sub_digest)));
        }

        let artifact = match self.fetch_artifact(tag).await? {
            Some(a) => a,
            None => return Ok(None),
        };

        let descriptor = match artifact.index.find_manifest_for_system(system) {
            Some(d) => d,
            None => return Ok(None),
        };

        let (sub_manifest_json, _) = self
            .get_manifest_with_digest(&descriptor.digest)
            .await?
            .ok_or_else(|| {
                OciError::Other(format!("Sub-manifest {} missing", descriptor.digest))
            })?;

        let sub_manifest: OciImageManifest = serde_json::from_str(&sub_manifest_json)?;
        let blob_digest = sub_manifest
            .first_layer_digest()
            .ok_or_else(|| OciError::Other("Sub-manifest missing layer digest".to_string()))?;

        let blob_bytes = self.get_blob(blob_digest).await?;
        let arch_session: ArchRunSessionManifest = serde_json::from_slice(&blob_bytes)?;
        Ok(Some((arch_session, artifact.digest)))
    }

    pub async fn get_session_manifest(
        &self,
        tag: &str,
    ) -> Result<Option<(RunSessionManifest, String)>, OciError> {
        let artifact = match self.fetch_artifact(tag).await? {
            Some(a) => a,
            None => return Ok(None),
        };

        let mut combined = RunSessionManifest {
            version: RUN_SESSION_VERSION,
            created_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            updated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ..Default::default()
        };

        for desc in &artifact.index.manifests {
            if let Ok(Some((sub_json, _))) = self.get_manifest_with_digest(&desc.digest).await
                && let Ok(sub_manifest) = serde_json::from_str::<OciImageManifest>(&sub_json)
                && let Some(blob_digest) = sub_manifest.first_layer_digest()
                && let Ok(blob_bytes) = self.get_blob(blob_digest).await
                && let Ok(arch_session) =
                    serde_json::from_slice::<ArchRunSessionManifest>(&blob_bytes)
            {
                combined.run_id = arch_session.run_id;
                if combined.head_sha.is_empty() {
                    combined.head_sha = arch_session.head_sha;
                }
                if combined.ref_name.is_empty() {
                    combined.ref_name = arch_session.ref_name;
                }
                if combined.public_key.is_none() {
                    combined.public_key = arch_session.public_key;
                }
                combined.entries.extend(arch_session.entries);
                if !arch_session.gc_roots.is_empty() {
                    combined
                        .gc_roots
                        .entry(arch_session.system)
                        .or_default()
                        .extend(arch_session.gc_roots);
                }
                combined.completed_jobs.extend(arch_session.completed_jobs);
            }
        }
        Ok(Some((combined, artifact.digest)))
    }

    pub async fn get_cache_index(
        &self,
        tag: &str,
    ) -> Result<Option<(CacheIndexData, String)>, OciError> {
        let artifact = match self.fetch_artifact(tag).await? {
            Some(a) => a,
            None => return Ok(None),
        };

        let mut combined = CacheIndexData {
            version: CACHE_INDEX_VERSION,
            repo: self.repo.clone(),
            registry: self.registry.clone(),
            image: format!("{}/{}/nix-cache", self.registry, self.repo),
            generated: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ..Default::default()
        };

        for desc in &artifact.index.manifests {
            if let Ok(Some((sub_json, _))) = self.get_manifest_with_digest(&desc.digest).await
                && let Ok(sub_manifest) = serde_json::from_str::<OciImageManifest>(&sub_json)
                && let Some(blob_digest) = sub_manifest.first_layer_digest()
                && let Ok(blob_bytes) = self.get_blob(blob_digest).await
                && let Ok(arch_data) = serde_json::from_slice::<ArchCacheIndexData>(&blob_bytes)
            {
                if combined.public_key.is_empty() && !arch_data.public_key.is_empty() {
                    combined.public_key = arch_data.public_key;
                }
                if combined.last_promoted_run.is_none() && arch_data.last_promoted_run.is_some() {
                    combined.last_promoted_run = arch_data.last_promoted_run;
                }
                combined.entries.extend(arch_data.entries);
                if !arch_data.gc_roots.is_empty() {
                    combined
                        .gc_roots
                        .entry(arch_data.system)
                        .or_default()
                        .extend(arch_data.gc_roots);
                }
            }
        }
        Ok(Some((combined, artifact.digest)))
    }

    pub async fn put_manifest_conditional(
        &self,
        tag: &str,
        manifest: &str,
        previous_digest: Option<&str>,
    ) -> Result<(), OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            tag
        );

        let mut headers = self.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static(OCI_IMAGE_MANIFEST_MEDIA_TYPE),
        );

        if let Some(prev) = previous_digest
            && let Ok(val) = HeaderValue::from_str(prev)
        {
            headers.insert(IF_MATCH, val);
        }

        let bytes = Bytes::copy_from_slice(manifest.as_bytes());
        let status = self.transport.put_bytes(&url, headers, bytes).await?;

        if status == StatusCode::PRECONDITION_FAILED || status == StatusCode::CONFLICT {
            return Err(OciError::CasConflict(tag.to_string()));
        }

        if status == StatusCode::OK
            || status == StatusCode::CREATED
            || status == StatusCode::ACCEPTED
        {
            info!("Successfully pushed manifest for tag {}", tag);
            Ok(())
        } else {
            Err(OciError::ManifestPushFailed(status))
        }
    }

    pub async fn put_image_index_conditional(
        &self,
        tag: &str,
        index: &OciImageIndex,
        previous_digest: Option<&str>,
    ) -> Result<(), OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            tag
        );

        let mut headers = self.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static(OCI_IMAGE_INDEX_MEDIA_TYPE),
        );

        if let Some(prev) = previous_digest
            && let Ok(val) = HeaderValue::from_str(prev)
        {
            headers.insert(IF_MATCH, val);
        }

        let index_json = index.to_json_string()?;
        let bytes = Bytes::copy_from_slice(index_json.as_bytes());
        let status = self.transport.put_bytes(&url, headers, bytes).await?;

        if status == StatusCode::PRECONDITION_FAILED || status == StatusCode::CONFLICT {
            return Err(OciError::CasConflict(tag.to_string()));
        }

        if status == StatusCode::OK
            || status == StatusCode::CREATED
            || status == StatusCode::ACCEPTED
        {
            info!("Successfully pushed OCI Image Index for tag {}", tag);
            Ok(())
        } else {
            Err(OciError::ManifestPushFailed(status))
        }
    }

    pub async fn push_image_index(&self, tag: &str, index: &OciImageIndex) -> Result<(), OciError> {
        self.put_image_index_conditional(tag, index, None).await
    }

    pub async fn push_manifest(&self, tag: &str, manifest: &str) -> Result<(), OciError> {
        self.put_manifest_conditional(tag, manifest, None).await
    }

    pub async fn delete_manifest(&self, tag_or_digest: &str) -> Result<bool, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            tag_or_digest
        );

        let headers = self.get_auth_headers().await?;
        let status = self.transport.delete(&url, headers).await?;

        if status.is_success() {
            Ok(true)
        } else if status == StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            Err(OciError::Other(format!(
                "Failed to delete manifest with status: {}",
                status
            )))
        }
    }

    pub async fn update_image_index_cas<F>(
        &self,
        tag: &str,
        max_retries: usize,
        mut mutator: F,
    ) -> Result<(), OciError>
    where
        F: FnMut(Option<OciImageIndex>) -> Result<OciImageIndex, OciError>,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (existing_index, prev_digest) = match self.fetch_artifact(tag).await? {
                Some(artifact) => (Some(artifact.index), Some(artifact.digest)),
                None => (None, None),
            };

            let updated_index = mutator(existing_index)?;
            match self
                .put_image_index_conditional(tag, &updated_index, prev_digest.as_deref())
                .await
            {
                Ok(_) => return Ok(()),
                Err(OciError::CasConflict(_)) if attempt <= max_retries => {
                    let pid = {
                        #[cfg(target_arch = "wasm32")]
                        {
                            0u64
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            std::process::id() as u64
                        }
                    };
                    let backoff_ms =
                        (500 * (1 << attempt.min(5))) + ((pid * 37 + attempt as u64 * 53) % 150);
                    warn!(
                        "CAS conflict on Image Index tag {}, retrying in {}ms (attempt {}/{})",
                        tag, backoff_ms, attempt, max_retries
                    );
                    self.transport
                        .sleep(Duration::from_millis(backoff_ms))
                        .await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn update_manifest_cas<S, F>(
        &self,
        tag: &str,
        max_retries: usize,
        mut mutator: F,
    ) -> Result<(), OciError>
    where
        S: Serialize + for<'de> Deserialize<'de> + Send,
        F: FnMut(Option<S>) -> Result<S, OciError>,
    {
        let empty_config = Bytes::from_static(b"{}");
        let config_digest = self.push_blob_bytes(empty_config).await?;
        let config_size = 2u64;

        let mut attempt = 0;
        loop {
            attempt += 1;
            let (existing_data, prev_digest) = match self.get_manifest_with_digest(tag).await? {
                Some((manifest_json, digest)) => {
                    let manifest = serde_json::from_str::<OciImageManifest>(&manifest_json)?;
                    let blob_digest = manifest.first_layer_digest().ok_or_else(|| {
                        OciError::Other("Manifest missing layer digest".to_string())
                    })?;
                    let blob_bytes = self.get_blob(blob_digest).await?;
                    let data = serde_json::from_slice::<S>(&blob_bytes)?;
                    (Some(data), Some(digest))
                }
                None => (None, None),
            };

            let updated_data = mutator(existing_data)?;
            let (blob_digest, blob_size) = self.push_json_blob(&updated_data).await?;

            let manifest =
                build_index_manifest(&blob_digest, blob_size, &config_digest, config_size);
            let manifest_str = manifest.to_json_string()?;

            match self
                .put_manifest_conditional(tag, &manifest_str, prev_digest.as_deref())
                .await
            {
                Ok(_) => return Ok(()),
                Err(OciError::CasConflict(_)) if attempt <= max_retries => {
                    let pid = {
                        #[cfg(target_arch = "wasm32")]
                        {
                            0u64
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            std::process::id() as u64
                        }
                    };
                    let backoff_ms =
                        (500 * (1 << attempt.min(5))) + ((pid * 37 + attempt as u64 * 53) % 150);
                    warn!(
                        "CAS conflict on tag {}, retrying in {}ms (attempt {}/{})",
                        tag, backoff_ms, attempt, max_retries
                    );
                    self.transport
                        .sleep(Duration::from_millis(backoff_ms))
                        .await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 单架构无锁 CAS 更新会话 (无跨架构竞争)
    pub async fn update_arch_session_with_cas(
        &self,
        request: SessionMutationRequest,
    ) -> Result<(), OciError> {
        let arch_tag = format!("run-{}-{}", request.run_id, request.system.as_str());
        let mut attempt = 0;

        let empty_config = Bytes::from_static(b"{}");
        let config_digest = self.push_blob_bytes(empty_config).await?;
        let config_size = 2u64;

        loop {
            attempt += 1;
            let (mut arch_session, previous_digest) = match self
                .get_arch_session_manifest(&arch_tag, &request.system)
                .await?
            {
                Some((data, digest)) => (data, Some(digest)),
                None => (
                    ArchRunSessionManifest::new(request.run_id, request.system.clone()),
                    None,
                ),
            };

            request.apply_to_arch(&mut arch_session);

            let (session_blob_digest, session_blob_size) =
                self.push_json_blob(&arch_session).await?;
            let manifest = build_arch_session_manifest(
                &session_blob_digest,
                session_blob_size,
                &config_digest,
                config_size,
                request.run_id,
                &request.system,
            );
            let manifest_str = manifest.to_json_string()?;

            match self
                .put_manifest_conditional(&arch_tag, &manifest_str, previous_digest.as_deref())
                .await
            {
                Ok(_) => {
                    info!(
                        "Successfully updated arch-session tag {} on attempt {}",
                        arch_tag, attempt
                    );
                    return Ok(());
                }
                Err(OciError::CasConflict(_)) if attempt <= request.max_retries => {
                    let pid = {
                        #[cfg(target_arch = "wasm32")]
                        {
                            0u64
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            std::process::id() as u64
                        }
                    };
                    let backoff_ms =
                        (500 * (1 << attempt.min(5))) + ((pid * 37 + attempt as u64 * 53) % 150);
                    warn!(
                        "CAS conflict on arch tag {}, retrying in {}ms (attempt {}/{})",
                        arch_tag, backoff_ms, attempt, request.max_retries
                    );
                    self.transport
                        .sleep(Duration::from_millis(backoff_ms))
                        .await;
                }
                Err(e) => {
                    error!(
                        "Failed to update arch-session tag {} after {} attempts: {}",
                        arch_tag, attempt, e
                    );
                    let fallback_tag = format!(
                        "run-{}-{}-job-{}",
                        request.run_id,
                        request.system.as_str(),
                        request.job_id.replace(['/', ':', ' '], "-")
                    );
                    warn!("Falling back to job-specific chunk tag: {}", fallback_tag);
                    self.push_manifest(&fallback_tag, &manifest_str).await?;
                    return Ok(());
                }
            }
        }
    }

    pub async fn update_run_session_with_cas(
        &self,
        request: SessionMutationRequest,
    ) -> Result<(), OciError> {
        self.update_arch_session_with_cas(request).await
    }

    pub async fn get_blob(&self, digest: &str) -> Result<Bytes, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let headers = self.get_auth_headers().await?;
        let (status, _resp_headers, bytes) = self.transport.get(&url, headers).await?;

        if status.is_success() {
            Ok(bytes)
        } else if status == StatusCode::NOT_FOUND {
            Err(OciError::BlobNotFound(digest.to_string()))
        } else {
            Err(OciError::BlobDownloadFailed(status))
        }
    }

    pub async fn stream_blob(
        &self,
        digest: &str,
    ) -> Result<OciBlobStream<T::BodyStream>, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let headers = self.get_auth_headers().await?;
        let (status, resp_headers, stream) = self.transport.stream(&url, headers).await?;

        if status.is_success() {
            Ok(OciBlobStream::new(status, resp_headers, stream))
        } else if status == StatusCode::NOT_FOUND {
            Err(OciError::BlobNotFound(digest.to_string()))
        } else {
            Err(OciError::BlobDownloadFailed(status))
        }
    }
}
