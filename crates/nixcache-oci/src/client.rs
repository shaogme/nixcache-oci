use crate::{
    backend::{
        BlobUploadStrategy, GitHubPackagesClient, OciDriver, RegistryCapabilities,
        RegistryDeletionStrategy, RegistryKind, detect_driver, driver_for_kind,
    },
    codec::{DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec},
    error::OciError,
    manifest::{
        CacheLayerMediaType, CacheLayerMediaTypeV5, EMPTY_CONFIG_DIGEST, EMPTY_CONFIG_SIZE,
        OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest,
        OciImageIndex, OciImageManifest, ShardedArchIndexManifestParams,
        build_delta_patch_manifest, build_sharded_arch_index_manifest,
    },
    mutation::SessionMutationRequest,
    token::TokenManager,
    transport::{HashingStream, OciBlobStream, OciTransport},
    upload::UploadConfig,
};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode, header::IF_MATCH};
use nixcache_core::{
    BloomFilterManifest, DeltaPatchData, FastBlockedBloomFilter, NarDigest, ShardDataPayload,
    ShardedArchCacheIndexData, SystemArch,
};
use nixcache_utils::get_process_id;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{pin::pin, str::from_utf8, sync::Arc, time::Duration};
use tracing::{error, info, warn};

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletionSummary {
    pub deleted_count: usize,
    pub not_found_count: usize,
    pub failed_count: usize,
    pub freed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedOciArtifact {
    pub manifest: OciArtifactManifest,
    pub digest: String,
}

#[derive(Clone, Debug)]
pub struct OciClient<T: OciTransport> {
    registry: String,
    repo: String,
    driver: OciDriver,
    token_manager: TokenManager,
    transport: T,
}

impl<T: OciTransport + Clone> OciClient<T> {
    /// 基于指定驱动构造 OCI 客户端
    pub fn new(
        registry: &str,
        repo: &str,
        auth_token: &str,
        write_access: bool,
        driver: impl Into<OciDriver>,
        transport: T,
    ) -> Self {
        let driver = driver.into();
        let canonical_registry = driver.canonicalize_endpoint(registry);
        let canonical_repo = driver.canonicalize_repository(repo);
        let token_manager = TokenManager::new(
            &canonical_registry,
            &canonical_repo,
            auth_token,
            write_access,
            driver,
        );

        Self {
            registry: canonical_registry,
            repo: canonical_repo,
            driver,
            token_manager,
            transport,
        }
    }

    /// 基于指定的 RegistryKind 构造 OCI 客户端
    pub fn from_kind(
        kind: RegistryKind,
        registry: &str,
        repo: &str,
        auth_token: &str,
        write_access: bool,
        transport: T,
    ) -> Self {
        let driver = driver_for_kind(kind);
        Self::new(registry, repo, auth_token, write_access, driver, transport)
    }

    /// 自动根据 registry 域名推导后端类型并构造 OCI 客户端
    pub fn with_transport(
        registry: &str,
        repo: &str,
        auth_token: &str,
        write_access: bool,
        transport: T,
    ) -> Self {
        let driver = detect_driver(registry);
        Self::new(registry, repo, auth_token, write_access, driver, transport)
    }

    pub fn driver(&self) -> &OciDriver {
        &self.driver
    }

    pub fn kind(&self) -> RegistryKind {
        self.driver.kind()
    }

    pub fn capabilities(&self) -> &'static RegistryCapabilities {
        self.driver.capabilities()
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

    pub fn url_scheme(&self) -> &str {
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

    pub async fn get_token(&self) -> Result<Arc<str>, OciError> {
        self.token_manager.get_token(&self.transport).await
    }

    pub async fn get_auth_headers(&self) -> Result<HeaderMap, OciError> {
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

    /// 执行确定性两阶段会话 PUT 上传 (POST /uploads/ -> PUT <location>?digest=...)
    async fn execute_two_step_put(&self, digest: &str, bytes: Bytes) -> Result<String, OciError> {
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
            .ok_or(OciError::UploadLocationMissing)?;

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

    pub async fn push_blob_bytes_with_digest(
        &self,
        digest: &str,
        bytes: Bytes,
    ) -> Result<String, OciError> {
        if self.head_blob(digest).await? {
            info!("Blob {} already exists, skipping upload.", digest);
            return Ok(digest.to_string());
        }

        let strategy = self.driver.capabilities().fixed_upload_strategy;
        match strategy {
            BlobUploadStrategy::FixedTwoStepPut => {
                // GHCR 等固化两阶段后端：严禁 Monolithic POST，100% 走两阶段 PUT
                self.execute_two_step_put(digest, bytes).await
            }
            BlobUploadStrategy::PreferMonolithicPost
            | BlobUploadStrategy::ResumableChunkedPatch => {
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
                        Ok(digest.to_string())
                    }
                    Ok((status, _)) => {
                        warn!(
                            "Monolithic POST returned status {}, falling back to two-step upload for blob {}",
                            status, digest
                        );
                        self.execute_two_step_put(digest, bytes).await
                    }
                    Err(e) => {
                        warn!(
                            "Monolithic POST failed ({}), falling back to two-step upload for blob {}",
                            e, digest
                        );
                        self.execute_two_step_put(digest, bytes).await
                    }
                }
            }
        }
    }

    pub async fn push_blob_bytes(&self, bytes: Bytes) -> Result<String, OciError> {
        let digest = compute_sha256_digest(&bytes);
        self.push_blob_bytes_with_digest(&digest, bytes).await
    }

    /// 确保 OCI 规范所需的空配置 Blob (b"{}") 已存在于目标 Registry 中
    pub async fn ensure_empty_config_blob(&self) -> Result<(), OciError> {
        if !self.head_blob(EMPTY_CONFIG_DIGEST).await? {
            self.push_blob_bytes_with_digest(EMPTY_CONFIG_DIGEST, Bytes::from_static(b"{}"))
                .await?;
        }
        Ok(())
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
            .ok_or(OciError::UploadLocationMissing)?;

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

    /// 确定性无盘流式上传管道 (依据后端 Driver 能力矩阵静态调度，彻底废除 416 运行时降级)
    pub async fn push_blob_streaming_resumable(
        &self,
        stream: T::BodyStream,
        config: &UploadConfig,
    ) -> Result<(String, u64), OciError> {
        let (hashing_stream, hash_state) = HashingStream::new(stream);
        let mut pinned_stream = pin!(hashing_stream);

        let capabilities = self.driver.capabilities();
        let strategy = capabilities.fixed_upload_strategy;

        info!(
            "Initiating deterministic streaming upload (backend: {:?}, strategy: {:?})",
            self.driver.kind(),
            strategy
        );

        let chunk_limit = config.chunk_size_bytes.max(1024 * 1024);
        let threshold = config.chunk_threshold_bytes.max(chunk_limit as u64) as usize;
        let mut buffer = BytesMut::with_capacity(threshold.max(chunk_limit * 2));

        // 判定是否应当启用分块上传：仅当 Driver 明确支持分块且策略配置为 ResumableChunkedPatch 时
        let allow_chunked = capabilities.supports_chunked_patch
            && strategy == BlobUploadStrategy::ResumableChunkedPatch;

        // 阶段 1：缓冲流数据
        while let Some(item) = pinned_stream.next().await {
            let bytes: Bytes = item?;
            buffer.extend_from_slice(&bytes);
            if buffer.len() >= threshold && allow_chunked {
                break;
            }
        }

        // 情况 A：流数据完整缓冲或当前后端不支持分块 (GHCR / PreferMonolithicPost)
        if buffer.len() < threshold || !allow_chunked {
            while let Some(item) = pinned_stream.next().await {
                let bytes: Bytes = item?;
                buffer.extend_from_slice(&bytes);
            }

            let final_digest = hash_state.force_finalize();
            let total_size = hash_state.bytes_streamed();

            // 1. 先 HEAD 检查是否已存在，如已存在直接秒级复用
            if self.head_blob(&final_digest).await? {
                info!("Blob {} already exists, skipping upload.", final_digest);
                return Ok((final_digest, total_size));
            }

            // 2. 依据 Driver 静态确定的策略直传完整 Payload
            let complete_bytes = buffer.freeze();
            let pushed_digest = self
                .push_blob_bytes_with_digest(&final_digest, complete_bytes)
                .await?;

            info!(
                "Successfully uploaded streaming blob {} ({} bytes)",
                pushed_digest, total_size
            );
            return Ok((pushed_digest, total_size));
        }

        // 情况 B：确定性分块断点续传 (仅在支持分块的后端执行，无需任何 416 猜测降级)
        info!("Executing standard chunked resumable upload for large stream");
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
            .ok_or(OciError::UploadLocationMissing)?;

        let mut session_url = if location.starts_with('/') {
            format!("{}://{}{}", self.url_scheme(), self.registry, location)
        } else {
            location.to_string()
        };

        let mut current_offset = 0u64;
        let mut chunk_buf = buffer;
        let mut stream_ended = false;

        while !stream_ended {
            while chunk_buf.len() < chunk_limit {
                if let Some(item) = pinned_stream.next().await {
                    let bytes: Bytes = item?;
                    chunk_buf.extend_from_slice(&bytes);
                } else {
                    stream_ended = true;
                    break;
                }
            }

            if chunk_buf.is_empty() {
                break;
            }

            let send_len = if stream_ended {
                chunk_buf.len()
            } else {
                chunk_limit.min(chunk_buf.len())
            };

            let send_bytes = chunk_buf.split_to(send_len).freeze();
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

    /// 将数据紧凑序列化并经过 Zstd 压缩后推送至 Registry Blob
    /// 返回: (blob_digest, compressed_size, uncompressed_size)
    pub async fn push_zstd_blob<S: Serialize>(
        &self,
        data: &S,
    ) -> Result<(String, u64, u64), OciError> {
        let raw_json = serde_json::to_vec(data)?;
        let uncompressed_size = raw_json.len() as u64;

        let compressed_bytes = IndexCodec::encode_zstd(data, DEFAULT_ZSTD_COMPRESSION_LEVEL)?;
        let compressed_size = compressed_bytes.len() as u64;
        let digest = compute_sha256_digest(&compressed_bytes);

        if self.head_blob(&digest).await? {
            return Ok((digest, compressed_size, uncompressed_size));
        }

        let pushed_digest = self
            .push_blob_bytes_with_digest(&digest, compressed_bytes)
            .await?;
        Ok((pushed_digest, compressed_size, uncompressed_size))
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

            let body = from_utf8(&bytes)?;

            let digest = digest_header.unwrap_or_else(|| compute_sha256_digest(&bytes));

            Ok(Some((body.to_string(), digest)))
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

    /// 拉取并解析 OCI 产物（强类型枚举支持 OciImageIndex 与 OciImageManifest）
    pub async fn fetch_artifact(
        &self,
        tag_or_digest: &str,
    ) -> Result<Option<FetchedOciArtifact>, OciError> {
        let (body, digest) = match self.get_manifest_with_digest(tag_or_digest).await? {
            Some(res) => res,
            None => return Ok(None),
        };

        let manifest: OciArtifactManifest = serde_json::from_str(&body)?;
        Ok(Some(FetchedOciArtifact { manifest, digest }))
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

    /// 针对特定系统架构，按需拉取单架构 Schema v5 分片根索引目录数据
    pub async fn get_sharded_root_index(
        &self,
        tag: &str,
        system: &SystemArch,
    ) -> Result<Option<(ShardedArchCacheIndexData, String)>, OciError> {
        let arch_tag = if tag.ends_with(system.as_str()) {
            tag.to_string()
        } else {
            format!("{}-{}", tag, system.as_str())
        };

        if let Some((sub_manifest, sub_digest)) = self.get_image_manifest(&arch_tag).await?
            && let Some(layer) = sub_manifest
                .layers
                .iter()
                .find(|l| {
                    l.media_type == CacheLayerMediaTypeV5::ROOT_INDEX_V5_ZSTD
                        || CacheLayerMediaType::parse(&l.media_type)
                            .is_some_and(|m| m.is_root_index())
                })
                .or_else(|| sub_manifest.layers.first())
        {
            let blob_bytes = self.get_blob(&layer.digest).await?;
            let root_data: ShardedArchCacheIndexData =
                IndexCodec::decode_zstd(&blob_bytes, &layer.media_type)?;
            return Ok(Some((root_data, sub_digest)));
        }

        let artifact = match self.fetch_artifact(tag).await? {
            Some(a) => a,
            None => return Ok(None),
        };

        match artifact.manifest {
            OciArtifactManifest::Index(ref index) => {
                let descriptor = match index.find_manifest_for_system(system) {
                    Some(d) => d,
                    None => return Ok(None),
                };

                let (sub_manifest_json, _) = self
                    .get_manifest_with_digest(&descriptor.digest)
                    .await?
                    .ok_or_else(|| OciError::SubManifestMissing {
                        digest: descriptor.digest.clone(),
                    })?;

                let sub_manifest: OciImageManifest = serde_json::from_str(&sub_manifest_json)?;
                let layer = sub_manifest
                    .layers
                    .iter()
                    .find(|l| {
                        l.media_type == CacheLayerMediaTypeV5::ROOT_INDEX_V5_ZSTD
                            || CacheLayerMediaType::parse(&l.media_type)
                                .is_some_and(|m| m.is_root_index())
                    })
                    .or_else(|| sub_manifest.layers.first())
                    .ok_or(OciError::LayerDescriptorMissing)?;

                let blob_bytes = self.get_blob(&layer.digest).await?;
                let root_data: ShardedArchCacheIndexData =
                    IndexCodec::decode_zstd(&blob_bytes, &layer.media_type)?;
                Ok(Some((root_data, artifact.digest)))
            }
            OciArtifactManifest::Manifest(ref sub_manifest) => {
                if let Some(layer) = sub_manifest
                    .layers
                    .iter()
                    .find(|l| {
                        l.media_type == CacheLayerMediaTypeV5::ROOT_INDEX_V5_ZSTD
                            || CacheLayerMediaType::parse(&l.media_type)
                                .is_some_and(|m| m.is_root_index())
                    })
                    .or_else(|| sub_manifest.layers.first())
                {
                    let blob_bytes = self.get_blob(&layer.digest).await?;
                    let root_data: ShardedArchCacheIndexData =
                        IndexCodec::decode_zstd(&blob_bytes, &layer.media_type)?;
                    Ok(Some((root_data, artifact.digest)))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// 推送单架构 Schema v5 分片根索引目录清单 (包含 Root Index 与 Bloom Filter Layers)
    pub async fn push_sharded_root_index(
        &self,
        tag: &str,
        root_data: &ShardedArchCacheIndexData,
        bloom_blob_digest: &str,
        bloom_blob_size: u64,
        previous_digest: Option<&str>,
    ) -> Result<String, OciError> {
        let (root_blob_digest, root_compressed_size, _) = self.push_zstd_blob(root_data).await?;

        let manifest = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
            root_blob_digest: &root_blob_digest,
            root_blob_size: root_compressed_size,
            bloom_blob_digest,
            bloom_blob_size,
            config_digest: EMPTY_CONFIG_DIGEST,
            config_size: EMPTY_CONFIG_SIZE,
            system: &root_data.system,
            merkle_root: &root_data.merkle_root,
        });

        let manifest_str = manifest.to_json_string()?;
        self.put_manifest_conditional(tag, &manifest_str, previous_digest)
            .await?;
        let manifest_digest = compute_sha256_digest(manifest_str.as_bytes());
        Ok(manifest_digest)
    }

    /// 下载并恢复指定 Blob Digest 的布隆过滤器
    pub async fn get_bloom_filter(
        &self,
        bloom_blob_digest: &str,
        num_entries: u64,
        num_hashes: u8,
    ) -> Result<FastBlockedBloomFilter, OciError> {
        let raw_bytes = self.get_blob(bloom_blob_digest).await?;
        IndexCodec::decode_bloom_filter(&raw_bytes, num_entries, num_hashes)
    }

    /// 压缩并推送布隆过滤器 Blob，返回对应的 BloomFilterManifest
    pub async fn push_bloom_filter(
        &self,
        filter: &FastBlockedBloomFilter,
    ) -> Result<BloomFilterManifest, OciError> {
        let compressed_bytes =
            IndexCodec::encode_bloom_filter(filter, DEFAULT_ZSTD_COMPRESSION_LEVEL)?;
        let blob_digest = compute_sha256_digest(&compressed_bytes);
        let compressed_size = compressed_bytes.len() as u64;

        if !self.head_blob(&blob_digest).await? {
            self.push_blob_bytes_with_digest(&blob_digest, compressed_bytes)
                .await?;
        }

        Ok(BloomFilterManifest::new(
            filter.num_entries(),
            filter.num_bits(),
            filter.num_hashes(),
            blob_digest,
            compressed_size,
        ))
    }

    /// 下载并解压指定 Blob Digest 的单分片数据 Payload
    pub async fn get_shard_data(&self, blob_digest: &str) -> Result<ShardDataPayload, OciError> {
        let blob_bytes = self.get_blob(blob_digest).await?;
        IndexCodec::decode_zstd(&blob_bytes, CacheLayerMediaTypeV5::SHARD_DATA_V5_ZSTD)
    }

    /// 压缩并推送单分片数据 Payload
    pub async fn push_shard_data(
        &self,
        payload: &ShardDataPayload,
    ) -> Result<(String, u64, u64), OciError> {
        self.push_zstd_blob(payload).await
    }

    /// 下载并解压指定 Blob Digest 的增量 Delta Patch
    pub async fn get_delta_patch(&self, blob_digest: &str) -> Result<DeltaPatchData, OciError> {
        let blob_bytes = self.get_blob(blob_digest).await?;
        IndexCodec::decode_zstd(&blob_bytes, CacheLayerMediaTypeV5::DELTA_PATCH_V5_ZSTD)
    }

    /// 压缩并推送增量 Delta Patch Blob
    pub async fn push_delta_patch(
        &self,
        delta: &DeltaPatchData,
    ) -> Result<(String, u64, u64), OciError> {
        self.push_zstd_blob(delta).await
    }

    /// 按 Tag 获取 Delta Patch Manifest 并下载反序列化
    pub async fn get_delta_patch_manifest(
        &self,
        tag: &str,
    ) -> Result<Option<(DeltaPatchData, String)>, OciError> {
        let (manifest_json, manifest_digest) = match self.get_manifest_with_digest(tag).await? {
            Some(res) => res,
            None => return Ok(None),
        };

        let manifest: OciImageManifest = serde_json::from_str(&manifest_json)?;
        let layer = match manifest.layers.first() {
            Some(l) => l,
            None => return Ok(None),
        };

        let blob_bytes = self.get_blob(&layer.digest).await?;
        let delta: DeltaPatchData = IndexCodec::decode_zstd(&blob_bytes, &layer.media_type)?;
        Ok(Some((delta, manifest_digest)))
    }

    /// 构造并推送 Delta Patch 的 Image Manifest
    pub async fn push_delta_patch_manifest(
        &self,
        tag: &str,
        delta: &DeltaPatchData,
        previous_digest: Option<&str>,
    ) -> Result<String, OciError> {
        let (delta_blob_digest, delta_blob_size, _) = self.push_delta_patch(delta).await?;
        let manifest = build_delta_patch_manifest(
            &delta_blob_digest,
            delta_blob_size,
            EMPTY_CONFIG_DIGEST,
            EMPTY_CONFIG_SIZE,
            delta.run_id,
            &delta.job_id,
            &delta.system,
        );

        let manifest_str = manifest.to_json_string()?;
        self.put_manifest_conditional(tag, &manifest_str, previous_digest)
            .await?;
        let manifest_digest = compute_sha256_digest(manifest_str.as_bytes());
        Ok(manifest_digest)
    }

    pub async fn put_manifest_conditional(
        &self,
        tag: &str,
        manifest: &str,
        previous_digest: Option<&str>,
    ) -> Result<(), OciError> {
        if manifest.contains(EMPTY_CONFIG_DIGEST) {
            self.ensure_empty_config_blob().await?;
        }

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

        if self.driver.capabilities().supports_manifest_cas_if_match
            && let Some(prev) = previous_digest
            && let Ok(val) = HeaderValue::from_str(prev)
        {
            headers.insert(IF_MATCH, val);
        }

        let bytes = Bytes::copy_from_slice(manifest.as_bytes());
        let status = self.transport.put_bytes(&url, headers, bytes).await?;

        if status == StatusCode::PRECONDITION_FAILED || status == StatusCode::CONFLICT {
            return Err(OciError::CasPreconditionFailed {
                tag: tag.to_string(),
                expected: previous_digest.map(|s| s.to_string()),
                actual: None,
            });
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

        if self.driver.capabilities().supports_manifest_cas_if_match
            && let Some(prev) = previous_digest
            && let Ok(val) = HeaderValue::from_str(prev)
        {
            headers.insert(IF_MATCH, val);
        }

        let index_json = index.to_json_string()?;
        let bytes = Bytes::copy_from_slice(index_json.as_bytes());
        let status = self.transport.put_bytes(&url, headers, bytes).await?;

        if status == StatusCode::PRECONDITION_FAILED || status == StatusCode::CONFLICT {
            return Err(OciError::CasPreconditionFailed {
                tag: tag.to_string(),
                expected: previous_digest.map(|s| s.to_string()),
                actual: None,
            });
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

    pub fn ghcr_client(&self) -> GitHubPackagesClient<T> {
        GitHubPackagesClient::new(
            self.transport.clone(),
            self.token_manager.auth_token(),
            &self.repo,
        )
    }

    /// 严格删除指定 Tag：
    /// - GHCR: 走 GitHub Packages REST API 查找并删除对应的 Package Version；
    /// - Generic OCI: 两阶段安全删除（先 HEAD/GET /manifests/<tag> 获得 Manifest Digest，再 DELETE /manifests/<digest>）；
    /// - 若资源不存在 (404) 视为幂等成功返回 Ok(())；若遇到 401/403/405/5xx 坚决返回 Err。
    pub async fn delete_tag_strict(&self, tag: &str) -> Result<(), OciError> {
        match self.capabilities().deletion_strategy {
            RegistryDeletionStrategy::GitHubPackagesRestApi => {
                self.ghcr_client().delete_by_tag(tag).await
            }
            RegistryDeletionStrategy::StandardOciDelete
            | RegistryDeletionStrategy::DockerHubRestApi
            | RegistryDeletionStrategy::AwsEcrApi => {
                // 两阶段删除：先获取 Tag 指向的 Manifest Digest
                let head_url = format!(
                    "{}://{}/v2/{}/nix-cache/manifests/{}",
                    self.url_scheme(),
                    self.registry,
                    self.repo,
                    tag
                );
                let headers = self.get_auth_headers().await?;
                let (status, resp_headers, body) =
                    match self.transport.get(&head_url, headers).await {
                        Ok(res) => res,
                        Err(e) => return Err(OciError::Transport(e)),
                    };

                if status == StatusCode::NOT_FOUND {
                    return Ok(());
                } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                    return Err(OciError::InsufficientPermission {
                        target: tag.to_string(),
                        required_scope: "pull,delete:manifest",
                        details: format!("HTTP {} when checking manifest tag {}", status, tag),
                    });
                } else if !status.is_success() {
                    return Err(OciError::DeletionFailed {
                        target: tag.to_string(),
                        status,
                        details: format!("HTTP {} when retrieving tag manifest {}", status, tag),
                    });
                }

                let digest = resp_headers
                    .get("Docker-Content-Digest")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| compute_sha256_digest(&body));

                self.delete_manifest_strict(&digest).await?;

                // 部分 Registry 允许直接删除 tag，尝试顺带删除 tag (若不支持忽略该次 direct tag delete)
                let tag_url = format!(
                    "{}://{}/v2/{}/nix-cache/manifests/{}",
                    self.url_scheme(),
                    self.registry,
                    self.repo,
                    tag
                );
                let tag_headers = self.get_auth_headers().await?;
                let _ = self.transport.delete(&tag_url, tag_headers).await;

                Ok(())
            }
            RegistryDeletionStrategy::Unsupported => Err(OciError::OperationNotSupported {
                operation: "delete_tag",
                backend: self.kind(),
                reason: format!(
                    "Registry backend '{}' does not support tag deletion",
                    self.kind()
                ),
            }),
        }
    }

    /// 严格删除指定 Manifest Digest (DELETE /v2/<repo>/manifests/<digest>)
    pub async fn delete_manifest_strict(&self, digest: &str) -> Result<(), OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let headers = self.get_auth_headers().await?;
        let status = self.transport.delete(&url, headers).await?;

        if status.is_success()
            || status == StatusCode::ACCEPTED
            || status == StatusCode::NO_CONTENT
            || status == StatusCode::NOT_FOUND
        {
            Ok(())
        } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            Err(OciError::InsufficientPermission {
                target: digest.to_string(),
                required_scope: "delete:manifest",
                details: format!(
                    "HTTP {} from registry when deleting manifest {}",
                    status, digest
                ),
            })
        } else if status == StatusCode::METHOD_NOT_ALLOWED {
            Err(OciError::OperationNotSupported {
                operation: "delete_manifest",
                backend: self.kind(),
                reason: format!(
                    "Registry returned 405 Method Not Allowed for manifest deletion on {}. Backend deletion strategy: {:?}",
                    digest,
                    self.capabilities().deletion_strategy
                ),
            })
        } else {
            Err(OciError::DeletionFailed {
                target: digest.to_string(),
                status,
                details: format!("HTTP {} when deleting manifest {}", status, digest),
            })
        }
    }

    /// 严格删除单个 OCI NAR Blob (DELETE /v2/<repo>/blobs/<digest>)
    /// 若后端不支持物理删除 (如 GHCR)，抛出 OperationNotSupported 错误
    pub async fn delete_blob_strict(&self, digest: &str) -> Result<(), OciError> {
        if !self.capabilities().supports_blob_physical_deletion {
            return Err(OciError::OperationNotSupported {
                operation: "delete_blob",
                backend: self.kind(),
                reason: format!(
                    "Backend '{}' does not support standalone physical OCI blob deletion. Blobs are automatically reclaimed with package/version removal.",
                    self.kind()
                ),
            });
        }

        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let headers = self.get_auth_headers().await?;
        let status = self.transport.delete(&url, headers).await?;

        if status.is_success()
            || status == StatusCode::ACCEPTED
            || status == StatusCode::NO_CONTENT
            || status == StatusCode::NOT_FOUND
        {
            Ok(())
        } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            Err(OciError::InsufficientPermission {
                target: digest.to_string(),
                required_scope: "delete:blob",
                details: format!(
                    "HTTP {} from registry when deleting blob {}",
                    status, digest
                ),
            })
        } else if status == StatusCode::METHOD_NOT_ALLOWED {
            Err(OciError::OperationNotSupported {
                operation: "delete_blob",
                backend: self.kind(),
                reason: format!(
                    "Registry returned 405 Method Not Allowed for blob deletion {}",
                    digest
                ),
            })
        } else {
            Err(OciError::DeletionFailed {
                target: digest.to_string(),
                status,
                details: format!("HTTP {} when deleting blob {}", status, digest),
            })
        }
    }

    /// 高并发批量物理删除 Blobs：
    /// - 若 strict_mode 为 true 且后端不支持或删除失败，抛出错误终止；
    /// - 若 strict_mode 为 false，记录 failed_count 并返回统计报告。
    pub async fn batch_delete_blobs_strict(
        &self,
        digests: &[NarDigest],
        concurrency: usize,
        strict_mode: bool,
    ) -> Result<DeletionSummary, OciError> {
        if digests.is_empty() {
            return Ok(DeletionSummary::default());
        }

        if !self.capabilities().supports_blob_physical_deletion {
            if strict_mode {
                return Err(OciError::OperationNotSupported {
                    operation: "batch_delete_blobs",
                    backend: self.kind(),
                    reason: format!(
                        "Backend '{}' does not support standalone physical OCI blob deletion. Blobs are automatically reclaimed with package/version removal.",
                        self.kind()
                    ),
                });
            } else {
                return Ok(DeletionSummary {
                    failed_count: digests.len(),
                    ..Default::default()
                });
            }
        }

        let concurrency = concurrency.clamp(1, 32);
        let mut stream = futures_util::stream::iter(digests)
            .map(|digest| {
                let digest_str = digest.to_string();
                async move { self.delete_blob_strict(&digest_str).await }
            })
            .buffer_unordered(concurrency);

        let mut summary = DeletionSummary::default();

        while let Some(res) = stream.next().await {
            match res {
                Ok(()) => summary.deleted_count += 1,
                Err(e) => {
                    if strict_mode {
                        return Err(e);
                    } else {
                        summary.failed_count += 1;
                        warn!("Non-fatal error deleting blob: {}", e);
                    }
                }
            }
        }

        Ok(summary)
    }

    /// 彻底删除/重置远程 Package (适用于 purge --all)：
    /// - GHCR: DELETE /orgs或users/{owner}/packages/container/{pkg}；
    /// - Generic OCI: 遍历删除已知 index 和 manifests，并清空 blobs。
    pub async fn delete_entire_package_strict(&self) -> Result<(), OciError> {
        match self.capabilities().deletion_strategy {
            RegistryDeletionStrategy::GitHubPackagesRestApi => {
                self.ghcr_client().delete_entire_package().await
            }
            RegistryDeletionStrategy::StandardOciDelete
            | RegistryDeletionStrategy::DockerHubRestApi
            | RegistryDeletionStrategy::AwsEcrApi => {
                // 标准 OCI: 尝试拉取 cache-index 并删除各子架构清单及顶层 index
                if let Ok(Some(artifact)) = self.fetch_artifact("cache-index").await {
                    match artifact.manifest {
                        OciArtifactManifest::Index(idx) => {
                            for sub in idx.manifests {
                                let _ = self.delete_manifest_strict(&sub.digest).await;
                            }
                        }
                        OciArtifactManifest::Manifest(_) => {}
                    }
                    let _ = self.delete_manifest_strict(&artifact.digest).await;
                    let _ = self.delete_tag_strict("cache-index").await;
                }
                Ok(())
            }
            RegistryDeletionStrategy::Unsupported => Err(OciError::OperationNotSupported {
                operation: "delete_package",
                backend: self.kind(),
                reason: format!(
                    "Package deletion is not supported on backend '{}'",
                    self.kind()
                ),
            }),
        }
    }

    pub async fn delete_manifest(&self, tag_or_digest: &str) -> Result<bool, OciError> {
        self.delete_manifest_strict(tag_or_digest)
            .await
            .map(|_| true)
    }

    pub async fn delete_blob(&self, digest: &str) -> Result<bool, OciError> {
        self.delete_blob_strict(digest).await.map(|_| true)
    }

    pub async fn batch_delete_blobs(
        &self,
        digests: &[NarDigest],
        concurrency: usize,
    ) -> Result<(usize, usize), OciError> {
        let summary = self
            .batch_delete_blobs_strict(digests, concurrency, false)
            .await?;
        Ok((
            summary.deleted_count,
            summary.failed_count + summary.not_found_count,
        ))
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
                Some(artifact) => (artifact.manifest.as_index().cloned(), Some(artifact.digest)),
                None => (None, None),
            };

            let updated_index = mutator(existing_index)?;
            match self
                .put_image_index_conditional(tag, &updated_index, prev_digest.as_deref())
                .await
            {
                Ok(_) => return Ok(()),
                Err(OciError::CasPreconditionFailed { .. }) if attempt <= max_retries => {
                    let pid = get_process_id();
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

    /// 单架构分片索引根目录 CAS 更新状态机
    pub async fn update_sharded_arch_index_cas<F>(
        &self,
        tag: &str,
        system: &SystemArch,
        max_retries: usize,
        mut mutator: F,
    ) -> Result<String, OciError>
    where
        F: FnMut(
            Option<ShardedArchCacheIndexData>,
        ) -> Result<(ShardedArchCacheIndexData, String, u64), OciError>,
    {
        let mut attempt = 0;
        let arch_tag = if tag.ends_with(system.as_str()) {
            tag.to_string()
        } else {
            format!("{}-{}", tag, system.as_str())
        };

        loop {
            attempt += 1;
            let (existing_root, prev_digest) =
                match self.get_sharded_root_index(&arch_tag, system).await? {
                    Some((data, digest)) => (Some(data), Some(digest)),
                    None => (None, None),
                };

            let (updated_root, bloom_digest, bloom_size) = mutator(existing_root)?;

            match self
                .push_sharded_root_index(
                    &arch_tag,
                    &updated_root,
                    &bloom_digest,
                    bloom_size,
                    prev_digest.as_deref(),
                )
                .await
            {
                Ok(digest) => return Ok(digest),
                Err(OciError::CasPreconditionFailed { .. }) if attempt <= max_retries => {
                    let pid = get_process_id();
                    let backoff_ms =
                        (100 * (1 << attempt.min(5))) + ((pid * 37 + attempt as u64 * 53) % 100);
                    warn!(
                        "CAS conflict on sharded root index tag {}, retrying in {}ms (attempt {}/{})",
                        arch_tag, backoff_ms, attempt, max_retries
                    );
                    self.transport
                        .sleep(Duration::from_millis(backoff_ms))
                        .await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 单架构无锁 CAS 追加会话 Delta Patch (构建期极速提交，零全量开销)
    pub async fn update_run_session_with_cas(
        &self,
        request: SessionMutationRequest,
    ) -> Result<(), OciError> {
        let delta = request.to_delta_patch();
        let arch_tag = format!("run-{}-{}", request.run_id, request.system.as_str());

        let mut attempt = 0;
        loop {
            attempt += 1;
            let (mut current_delta, previous_digest) =
                match self.get_delta_patch_manifest(&arch_tag).await? {
                    Some((d, digest)) => (d, Some(digest)),
                    None => (
                        DeltaPatchData::new(request.run_id, &request.job_id, request.system),
                        None,
                    ),
                };

            request.apply_to_delta(&mut current_delta);

            match self
                .push_delta_patch_manifest(&arch_tag, &current_delta, previous_digest.as_deref())
                .await
            {
                Ok(_) => {
                    info!(
                        "Successfully updated run session delta tag {} on attempt {}",
                        arch_tag, attempt
                    );
                    return Ok(());
                }
                Err(OciError::CasPreconditionFailed { .. }) if attempt <= request.max_retries => {
                    let pid = get_process_id();
                    let backoff_ms =
                        (50 * (1 << attempt.min(4))) + ((pid * 37 + attempt as u64 * 53) % 50);
                    warn!(
                        "CAS conflict on session delta tag {}, retrying in {}ms (attempt {}/{})",
                        arch_tag, backoff_ms, attempt, request.max_retries
                    );
                    self.transport
                        .sleep(Duration::from_millis(backoff_ms))
                        .await;
                }
                Err(e) => {
                    error!(
                        "Failed to update session delta tag {} after {} attempts: {}",
                        arch_tag, attempt, e
                    );
                    let fallback_tag = format!(
                        "run-{}-{}-job-{}",
                        request.run_id,
                        request.system.as_str(),
                        request.job_id.replace(['/', ':', ' '], "-")
                    );
                    warn!("Falling back to job-specific chunk tag: {}", fallback_tag);
                    self.push_delta_patch_manifest(&fallback_tag, &delta, None)
                        .await?;
                    return Ok(());
                }
            }
        }
    }

    pub async fn update_arch_session_with_cas(
        &self,
        request: SessionMutationRequest,
    ) -> Result<(), OciError> {
        self.update_run_session_with_cas(request).await
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
            Err(OciError::BlobNotFound {
                digest: digest.to_string(),
            })
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
            Err(OciError::BlobNotFound {
                digest: digest.to_string(),
            })
        } else {
            Err(OciError::BlobDownloadFailed(status))
        }
    }
}
