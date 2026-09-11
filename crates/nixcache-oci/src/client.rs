use crate::{
    auth::{BearerChallenge, RegistryCredentials, parse_www_authenticate},
    backend::{
        BlobUploadStrategy, GitHubPackagesClient, ManifestCasSupport, OciDriver,
        PackageDeletionSupport, RegistryCapabilities, RegistryDeletionStrategy, RegistryKind,
        detect_driver, driver_for_kind,
    },
    codec::{DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec},
    error::{OciError, TransportError},
    manifest::{
        CacheLayerMediaType, CacheLayerMediaTypeV6, EMPTY_CONFIG_DIGEST, EMPTY_CONFIG_SIZE,
        OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest,
        OciImageIndex, OciImageManifest, ShardedArchIndexManifestParams,
        build_sharded_arch_index_manifest,
    },
    token::TokenManager,
    transport::{
        HashingStream, OciBlobStream, OciTransport, UploadChunkResponse, parse_range_header,
    },
    upload::UploadConfig,
};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{IF_MATCH, LOCATION, RANGE},
};
use nixcache_core::{NarDigest, ShardDataPayload, ShardedArchCacheIndexData, SystemArch};
use nixcache_utils::get_process_id;
use serde::{Deserialize, Serialize, de::Deserializer};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    pin::pin,
    str::from_utf8,
    sync::Arc,
    time::Duration,
};
use tracing::{info, warn};

/// Manifest 发布时使用的 CAS 前置条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestCasCondition {
    /// 只有目标 tag 不存在时才允许发布。
    CreateOnly,
    /// 只有目标 tag 当前 digest 与此值一致时才允许发布。
    Match(String),
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

fn encode_query_value(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                vec![byte as char]
            } else {
                vec!['%', hex_digit(byte >> 4), hex_digit(byte & 0x0f)]
            }
        })
        .collect()
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => unreachable!(),
    }
}

#[derive(Deserialize)]
struct OciTagsListResponse {
    #[serde(deserialize_with = "deserialize_nullable_tags")]
    tags: Vec<String>,
}

fn deserialize_nullable_tags<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletionSummary {
    pub deleted_count: usize,
    pub not_found_count: usize,
    pub failed_count: usize,
    pub freed_bytes: u64,
}

/// 单个 manifest/blob DELETE 的明确结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionOutcome {
    Deleted,
    AlreadyAbsent,
}

/// 一次严格包删除的可审计汇总。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageDeletionSummary {
    pub tags_discovered: usize,
    pub manifests_discovered: usize,
    pub blobs_discovered: usize,
    pub manifests_deleted: usize,
    pub blobs_deleted: usize,
    pub already_absent: usize,
}

#[derive(Debug, Clone)]
struct DeletionPlan {
    tags: Vec<String>,
    manifests: HashMap<String, String>,
    blobs: HashMap<String, u64>,
}

impl DeletionPlan {
    fn summary(&self) -> PackageDeletionSummary {
        PackageDeletionSummary {
            tags_discovered: self.tags.len(),
            manifests_discovered: self.manifests.len(),
            blobs_discovered: self.blobs.len(),
            ..PackageDeletionSummary::default()
        }
    }
}

const MAX_DELETION_MANIFESTS: usize = 100_000;
const MAX_DELETION_BLOBS: usize = 2_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedOciArtifact {
    pub manifest: OciArtifactManifest,
    pub digest: String,
}

#[derive(Clone)]
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
        credentials: impl Into<RegistryCredentials>,
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
            credentials,
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
        credentials: impl Into<RegistryCredentials>,
        write_access: bool,
        transport: T,
    ) -> Self {
        let driver = driver_for_kind(kind);
        Self::new(registry, repo, credentials, write_access, driver, transport)
    }

    /// 自动根据 registry 域名推导后端类型并构造 OCI 客户端
    pub fn with_transport(
        registry: &str,
        repo: &str,
        credentials: impl Into<RegistryCredentials>,
        write_access: bool,
        transport: T,
    ) -> Self {
        let driver = detect_driver(registry);
        Self::new(registry, repo, credentials, write_access, driver, transport)
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

    pub async fn get_token(&self, challenge: &BearerChallenge) -> Result<Arc<str>, OciError> {
        self.token_manager
            .get_token(&self.transport, challenge)
            .await
    }

    pub async fn get_auth_headers(&self) -> Result<HeaderMap, OciError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Accept",
            HeaderValue::from_static(
                "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json",
            ),
        );

        if let Some(token) = self.token_manager.cached_token().await {
            let auth_val = format!("Bearer {}", token);
            if let Ok(val) = HeaderValue::from_str(&auth_val) {
                headers.insert("Authorization", val);
            }
        }
        Ok(headers)
    }

    fn auth_error(
        operation: &'static str,
        status: StatusCode,
        details: impl Into<String>,
    ) -> OciError {
        OciError::AuthenticationFailed {
            operation,
            status,
            details: details.into(),
        }
    }

    fn with_bearer(mut headers: HeaderMap, token: &str) -> HeaderMap {
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert("Authorization", value);
        }
        headers
    }

    async fn challenge_for_response(
        &self,
        operation: &'static str,
        status: StatusCode,
        headers: &HeaderMap,
    ) -> Result<(BearerChallenge, HeaderMap), OciError> {
        if status != StatusCode::UNAUTHORIZED {
            return Err(Self::auth_error(
                operation,
                status,
                "unexpected authentication response",
            ));
        }
        let challenge = parse_www_authenticate(headers)?.ok_or_else(|| {
            Self::auth_error(
                operation,
                status,
                "registry returned 401 without a Bearer challenge",
            )
        })?;
        let token = self
            .token_manager
            .refresh_token(&self.transport, &challenge)
            .await?;
        let request_headers = Self::with_bearer(self.get_auth_headers().await?, &token);
        Ok((challenge, request_headers))
    }

    async fn get_with_auth_retry(
        &self,
        url: &str,
        operation: &'static str,
    ) -> Result<(StatusCode, HeaderMap, Bytes), OciError> {
        let headers = self.get_auth_headers().await?;
        let first = self.transport.get(url, headers).await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let (_, retry_headers) = self
            .challenge_for_response(operation, first.0, &first.1)
            .await?;
        let second = self.transport.get(url, retry_headers).await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second)
    }

    async fn head_with_auth_retry(
        &self,
        url: &str,
        operation: &'static str,
    ) -> Result<(StatusCode, HeaderMap), OciError> {
        let headers = self.get_auth_headers().await?;
        let first = self.transport.head_with_headers(url, headers).await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let (_, retry_headers) = self
            .challenge_for_response(operation, first.0, &first.1)
            .await?;
        let second = self.transport.head_with_headers(url, retry_headers).await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second)
    }

    async fn post_with_auth_retry(
        &self,
        url: &str,
        operation: &'static str,
    ) -> Result<(StatusCode, HeaderMap), OciError> {
        let headers = self.get_auth_headers().await?;
        let first = self.transport.post(url, headers).await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let (_, retry_headers) = self
            .challenge_for_response(operation, first.0, &first.1)
            .await?;
        let second = self.transport.post(url, retry_headers).await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second)
    }

    async fn post_bytes_with_auth_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
        operation: &'static str,
    ) -> Result<StatusCode, OciError> {
        let first = self
            .transport
            .post_bytes(url, headers.clone(), body.clone())
            .await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first.0);
        }
        let (_, mut retry_headers) = self
            .challenge_for_response(operation, first.0, &first.1)
            .await?;
        for (name, value) in &headers {
            if name.as_str() != "authorization" {
                retry_headers.insert(name.clone(), value.clone());
            }
        }
        let second = self.transport.post_bytes(url, retry_headers, body).await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second.0)
    }

    async fn put_bytes_with_auth_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
        operation: &'static str,
    ) -> Result<StatusCode, OciError> {
        let first = self
            .transport
            .put_bytes_with_headers(url, headers.clone(), body.clone())
            .await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first.0);
        }
        let (_, mut retry_headers) = self
            .challenge_for_response(operation, first.0, &first.1)
            .await?;
        for (name, value) in &headers {
            if name.as_str() != "authorization" {
                retry_headers.insert(name.clone(), value.clone());
            }
        }
        let second = self
            .transport
            .put_bytes_with_headers(url, retry_headers, body)
            .await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second.0)
    }

    async fn patch_chunk_with_auth_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        chunk: Bytes,
        byte_range: (u64, u64),
        operation: &'static str,
    ) -> Result<UploadChunkResponse, OciError> {
        let first = self
            .transport
            .patch_chunk(url, headers.clone(), chunk.clone(), byte_range)
            .await?;
        if first.status != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let (_, mut retry_headers) = self
            .challenge_for_response(operation, first.status, &first.headers)
            .await?;
        for (name, value) in &headers {
            if name.as_str() != "authorization" {
                retry_headers.insert(name.clone(), value.clone());
            }
        }
        let second = self
            .transport
            .patch_chunk(url, retry_headers, chunk, byte_range)
            .await?;
        if second.status == StatusCode::UNAUTHORIZED {
            return Err(Self::auth_error(
                operation,
                second.status,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second)
    }

    fn resolved_upload_location(&self, location: &str) -> String {
        if location.starts_with('/') {
            format!("{}://{}{}", self.url_scheme(), self.registry, location)
        } else {
            location.to_string()
        }
    }

    fn update_upload_location(&self, session_url: &mut String, headers: &HeaderMap) {
        if let Some(location) = headers.get(LOCATION).and_then(|value| value.to_str().ok()) {
            *session_url = self.resolved_upload_location(location);
        }
    }

    fn retryable_status(status: StatusCode) -> bool {
        matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_EARLY | StatusCode::TOO_MANY_REQUESTS
        ) || status.is_server_error()
    }

    fn retryable_error(error: &OciError) -> bool {
        match error {
            OciError::BlobUploadFailed(status) => Self::retryable_status(*status),
            OciError::Transport(TransportError::Io(_))
            | OciError::Transport(TransportError::ConnectionFailed { .. })
            | OciError::Transport(TransportError::Timeout { .. }) => true,
            OciError::Transport(TransportError::HttpStatus { status, .. }) => {
                Self::retryable_status(*status)
            }
            _ => false,
        }
    }

    fn invalid_upload_range(details: impl Into<String>) -> OciError {
        OciError::UploadRangeInvalid {
            details: details.into(),
        }
    }

    fn response_next_offset(
        range: Option<(u64, u64)>,
        chunk_start: u64,
        request_start: u64,
        chunk_end: u64,
    ) -> Result<u64, OciError> {
        let Some((start, end)) = range else {
            return chunk_end
                .checked_add(1)
                .ok_or_else(|| Self::invalid_upload_range("chunk end offset overflowed"));
        };

        if start != 0 && start != chunk_start && start != request_start {
            return Err(Self::invalid_upload_range(format!(
                "range starts at {start}, expected 0, chunk start {chunk_start}, or request start {request_start}"
            )));
        }
        if end < request_start {
            return Err(Self::invalid_upload_range(format!(
                "range ends at {end}, before requested offset {request_start}"
            )));
        }
        if end > chunk_end {
            return Err(Self::invalid_upload_range(format!(
                "range ends at {end}, beyond chunk end {chunk_end}"
            )));
        }

        end.checked_add(1)
            .ok_or_else(|| Self::invalid_upload_range("range end offset overflowed"))
    }

    fn probe_next_offset(
        range: Option<(u64, u64)>,
        chunk_start: u64,
        current_offset: u64,
        chunk_end: u64,
    ) -> Result<Option<u64>, OciError> {
        let Some((start, end)) = range else {
            return Ok(None);
        };
        if start != 0 && start != chunk_start {
            return Err(Self::invalid_upload_range(format!(
                "probe range starts at {start}, expected 0 or chunk start {chunk_start}"
            )));
        }
        if end > chunk_end {
            return Err(Self::invalid_upload_range(format!(
                "probe range ends at {end}, beyond chunk end {chunk_end}"
            )));
        }
        let next = end
            .checked_add(1)
            .ok_or_else(|| Self::invalid_upload_range("probe range end offset overflowed"))?;
        if next < current_offset {
            return Err(Self::invalid_upload_range(format!(
                "probe moved remote offset backwards from {current_offset} to {next}"
            )));
        }
        Ok(Some(next))
    }

    async fn probe_upload_session_with_auth_retry(
        &self,
        session_url: &str,
    ) -> Result<(StatusCode, HeaderMap, Option<(u64, u64)>), OciError> {
        let (status, headers, _) = self
            .get_with_auth_retry(session_url, "probe upload session")
            .await?;
        let range = if let Some(value) = headers.get(RANGE) {
            let text = value
                .to_str()
                .map_err(|_| TransportError::HeaderParse { header: "Range" })?;
            Some(parse_range_header(text).ok_or_else(|| {
                Self::invalid_upload_range(format!("cannot parse Range header '{text}'"))
            })?)
        } else {
            None
        };
        Ok((status, headers, range))
    }

    async fn abort_upload_session(&self, session_url: &str) {
        match self
            .delete_with_auth_retry(session_url, "abort upload session")
            .await
        {
            Ok(status) if status.is_success() || status == StatusCode::NOT_FOUND => {}
            Ok(status) => warn!(
                "Best-effort upload session abort returned unexpected status {}",
                status
            ),
            Err(error) => warn!("Best-effort upload session abort failed: {error}"),
        }
    }

    async fn upload_chunk_with_retry(
        &self,
        session_url: &mut String,
        chunk: &Bytes,
        chunk_start: u64,
        config: &UploadConfig,
    ) -> Result<(), OciError> {
        let chunk_end = chunk_start
            .checked_add(chunk.len() as u64)
            .and_then(|end| end.checked_sub(1))
            .ok_or_else(|| Self::invalid_upload_range("chunk end offset overflowed"))?;
        let mut remote_next = chunk_start;
        let mut retry_count = 0usize;
        let mut patch_attempts = 0usize;
        let mut last_error: OciError;
        let mut probed_416 = false;
        let max_patch_attempts = config.max_retry_attempts.saturating_add(1);

        loop {
            if remote_next > chunk_end {
                return Ok(());
            }
            if patch_attempts >= max_patch_attempts {
                return Err(OciError::ResumableUploadFailed {
                    attempts: patch_attempts,
                    source: Box::new(Self::invalid_upload_range(
                        "retry budget exhausted before the logical chunk completed",
                    )),
                });
            }

            let relative_start = (remote_next - chunk_start) as usize;
            let body = chunk.slice(relative_start..);
            let byte_range = (remote_next, chunk_end);
            let headers = self.get_auth_headers().await?;
            patch_attempts += 1;
            let mut is_416_failure = false;

            let patch_result = self
                .patch_chunk_with_auth_retry(
                    session_url,
                    headers,
                    body,
                    byte_range,
                    "upload blob chunk",
                )
                .await;

            match patch_result {
                Ok(response) => {
                    self.update_upload_location(session_url, &response.headers);
                    if let Some(location) = response.location.as_deref() {
                        *session_url = self.resolved_upload_location(location);
                    }

                    if response.status.is_success() {
                        remote_next = Self::response_next_offset(
                            response.range,
                            chunk_start,
                            byte_range.0,
                            chunk_end,
                        )?;
                        continue;
                    }

                    let status_error = OciError::BlobUploadFailed(response.status);
                    let is_416 = response.status == StatusCode::RANGE_NOT_SATISFIABLE;
                    is_416_failure = is_416;
                    if !is_416 && !Self::retryable_error(&status_error) {
                        return Err(status_error);
                    }
                    last_error = status_error;

                    if is_416 {
                        if probed_416 {
                            return Err(last_error);
                        }
                        probed_416 = true;
                    } else if retry_count >= config.max_retry_attempts {
                        return Err(OciError::ResumableUploadFailed {
                            attempts: patch_attempts,
                            source: Box::new(last_error),
                        });
                    }
                }
                Err(error) => {
                    if !Self::retryable_error(&error) {
                        return Err(error);
                    }
                    last_error = error;
                    if retry_count >= config.max_retry_attempts {
                        return Err(OciError::ResumableUploadFailed {
                            attempts: patch_attempts,
                            source: Box::new(last_error),
                        });
                    }
                }
            }

            let (probe_status, probe_headers, probe_range) = self
                .probe_upload_session_with_auth_retry(session_url)
                .await
                .map_err(|probe_error| OciError::ResumableUploadFailed {
                    attempts: patch_attempts,
                    source: Box::new(probe_error),
                })?;
            self.update_upload_location(session_url, &probe_headers);
            if !probe_status.is_success() {
                if is_416_failure {
                    return Err(last_error);
                }
                return Err(OciError::ResumableUploadFailed {
                    attempts: patch_attempts,
                    source: Box::new(OciError::BlobUploadFailed(probe_status)),
                });
            }

            if is_416_failure && probe_range.is_none() {
                return Err(last_error);
            }

            if let Some(next) =
                Self::probe_next_offset(probe_range, chunk_start, remote_next, chunk_end)?
            {
                remote_next = next;
                if remote_next > chunk_end {
                    return Ok(());
                }
            }

            if retry_count >= config.max_retry_attempts {
                return Err(OciError::ResumableUploadFailed {
                    attempts: patch_attempts,
                    source: Box::new(last_error),
                });
            }

            retry_count += 1;
            let backoff_exponent = retry_count.saturating_sub(1).min(5) as u32;
            let backoff_ms = 100u64.saturating_mul(1u64 << backoff_exponent);
            self.transport()
                .sleep(Duration::from_millis(backoff_ms))
                .await;
        }
    }

    async fn put_chunk_finish_with_auth_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
        operation: &'static str,
    ) -> Result<StatusCode, OciError> {
        let first = self
            .transport
            .put_chunk_finish_with_headers(url, headers.clone(), final_chunk.clone())
            .await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first.0);
        }
        let (_, mut retry_headers) = self
            .challenge_for_response(operation, first.0, &first.1)
            .await?;
        for (name, value) in &headers {
            if name.as_str() != "authorization" {
                retry_headers.insert(name.clone(), value.clone());
            }
        }
        let second = self
            .transport
            .put_chunk_finish_with_headers(url, retry_headers, final_chunk)
            .await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second.0)
    }

    async fn delete_with_auth_retry(
        &self,
        url: &str,
        operation: &'static str,
    ) -> Result<StatusCode, OciError> {
        let headers = self.get_auth_headers().await?;
        let first = self.transport.delete_with_headers(url, headers).await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first.0);
        }
        let (_, retry_headers) = self
            .challenge_for_response(operation, first.0, &first.1)
            .await?;
        let second = self
            .transport
            .delete_with_headers(url, retry_headers)
            .await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second.0)
    }

    pub async fn head_blob(&self, digest: &str) -> Result<bool, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let (status, _response_headers) = self.head_with_auth_retry(&url, "head blob").await?;

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

        let (status, resp_headers) = self
            .post_with_auth_retry(&upload_init_url, "initialize blob upload")
            .await?;

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
            .put_bytes_with_auth_retry(&put_url, headers, bytes, "upload blob")
            .await?;

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
                    .post_bytes_with_auth_retry(
                        &monolithic_url,
                        headers,
                        bytes.clone(),
                        "monolithic blob upload",
                    )
                    .await
                {
                    Ok(status) if status == StatusCode::CREATED || status == StatusCode::OK => {
                        info!(
                            "Successfully uploaded blob via 1-RTT Monolithic POST: {}",
                            digest
                        );
                        Ok(digest.to_string())
                    }
                    Ok(status) => {
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

        let (status, resp_headers) = self
            .post_with_auth_retry(&upload_init_url, "initialize blob stream upload")
            .await?;

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

        let (put_status, _put_headers) = self
            .transport
            .put_stream_with_headers(&put_url, headers, stream, content_len)
            .await?;

        if put_status == StatusCode::UNAUTHORIZED {
            return Err(OciError::AuthenticationNotReplayable {
                operation: "upload blob stream",
            });
        }
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

        let (status, resp_headers) = self
            .post_with_auth_retry(&upload_init_url, "initialize chunked upload")
            .await?;
        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }

        let location = resp_headers
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .ok_or(OciError::UploadLocationMissing)?;

        let mut session_url = self.resolved_upload_location(location);
        let mut current_offset = 0u64;
        let mut chunk_buf = buffer;
        let mut stream_ended = false;

        let upload_result = async {
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
                let chunk_bytes = chunk_buf.split_to(send_len).freeze();
                self.upload_chunk_with_retry(
                    &mut session_url,
                    &chunk_bytes,
                    current_offset,
                    config,
                )
                .await?;
                current_offset += chunk_bytes.len() as u64;
            }

            let final_digest = hash_state.force_finalize();
            let total_size = hash_state.bytes_streamed();
            let separator = if session_url.contains('?') { "&" } else { "?" };
            let finish_url = format!("{}{}digest={}", session_url, separator, final_digest);
            let headers = self.get_auth_headers().await?;
            let finish_status = self
                .put_chunk_finish_with_auth_retry(&finish_url, headers, None, "finish blob upload")
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
        .await;

        if upload_result.is_err() {
            self.abort_upload_session(&session_url).await;
        }
        upload_result
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

        let (status, resp_headers, bytes) = self.get_with_auth_retry(&url, "get manifest").await?;

        if status == StatusCode::OK {
            let digest_header = resp_headers
                .get("Docker-Content-Digest")
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_string)
                        .map_err(|_| TransportError::HeaderParse {
                            header: "Docker-Content-Digest",
                        })
                })
                .transpose()?;

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

    /// 轻量级探测远端 Manifest 是否存在并获取其摘要 (HEAD 请求)
    pub async fn head_manifest(&self, tag: &str) -> Result<Option<String>, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            tag
        );

        let (status, resp_headers) = self.head_with_auth_retry(&url, "head manifest").await?;

        if status == StatusCode::OK {
            let digest = resp_headers
                .get("Docker-Content-Digest")
                .or_else(|| resp_headers.get("ETag"))
                .and_then(|v| v.to_str().ok())
                .map(|s| {
                    s.trim_matches('"')
                        .trim_start_matches("W/\"")
                        .trim_end_matches('"')
                        .to_string()
                });
            Ok(digest)
        } else if status == StatusCode::NOT_FOUND {
            Ok(None)
        } else {
            Err(OciError::ManifestFetchFailed(status))
        }
    }

    /// 列出仓库的所有 Tags (标准 OCI Distribution GET /v2/<name>/tags/list 路由)
    pub async fn list_tags(&self) -> Result<Vec<String>, OciError> {
        let mut all_tags = Vec::new();
        let mut last_tag: Option<String> = None;

        loop {
            let mut url = format!(
                "{}://{}/v2/{}/nix-cache/tags/list?n=100",
                self.url_scheme(),
                self.registry,
                self.repo
            );
            if let Some(ref last) = last_tag {
                url.push_str("&last=");
                url.push_str(&encode_query_value(last));
            }

            let (status, _resp_headers, body) = self.get_with_auth_retry(&url, "list tags").await?;

            if status == StatusCode::NOT_FOUND {
                if !all_tags.is_empty() {
                    return Err(OciError::DeletionDiscoveryFailed {
                        stage: "list_tags",
                        target: self.repo.clone(),
                        details: "registry returned 404 after a partial tag listing".to_string(),
                    });
                }
                // 仅将首页 404 解释为不存在仓库或空仓库。
                all_tags.sort();
                all_tags.dedup();
                return Ok(all_tags);
            }

            if !status.is_success() {
                // 部分 Registry 可能不支持带 query params 的 tags/list，回退到无参 URL 尝试
                if last_tag.is_none() {
                    let plain_url = format!(
                        "{}://{}/v2/{}/nix-cache/tags/list",
                        self.url_scheme(),
                        self.registry,
                        self.repo
                    );
                    let (plain_status, _, plain_body) = self
                        .get_with_auth_retry(&plain_url, "list tags fallback")
                        .await?;
                    if plain_status.is_success() {
                        let resp = serde_json::from_slice::<OciTagsListResponse>(&plain_body)
                            .map_err(OciError::Json)?;
                        all_tags.extend(resp.tags);
                        all_tags.sort();
                        all_tags.dedup();
                        return Ok(all_tags);
                    }
                }
                return Err(OciError::Transport(TransportError::HttpStatus {
                    status,
                    message: Some(format!("Failed to list tags from {}", url)),
                }));
            }

            let parsed: OciTagsListResponse = match serde_json::from_slice(&body) {
                Ok(p) => p,
                Err(e) => return Err(OciError::Json(e)),
            };

            if parsed.tags.is_empty() {
                break;
            }

            let count = parsed.tags.len();
            let new_last = parsed.tags.last().cloned();
            all_tags.extend(parsed.tags);

            if count < 100 {
                break;
            }
            if new_last == last_tag || new_last.is_none() {
                return Err(OciError::DeletionDiscoveryFailed {
                    stage: "list_tags",
                    target: self.repo.clone(),
                    details: "registry returned a non-advancing pagination cursor".to_string(),
                });
            }
            last_tag = new_last;
        }

        if all_tags.is_empty()
            && self.capabilities().deletion_strategy
                == RegistryDeletionStrategy::GitHubPackagesRestApi
        {
            let versions = self.ghcr_client().list_package_versions().await?;
            for v in versions {
                if let Some(meta) = v.metadata
                    && let Some(container) = meta.container
                {
                    all_tags.extend(container.tags);
                }
            }
            all_tags.sort();
            all_tags.dedup();
        }

        all_tags.sort();
        all_tags.dedup();
        Ok(all_tags)
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

    /// 针对特定系统架构，按需拉取单架构 Schema v6 分片根索引目录数据
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
                    CacheLayerMediaType::parse(&l.media_type).is_some_and(|m| m.is_root_index())
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
                        CacheLayerMediaType::parse(&l.media_type).is_some_and(|m| m.is_root_index())
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
                        CacheLayerMediaType::parse(&l.media_type).is_some_and(|m| m.is_root_index())
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

    /// 推送单架构 Schema v6 分片根索引目录清单 (内置 1024 分片描述符，彻底废除全局 Bloom Filter)
    pub async fn push_sharded_root_index(
        &self,
        tag: &str,
        root_data: &ShardedArchCacheIndexData,
    ) -> Result<String, OciError> {
        let (root_blob_digest, root_compressed_size, _) = self.push_zstd_blob(root_data).await?;

        let manifest = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
            root_blob_digest: &root_blob_digest,
            root_blob_size: root_compressed_size,
            config_digest: EMPTY_CONFIG_DIGEST,
            config_size: EMPTY_CONFIG_SIZE,
            system: &root_data.system,
            merkle_root: &root_data.merkle_root,
        });

        let manifest_str = manifest.to_json_string()?;
        self.put_manifest(tag, &manifest_str).await?;
        let manifest_digest = compute_sha256_digest(manifest_str.as_bytes());
        Ok(manifest_digest)
    }

    /// 推送单架构分片根索引目录清单，并要求后端执行 manifest CAS。
    pub async fn push_sharded_root_index_cas(
        &self,
        tag: &str,
        root_data: &ShardedArchCacheIndexData,
        condition: ManifestCasCondition,
    ) -> Result<String, OciError> {
        self.ensure_manifest_cas_supported(tag)?;
        let (root_blob_digest, root_compressed_size, _) = self.push_zstd_blob(root_data).await?;

        let manifest = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
            root_blob_digest: &root_blob_digest,
            root_blob_size: root_compressed_size,
            config_digest: EMPTY_CONFIG_DIGEST,
            config_size: EMPTY_CONFIG_SIZE,
            system: &root_data.system,
            merkle_root: &root_data.merkle_root,
        });

        let manifest_str = manifest.to_json_string()?;
        self.put_manifest_cas(tag, &manifest_str, condition).await?;
        let manifest_digest = compute_sha256_digest(manifest_str.as_bytes());
        Ok(manifest_digest)
    }

    /// 下载并解压指定 Blob Digest 的单分片数据 Payload
    pub async fn get_shard_data(&self, blob_digest: &str) -> Result<ShardDataPayload, OciError> {
        let blob_bytes = self.get_blob(blob_digest).await?;
        IndexCodec::decode_zstd(&blob_bytes, CacheLayerMediaTypeV6::SHARD_DATA_V6_ZSTD)
    }

    /// 压缩并推送单分片数据 Payload
    pub async fn push_shard_data(
        &self,
        payload: &ShardDataPayload,
    ) -> Result<(String, u64, u64), OciError> {
        self.push_zstd_blob(payload).await
    }

    fn ensure_manifest_cas_supported(&self, tag: &str) -> Result<(), OciError> {
        if self.driver.capabilities().manifest_cas_support == ManifestCasSupport::IfMatch {
            Ok(())
        } else {
            Err(OciError::CasUnsupported {
                tag: tag.to_string(),
                backend: self.kind(),
            })
        }
    }

    fn insert_manifest_cas_condition(
        headers: &mut HeaderMap,
        condition: &ManifestCasCondition,
    ) -> Result<Option<String>, OciError> {
        match condition {
            ManifestCasCondition::CreateOnly => {
                headers.insert("If-None-Match", HeaderValue::from_static("*"));
                Ok(None)
            }
            ManifestCasCondition::Match(expected) => {
                let value = HeaderValue::from_str(expected).map_err(|_| {
                    OciError::Transport(TransportError::HeaderParse { header: "If-Match" })
                })?;
                headers.insert(IF_MATCH, value);
                Ok(Some(expected.clone()))
            }
        }
    }

    pub async fn put_manifest(&self, tag: &str, manifest: &str) -> Result<(), OciError> {
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

        let bytes = Bytes::copy_from_slice(manifest.as_bytes());
        let status = self
            .put_bytes_with_auth_retry(&url, headers, bytes, "push manifest")
            .await?;

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

    pub async fn put_manifest_cas(
        &self,
        tag: &str,
        manifest: &str,
        condition: ManifestCasCondition,
    ) -> Result<(), OciError> {
        self.ensure_manifest_cas_supported(tag)?;
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
        let expected = Self::insert_manifest_cas_condition(&mut headers, &condition)?;

        let bytes = Bytes::copy_from_slice(manifest.as_bytes());
        let status = self
            .put_bytes_with_auth_retry(&url, headers, bytes, "push manifest with CAS")
            .await?;

        if status == StatusCode::PRECONDITION_FAILED || status == StatusCode::CONFLICT {
            return Err(OciError::CasPreconditionFailed {
                tag: tag.to_string(),
                expected,
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

    pub async fn put_image_index(&self, tag: &str, index: &OciImageIndex) -> Result<(), OciError> {
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

        let index_json = index.to_json_string()?;
        let bytes = Bytes::copy_from_slice(index_json.as_bytes());
        let status = self
            .put_bytes_with_auth_retry(&url, headers, bytes, "push image index")
            .await?;

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

    pub async fn put_image_index_cas(
        &self,
        tag: &str,
        index: &OciImageIndex,
        condition: ManifestCasCondition,
    ) -> Result<(), OciError> {
        self.ensure_manifest_cas_supported(tag)?;

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
        let expected = Self::insert_manifest_cas_condition(&mut headers, &condition)?;

        let index_json = index.to_json_string()?;
        let bytes = Bytes::copy_from_slice(index_json.as_bytes());
        let status = self
            .put_bytes_with_auth_retry(&url, headers, bytes, "push image index with CAS")
            .await?;

        if status == StatusCode::PRECONDITION_FAILED || status == StatusCode::CONFLICT {
            return Err(OciError::CasPreconditionFailed {
                tag: tag.to_string(),
                expected,
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
        self.put_image_index(tag, index).await
    }

    pub async fn push_manifest(&self, tag: &str, manifest: &str) -> Result<(), OciError> {
        self.put_manifest(tag, manifest).await
    }

    pub fn ghcr_client(&self) -> GitHubPackagesClient<T> {
        GitHubPackagesClient::new(
            self.transport.clone(),
            self.token_manager.auth_token(),
            &self.repo,
        )
    }

    fn valid_digest(digest: &str) -> bool {
        let Some(hex) = digest.strip_prefix("sha256:") else {
            return false;
        };
        hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn discovery_error(
        stage: &'static str,
        target: impl Into<String>,
        error: impl ToString,
    ) -> OciError {
        OciError::DeletionDiscoveryFailed {
            stage,
            target: target.into(),
            details: error.to_string(),
        }
    }

    fn parse_discovered_manifest(
        body: &str,
        target: &str,
    ) -> Result<OciArtifactManifest, OciError> {
        let value: serde_json::Value = serde_json::from_str(body)
            .map_err(|error| Self::discovery_error("manifest_json", target, error))?;
        let schema_version = value
            .get("schemaVersion")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                Self::discovery_error("manifest_json", target, "missing schemaVersion")
            })?;
        if schema_version != 2 {
            return Err(Self::discovery_error(
                "manifest_json",
                target,
                "unsupported OCI schemaVersion",
            ));
        }
        let media_type = value
            .get("mediaType")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Self::discovery_error("manifest_json", target, "missing mediaType"))?;
        if !matches!(
            media_type,
            OCI_IMAGE_INDEX_MEDIA_TYPE | OCI_IMAGE_MANIFEST_MEDIA_TYPE
        ) {
            return Err(Self::discovery_error(
                "manifest_json",
                target,
                format!("unsupported OCI manifest media type '{media_type}'"),
            ));
        }
        serde_json::from_value(value)
            .map_err(|error| Self::discovery_error("manifest_json", target, error))
    }

    async fn discover_tag_reachable_graph(&self) -> Result<DeletionPlan, OciError> {
        let mut tags = self.list_tags().await?;
        tags.sort();
        tags.dedup();

        let mut plan = DeletionPlan {
            tags,
            manifests: HashMap::new(),
            blobs: HashMap::new(),
        };
        let mut pending = VecDeque::new();
        let mut queued_manifests = HashSet::new();
        let mut decoded_shard_blobs = HashSet::new();

        for tag in &plan.tags {
            let Some((body, digest)) = self
                .get_manifest_with_digest(tag)
                .await
                .map_err(|error| Self::discovery_error("manifest_get", tag, error))?
            else {
                // Tag lists and manifests can race. A tag which disappeared is
                // not evidence that the repository was empty.
                continue;
            };
            if !Self::valid_digest(&digest) {
                return Err(Self::discovery_error(
                    "manifest_digest",
                    tag,
                    "registry returned an invalid Docker-Content-Digest",
                ));
            }
            let computed = compute_sha256_digest(body.as_bytes());
            if computed != digest {
                return Err(Self::discovery_error(
                    "manifest_digest",
                    tag,
                    format!("digest header/body mismatch: header {digest}, body {computed}"),
                ));
            }
            if queued_manifests.contains(&digest) {
                continue;
            }
            if queued_manifests.len() >= MAX_DELETION_MANIFESTS {
                return Err(OciError::DeletionObjectLimitExceeded {
                    target: self.repo.clone(),
                });
            }
            queued_manifests.insert(digest.clone());
            pending.push_back((digest, body, tag.clone()));
        }

        while let Some((digest, body, source)) = pending.pop_front() {
            if plan.manifests.contains_key(&digest) {
                continue;
            }
            if plan.manifests.len() >= MAX_DELETION_MANIFESTS {
                return Err(OciError::DeletionObjectLimitExceeded {
                    target: self.repo.clone(),
                });
            }
            let computed = compute_sha256_digest(body.as_bytes());
            if computed != digest {
                return Err(Self::discovery_error(
                    "manifest_digest",
                    &digest,
                    format!("digest/body mismatch while traversing tag {source}"),
                ));
            }
            let artifact = Self::parse_discovered_manifest(&body, &digest)?;
            plan.manifests.insert(digest.clone(), body);

            match artifact {
                OciArtifactManifest::Index(index) => {
                    for descriptor in index.manifests {
                        if !Self::valid_digest(&descriptor.digest) {
                            return Err(Self::discovery_error(
                                "manifest_descriptor",
                                &digest,
                                "index contains an invalid manifest digest",
                            ));
                        }
                        if !matches!(
                            descriptor.media_type.as_str(),
                            OCI_IMAGE_INDEX_MEDIA_TYPE | OCI_IMAGE_MANIFEST_MEDIA_TYPE
                        ) {
                            return Err(Self::discovery_error(
                                "manifest_descriptor",
                                &digest,
                                format!(
                                    "unsupported child manifest media type '{}'",
                                    descriptor.media_type
                                ),
                            ));
                        }
                        if queued_manifests.contains(&descriptor.digest) {
                            continue;
                        }
                        if queued_manifests.len() >= MAX_DELETION_MANIFESTS {
                            return Err(OciError::DeletionObjectLimitExceeded {
                                target: descriptor.digest,
                            });
                        }
                        queued_manifests.insert(descriptor.digest.clone());
                        let Some((child_body, child_digest)) = self
                            .get_manifest_with_digest(&descriptor.digest)
                            .await
                            .map_err(|error| {
                                Self::discovery_error("manifest_get", &descriptor.digest, error)
                            })?
                        else {
                            return Err(Self::discovery_error(
                                "manifest_get",
                                &descriptor.digest,
                                "child manifest disappeared during discovery",
                            ));
                        };
                        if child_digest != descriptor.digest
                            || compute_sha256_digest(child_body.as_bytes()) != descriptor.digest
                        {
                            return Err(Self::discovery_error(
                                "manifest_digest",
                                &descriptor.digest,
                                "child manifest digest does not match descriptor",
                            ));
                        }
                        pending.push_back((descriptor.digest, child_body, source.clone()));
                    }
                }
                OciArtifactManifest::Manifest(manifest) => {
                    if !Self::valid_digest(&manifest.config.digest) {
                        return Err(Self::discovery_error(
                            "blob_descriptor",
                            &digest,
                            "manifest contains an invalid config digest",
                        ));
                    }
                    if plan.blobs.len() >= MAX_DELETION_BLOBS
                        && !plan.blobs.contains_key(&manifest.config.digest)
                    {
                        return Err(OciError::DeletionObjectLimitExceeded {
                            target: self.repo.clone(),
                        });
                    }
                    plan.blobs
                        .entry(manifest.config.digest.clone())
                        .or_insert(manifest.config.size);

                    for layer in manifest.layers {
                        if !Self::valid_digest(&layer.digest) {
                            return Err(Self::discovery_error(
                                "blob_descriptor",
                                &digest,
                                "manifest contains an invalid layer digest",
                            ));
                        }
                        if plan.blobs.len() >= MAX_DELETION_BLOBS
                            && !plan.blobs.contains_key(&layer.digest)
                        {
                            return Err(OciError::DeletionObjectLimitExceeded {
                                target: self.repo.clone(),
                            });
                        }
                        plan.blobs.entry(layer.digest.clone()).or_insert(layer.size);

                        let Some(layer_type) = CacheLayerMediaType::parse(&layer.media_type) else {
                            continue;
                        };
                        let layer_bytes = self.get_blob(&layer.digest).await.map_err(|error| {
                            Self::discovery_error("cache_layer_get", &layer.digest, error)
                        })?;
                        let computed_layer_digest = compute_sha256_digest(&layer_bytes);
                        if computed_layer_digest != layer.digest {
                            return Err(Self::discovery_error(
                                "cache_layer_digest",
                                &layer.digest,
                                format!(
                                    "cache layer body digest mismatch: expected {}, got {}",
                                    layer.digest, computed_layer_digest
                                ),
                            ));
                        }
                        if layer_type.is_root_index() {
                            let root: ShardedArchCacheIndexData = IndexCodec::decode_zstd(
                                &layer_bytes,
                                &layer.media_type,
                            )
                            .map_err(|error| {
                                Self::discovery_error("root_index_decode", &layer.digest, error)
                            })?;
                            for shard in root.shards {
                                if shard.entry_count == 0 {
                                    continue;
                                }
                                if shard.blob_digest.is_empty() {
                                    return Err(Self::discovery_error(
                                        "root_index_decode",
                                        &layer.digest,
                                        "non-empty root shard is missing its blob digest",
                                    ));
                                }
                                if !Self::valid_digest(&shard.blob_digest) {
                                    return Err(Self::discovery_error(
                                        "root_index_decode",
                                        &layer.digest,
                                        "root index contains an invalid shard digest",
                                    ));
                                }
                                if plan.blobs.len() >= MAX_DELETION_BLOBS
                                    && !plan.blobs.contains_key(&shard.blob_digest)
                                {
                                    return Err(OciError::DeletionObjectLimitExceeded {
                                        target: self.repo.clone(),
                                    });
                                }
                                let shard_digest = shard.blob_digest.clone();
                                plan.blobs
                                    .entry(shard_digest.clone())
                                    .or_insert(shard.compressed_size);

                                if decoded_shard_blobs.insert(shard_digest.clone()) {
                                    let shard_bytes =
                                        self.get_blob(&shard_digest).await.map_err(|error| {
                                            Self::discovery_error("shard_get", &shard_digest, error)
                                        })?;
                                    let computed_shard_digest = compute_sha256_digest(&shard_bytes);
                                    if computed_shard_digest != shard_digest {
                                        return Err(Self::discovery_error(
                                            "shard_digest",
                                            &shard_digest,
                                            format!(
                                                "shard body digest mismatch: expected {}, got {}",
                                                shard_digest, computed_shard_digest
                                            ),
                                        ));
                                    }
                                    let shard_data: ShardDataPayload = IndexCodec::decode_zstd(
                                        &shard_bytes,
                                        CacheLayerMediaTypeV6::SHARD_DATA_V6_ZSTD,
                                    )
                                    .map_err(|error| {
                                        Self::discovery_error("shard_decode", &shard_digest, error)
                                    })?;
                                    for entry in shard_data.entries.values() {
                                        let nar_digest = entry.nar_digest.to_string();
                                        if !Self::valid_digest(&nar_digest) {
                                            return Err(Self::discovery_error(
                                                "shard_decode",
                                                &shard_digest,
                                                "shard contains an invalid NAR digest",
                                            ));
                                        }
                                        if plan.blobs.len() >= MAX_DELETION_BLOBS
                                            && !plan.blobs.contains_key(&nar_digest)
                                        {
                                            return Err(OciError::DeletionObjectLimitExceeded {
                                                target: self.repo.clone(),
                                            });
                                        }
                                        plan.blobs.entry(nar_digest).or_insert(entry.nar_size);
                                    }
                                }
                            }
                        } else {
                            let shard: ShardDataPayload =
                                IndexCodec::decode_zstd(&layer_bytes, &layer.media_type).map_err(
                                    |error| {
                                        Self::discovery_error("shard_decode", &layer.digest, error)
                                    },
                                )?;
                            for entry in shard.entries.values() {
                                let nar_digest = entry.nar_digest.to_string();
                                if !Self::valid_digest(&nar_digest) {
                                    return Err(Self::discovery_error(
                                        "shard_decode",
                                        &layer.digest,
                                        "shard contains an invalid NAR digest",
                                    ));
                                }
                                if plan.blobs.len() >= MAX_DELETION_BLOBS
                                    && !plan.blobs.contains_key(&nar_digest)
                                {
                                    return Err(OciError::DeletionObjectLimitExceeded {
                                        target: self.repo.clone(),
                                    });
                                }
                                plan.blobs.entry(nar_digest).or_insert(entry.nar_size);
                            }
                        }
                    }
                }
            }
        }

        Ok(plan)
    }

    /// 只读发现当前所有 tag 可达的对象图，供 dry-run 和审计使用。
    pub async fn preview_tag_reachable_package(&self) -> Result<PackageDeletionSummary, OciError> {
        Ok(self.discover_tag_reachable_graph().await?.summary())
    }

    async fn verify_manifest_absent(&self, digest: &str) -> Result<(), OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );
        let (status, _) = self
            .head_with_auth_retry(&url, "verify manifest deletion")
            .await?;
        if status == StatusCode::NOT_FOUND {
            Ok(())
        } else if status == StatusCode::OK {
            Err(OciError::DeletionVerificationFailed {
                target: digest.to_string(),
                details: "manifest is still present after DELETE".to_string(),
            })
        } else {
            Err(OciError::DeletionVerificationFailed {
                target: digest.to_string(),
                details: format!("manifest verification returned HTTP {status}"),
            })
        }
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
                let (status, resp_headers, body) = self
                    .get_with_auth_retry(&head_url, "get tag manifest")
                    .await?;

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
                let tag_status = self.delete_with_auth_retry(&tag_url, "delete tag").await?;
                if tag_status != StatusCode::NOT_FOUND
                    && !tag_status.is_success()
                    && tag_status != StatusCode::ACCEPTED
                {
                    return Err(OciError::DeletionFailed {
                        target: tag.to_string(),
                        status: tag_status,
                        details: "registry rejected tag deletion".to_string(),
                    });
                }

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
    pub async fn delete_manifest_strict(&self, digest: &str) -> Result<DeletionOutcome, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let status = self.delete_with_auth_retry(&url, "delete manifest").await?;

        if status.is_success() || status == StatusCode::ACCEPTED || status == StatusCode::NO_CONTENT
        {
            Ok(DeletionOutcome::Deleted)
        } else if status == StatusCode::NOT_FOUND {
            Ok(DeletionOutcome::AlreadyAbsent)
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
    pub async fn delete_blob_strict(&self, digest: &str) -> Result<DeletionOutcome, OciError> {
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

        let status = self.delete_with_auth_retry(&url, "delete blob").await?;

        if status.is_success() || status == StatusCode::ACCEPTED || status == StatusCode::NO_CONTENT
        {
            Ok(DeletionOutcome::Deleted)
        } else if status == StatusCode::NOT_FOUND {
            Ok(DeletionOutcome::AlreadyAbsent)
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
                Ok(DeletionOutcome::Deleted) => summary.deleted_count += 1,
                Ok(DeletionOutcome::AlreadyAbsent) => summary.not_found_count += 1,
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

    /// 严格删除远程 Package，成功时返回可审计的发现、删除和幂等计数。
    pub async fn delete_entire_package_strict(&self) -> Result<PackageDeletionSummary, OciError> {
        match self.capabilities().package_deletion_support {
            PackageDeletionSupport::NativeComplete => {
                self.ghcr_client().delete_entire_package().await?;
                Ok(PackageDeletionSummary::default())
            }
            PackageDeletionSupport::TaggedGraphOnly => {
                let plan = self.discover_tag_reachable_graph().await?;
                let mut summary = plan.summary();

                let mut manifests: Vec<_> = plan.manifests.keys().cloned().collect();
                manifests.sort();
                for digest in manifests {
                    match self.delete_manifest_strict(&digest).await? {
                        DeletionOutcome::Deleted => summary.manifests_deleted += 1,
                        DeletionOutcome::AlreadyAbsent => summary.already_absent += 1,
                    }
                }

                let mut blobs: Vec<_> = plan.blobs.keys().cloned().collect();
                blobs.sort();
                for digest in blobs {
                    match self.delete_blob_strict(&digest).await? {
                        DeletionOutcome::Deleted => summary.blobs_deleted += 1,
                        DeletionOutcome::AlreadyAbsent => summary.already_absent += 1,
                    }
                }

                let remaining_tags = self
                    .list_tags()
                    .await
                    .map_err(|error| Self::discovery_error("final_list_tags", &self.repo, error))?;
                if !remaining_tags.is_empty() {
                    return Err(OciError::DeletionVerificationFailed {
                        target: self.repo.clone(),
                        details: format!(
                            "{} tag(s) remain after deleting the discovered graph",
                            remaining_tags.len()
                        ),
                    });
                }

                for digest in plan.manifests.keys() {
                    self.verify_manifest_absent(digest).await?;
                }
                for digest in plan.blobs.keys() {
                    let present = self.head_blob(digest).await?;
                    if present {
                        return Err(OciError::DeletionVerificationFailed {
                            target: digest.clone(),
                            details: "blob is still present after DELETE".to_string(),
                        });
                    }
                }

                Ok(summary)
            }
            PackageDeletionSupport::Unsupported => Err(OciError::OperationNotSupported {
                operation: "delete_package",
                backend: self.kind(),
                reason: format!(
                    "Registry backend '{}' cannot prove complete package deletion",
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
        self.ensure_manifest_cas_supported(tag)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (existing_index, condition) = match self.fetch_artifact(tag).await? {
                Some(artifact) => (
                    artifact.manifest.as_index().cloned(),
                    ManifestCasCondition::Match(artifact.digest),
                ),
                None => (None, ManifestCasCondition::CreateOnly),
            };

            let updated_index = mutator(existing_index)?;
            match self
                .put_image_index_cas(tag, &updated_index, condition)
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

    /// 在调用方已提供外部互斥时更新 Image Index。
    pub async fn update_image_index_single_writer<F>(
        &self,
        tag: &str,
        mut mutator: F,
    ) -> Result<(), OciError>
    where
        F: FnMut(Option<OciImageIndex>) -> Result<OciImageIndex, OciError>,
    {
        let existing_index = self
            .fetch_artifact(tag)
            .await?
            .and_then(|artifact| artifact.manifest.as_index().cloned());
        let updated_index = mutator(existing_index)?;
        self.put_image_index(tag, &updated_index).await
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
        F: FnMut(Option<ShardedArchCacheIndexData>) -> Result<ShardedArchCacheIndexData, OciError>,
    {
        self.ensure_manifest_cas_supported(tag)?;
        let mut attempt = 0;
        let arch_tag = if tag.ends_with(system.as_str()) {
            tag.to_string()
        } else {
            format!("{}-{}", tag, system.as_str())
        };

        loop {
            attempt += 1;
            let (existing_root, condition) =
                match self.get_sharded_root_index(&arch_tag, system).await? {
                    Some((data, digest)) => (Some(data), ManifestCasCondition::Match(digest)),
                    None => (None, ManifestCasCondition::CreateOnly),
                };

            let updated_root = mutator(existing_root)?;

            match self
                .push_sharded_root_index_cas(&arch_tag, &updated_root, condition)
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

    /// 在调用方已提供外部互斥时更新单架构分片根索引。
    pub async fn update_sharded_arch_index_single_writer<F>(
        &self,
        tag: &str,
        system: &SystemArch,
        mut mutator: F,
    ) -> Result<String, OciError>
    where
        F: FnMut(Option<ShardedArchCacheIndexData>) -> Result<ShardedArchCacheIndexData, OciError>,
    {
        let arch_tag = if tag.ends_with(system.as_str()) {
            tag.to_string()
        } else {
            format!("{}-{}", tag, system.as_str())
        };
        let existing_root = self
            .get_sharded_root_index(&arch_tag, system)
            .await?
            .map(|(data, _)| data);
        let updated_root = mutator(existing_root)?;
        self.push_sharded_root_index(&arch_tag, &updated_root).await
    }

    pub async fn get_blob(&self, digest: &str) -> Result<Bytes, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let (status, _resp_headers, bytes) = self.get_with_auth_retry(&url, "get blob").await?;

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
        let first = self.transport.stream(&url, headers).await?;
        let (status, resp_headers, stream) = if first.0 == StatusCode::UNAUTHORIZED {
            let (_, retry_headers) = self
                .challenge_for_response("stream blob", first.0, &first.1)
                .await?;
            let second = self.transport.stream(&url, retry_headers).await?;
            if second.0 == StatusCode::UNAUTHORIZED {
                return Err(Self::auth_error(
                    "stream blob",
                    second.0,
                    "Bearer challenge retry was rejected",
                ));
            }
            second
        } else {
            first
        };

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
