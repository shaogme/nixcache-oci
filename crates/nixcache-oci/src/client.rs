use crate::{
    error::OciError,
    manifest::{
        OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciImageManifest, build_index_manifest,
        build_session_manifest,
    },
    token::TokenManager,
};
use chrono::Utc;
use nixcache_core::{
    CacheIndexData, IndexEntry, JobSummaryMetadata, RUN_SESSION_VERSION, RunSessionManifest,
    StoreHash, SystemArch,
};
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue, IF_MATCH},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::{fs::File, io::AsyncReadExt, time::sleep};
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct OciClient {
    registry: String,
    repo: String,
    token_manager: TokenManager,
    client: Client,
}

impl OciClient {
    pub fn new(registry: &str, repo: &str, github_token: &str, write_access: bool) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| Client::new());

        let token_manager = TokenManager::new(registry, repo, github_token, write_access);

        Self {
            registry: registry.to_string(),
            repo: repo.to_string(),
            token_manager,
            client,
        }
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
        self.token_manager.get_token(&self.client).await
    }

    async fn get_auth_headers(&self) -> Result<HeaderMap, OciError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Accept",
            HeaderValue::from_static(OCI_IMAGE_MANIFEST_MEDIA_TYPE),
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
        let resp = self.client.head(&url).headers(headers).send().await?;

        if resp.status() == StatusCode::OK {
            Ok(true)
        } else if resp.status() == StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            warn!(
                "Unexpected status when checking blob {}: HTTP {}",
                digest,
                resp.status()
            );
            Ok(false)
        }
    }

    pub async fn push_blob(&self, file_path: &Path) -> Result<String, OciError> {
        let mut file = File::open(file_path).await?;
        let mut hasher = Sha256::default();
        let mut buffer = vec![0; 64 * 1024];
        let mut size = 0u64;

        loop {
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            size += n as u64;
        }

        let hash_result = hasher.finalize();
        let digest = format!(
            "sha256:{}",
            hash_result
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        );

        // Check if blob already exists
        if self.head_blob(&digest).await? {
            info!("Blob {} already exists, skipping upload.", digest);
            return Ok(digest);
        }

        info!("Initiating upload for blob {}", digest);
        let upload_init_url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/uploads/",
            self.url_scheme(),
            self.registry,
            self.repo
        );

        let headers = self.get_auth_headers().await?;
        let resp = self
            .client
            .post(&upload_init_url)
            .headers(headers)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(OciError::UploadFailed(status));
        }

        let location = resp
            .headers()
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

        // Upload file contents
        let file_stream = File::open(file_path).await?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file_stream));

        let mut headers = self.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert("Content-Length", HeaderValue::from(size));

        let put_resp = self
            .client
            .put(&put_url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        let put_status = put_resp.status();
        if put_status == StatusCode::CREATED || put_status == StatusCode::ACCEPTED {
            info!("Successfully uploaded blob: {}", digest);
            Ok(digest)
        } else {
            Err(OciError::UploadFailed(put_status))
        }
    }

    pub async fn push_json_blob<T: Serialize>(&self, data: &T) -> Result<(String, u64), OciError> {
        let json_bytes = serde_json::to_vec_pretty(data)?;
        let size = json_bytes.len() as u64;

        let mut hasher = Sha256::new();
        hasher.update(&json_bytes);
        let hash_bytes = hasher.finalize();
        let digest = format!(
            "sha256:{}",
            hash_bytes
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        );

        if self.head_blob(&digest).await? {
            return Ok((digest, size));
        }

        let temp_file = NamedTempFile::new()?;
        tokio::fs::write(temp_file.path(), &json_bytes).await?;
        let pushed_digest = self.push_blob(temp_file.path()).await?;
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
        let resp = self.client.get(&url).headers(headers).send().await?;

        if resp.status() == StatusCode::OK {
            let digest_header = resp
                .headers()
                .get("Docker-Content-Digest")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            let body = resp.text().await?;
            let digest = digest_header.unwrap_or_else(|| {
                let mut hasher = Sha256::new();
                hasher.update(body.as_bytes());
                let hash_bytes = hasher.finalize();
                format!(
                    "sha256:{}",
                    hash_bytes
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<String>()
                )
            });

            Ok(Some((body, digest)))
        } else if resp.status() == StatusCode::NOT_FOUND {
            Ok(None)
        } else {
            Err(OciError::Other(format!(
                "OCI registry manifest request failed with status: {}",
                resp.status()
            )))
        }
    }

    pub async fn get_manifest(&self, tag: &str) -> Result<Option<String>, OciError> {
        self.get_manifest_with_digest(tag)
            .await
            .map(|opt| opt.map(|(body, _)| body))
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

    pub async fn get_session_manifest(
        &self,
        tag: &str,
    ) -> Result<Option<(RunSessionManifest, String)>, OciError> {
        match self.get_manifest_with_digest(tag).await? {
            Some((manifest_json, manifest_digest)) => {
                let manifest = serde_json::from_str::<OciImageManifest>(&manifest_json)?;
                let blob_digest = manifest.first_layer_digest().ok_or_else(|| {
                    OciError::Other("Session manifest missing layer digest".to_string())
                })?;

                let blob_bytes = self.get_blob(blob_digest).await?;
                let session: RunSessionManifest = serde_json::from_slice(&blob_bytes)?;
                Ok(Some((session, manifest_digest)))
            }
            None => Ok(None),
        }
    }

    pub async fn get_cache_index(
        &self,
        tag: &str,
    ) -> Result<Option<(CacheIndexData, String)>, OciError> {
        match self.get_manifest_with_digest(tag).await? {
            Some((manifest_json, manifest_digest)) => {
                let manifest = serde_json::from_str::<OciImageManifest>(&manifest_json)?;
                let blob_digest = manifest.first_layer_digest().ok_or_else(|| {
                    OciError::Other("Index manifest missing layer digest".to_string())
                })?;

                let blob_bytes = self.get_blob(blob_digest).await?;
                let index: CacheIndexData = serde_json::from_slice(&blob_bytes)?;
                Ok(Some((index, manifest_digest)))
            }
            None => Ok(None),
        }
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

        let resp = self
            .client
            .put(&url)
            .headers(headers)
            .body(manifest.to_string())
            .send()
            .await?;

        let status = resp.status();
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
        let resp = self.client.delete(&url).headers(headers).send().await?;

        if resp.status().is_success() {
            Ok(true)
        } else if resp.status() == StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            Err(OciError::Other(format!(
                "Failed to delete manifest with status: {}",
                resp.status()
            )))
        }
    }

    /// 泛型 CAS 乐观并发重试更新器，自动处理冲突与退避
    pub async fn update_manifest_cas<T, F>(
        &self,
        tag: &str,
        max_retries: usize,
        mut mutator: F,
    ) -> Result<(), OciError>
    where
        T: Serialize + for<'de> Deserialize<'de> + Send,
        F: FnMut(Option<T>) -> Result<T, OciError>,
    {
        let empty_config = "{}";
        let temp_cfg = NamedTempFile::new()?;
        tokio::fs::write(temp_cfg.path(), empty_config.as_bytes()).await?;
        let config_digest = self.push_blob(temp_cfg.path()).await?;
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
                    let data = serde_json::from_slice::<T>(&blob_bytes)?;
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
                    let backoff_ms = (500 * (1 << attempt.min(5)))
                        + ((std::process::id() as u64 * 37 + attempt as u64 * 53) % 150);
                    warn!(
                        "CAS conflict on tag {}, retrying in {}ms (attempt {}/{})",
                        tag, backoff_ms, attempt, max_retries
                    );
                    sleep(Duration::from_millis(backoff_ms)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_run_session_with_cas(
        &self,
        run_id: u64,
        new_entries: HashMap<StoreHash, IndexEntry>,
        new_roots: Vec<StoreHash>,
        system: impl Into<SystemArch>,
        job_id: &str,
        head_sha: Option<&str>,
        ref_name: Option<&str>,
        public_key: Option<&str>,
        uploaded_blobs: usize,
        uploaded_bytes: u64,
        max_retries: usize,
    ) -> Result<(), OciError> {
        let tag = format!("run-{}", run_id);
        let system_arch = system.into();
        let mut attempt = 0;

        let empty_config = "{}";
        let temp_cfg = NamedTempFile::new()?;
        tokio::fs::write(temp_cfg.path(), empty_config.as_bytes()).await?;
        let config_digest = self.push_blob(temp_cfg.path()).await?;
        let config_size = 2u64;

        loop {
            attempt += 1;
            let (mut session, previous_digest) = match self.get_session_manifest(&tag).await? {
                Some((data, digest)) => (data, Some(digest)),
                None => (
                    RunSessionManifest {
                        version: RUN_SESSION_VERSION,
                        run_id,
                        head_sha: head_sha.unwrap_or_default().to_string(),
                        ref_name: ref_name.unwrap_or_default().to_string(),
                        created_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        updated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        public_key: public_key.map(|k| k.to_string()),
                        entries: HashMap::new(),
                        gc_roots: HashMap::new(),
                        completed_jobs: Vec::new(),
                    },
                    None,
                ),
            };

            if session.head_sha.is_empty()
                && let Some(sha) = head_sha
            {
                session.head_sha = sha.to_string();
            }
            if session.ref_name.is_empty()
                && let Some(rn) = ref_name
            {
                session.ref_name = rn.to_string();
            }
            if session.public_key.is_none()
                && let Some(pk) = public_key
                && !pk.is_empty()
            {
                session.public_key = Some(pk.to_string());
            }

            session.entries.extend(new_entries.clone());
            let roots_entry = session.gc_roots.entry(system_arch.clone()).or_default();
            let mut set: HashSet<StoreHash> = roots_entry.iter().cloned().collect();
            set.extend(new_roots.clone());
            let mut sorted: Vec<StoreHash> = set.into_iter().collect();
            sorted.sort();
            *roots_entry = sorted;
            session.updated_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

            session.completed_jobs.push(JobSummaryMetadata {
                job_id: job_id.to_string(),
                system: system_arch.clone(),
                uploaded_blobs,
                uploaded_bytes,
                timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            });

            let (session_blob_digest, session_blob_size) = self.push_json_blob(&session).await?;
            let manifest = build_session_manifest(
                &session_blob_digest,
                session_blob_size,
                &config_digest,
                config_size,
                run_id,
            );
            let manifest_str = manifest.to_json_string()?;

            match self
                .put_manifest_conditional(&tag, &manifest_str, previous_digest.as_deref())
                .await
            {
                Ok(_) => {
                    info!(
                        "Successfully updated session tag {} on attempt {}",
                        tag, attempt
                    );
                    return Ok(());
                }
                Err(OciError::CasConflict(_)) if attempt <= max_retries => {
                    let backoff_ms = (500 * (1 << attempt.min(5)))
                        + ((std::process::id() as u64 * 37 + attempt as u64 * 53) % 150);
                    warn!(
                        "CAS conflict on tag {}, retrying in {}ms (attempt {}/{})",
                        tag, backoff_ms, attempt, max_retries
                    );
                    sleep(Duration::from_millis(backoff_ms)).await;
                }
                Err(e) => {
                    error!(
                        "Failed to update session tag {} after {} attempts: {}",
                        tag, attempt, e
                    );
                    let fallback_tag = format!(
                        "run-{}-job-{}",
                        run_id,
                        job_id.replace(['/', ':', ' '], "-")
                    );
                    warn!("Falling back to job-specific chunk tag: {}", fallback_tag);
                    self.push_manifest(&fallback_tag, &manifest_str).await?;
                    return Ok(());
                }
            }
        }
    }

    pub async fn get_blob(&self, digest: &str) -> Result<Vec<u8>, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let headers = self.get_auth_headers().await?;
        let resp = self.client.get(&url).headers(headers).send().await?;

        if resp.status().is_success() {
            let bytes = resp.bytes().await?;
            Ok(bytes.to_vec())
        } else {
            Err(OciError::UploadFailed(resp.status()))
        }
    }

    pub async fn stream_blob(&self, digest: &str) -> Result<Response, OciError> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let headers = self.get_auth_headers().await?;
        let resp = self.client.get(&url).headers(headers).send().await?;

        Ok(resp)
    }
}
