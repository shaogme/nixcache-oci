use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue},
};
use serde::Deserialize;
use std::{path::Path, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{fs::File, io::AsyncReadExt, sync::Mutex, time::Instant};
use tracing::info;

#[derive(Error, Debug)]
pub enum OciError {
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Registry authentication failed")]
    AuthFailed,

    #[error("Blob upload failed with status: {0}")]
    UploadFailed(StatusCode),

    #[error("Manifest push failed with status: {0}")]
    ManifestPushFailed(StatusCode),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Other error: {0}")]
    Other(String),
}

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
}

#[derive(Clone)]
pub struct OciClient {
    registry: String,
    repo: String,
    github_token: String,
    write_access: bool,
    token_cache: Arc<Mutex<Option<(String, Instant)>>>,
    client: Client,
}

impl OciClient {
    pub fn new(registry: &str, repo: &str, github_token: &str, write_access: bool) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            registry: registry.to_string(),
            repo: repo.to_string(),
            github_token: github_token.to_string(),
            write_access,
            token_cache: Arc::new(Mutex::new(None)),
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
        let mut cache = self.token_cache.lock().await;
        if let Some((ref token, ref instant)) = *cache
            && instant.elapsed() < Duration::from_secs(240)
        {
            return Ok(token.clone());
        }

        let scope = if self.write_access {
            "pull,push"
        } else {
            "pull"
        };

        let token_url = format!(
            "{}://{}/token?scope=repository:{}/nix-cache:{}&service={}",
            self.url_scheme(),
            self.registry,
            self.repo,
            scope,
            self.registry
        );

        let mut req = self.client.get(&token_url);

        if !self.github_token.is_empty() {
            let auth_str = format!("token:{}", self.github_token);
            let b64 = STANDARD.encode(auth_str);
            req = req.header("Authorization", format!("Basic {}", b64));
        }

        let res = req.send().await;
        let token = match res {
            Ok(resp) => {
                if resp.status().is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    if let Ok(data) = serde_json::from_str::<TokenResponse>(&text) {
                        data.token.unwrap_or_else(|| self.github_token.clone())
                    } else {
                        self.github_token.clone()
                    }
                } else {
                    self.github_token.clone()
                }
            }
            Err(_) => self.github_token.clone(),
        };

        if token.is_empty() && !self.github_token.is_empty() {
            let fallback = self.github_token.clone();
            *cache = Some((fallback.clone(), Instant::now()));
            return Ok(fallback);
        }

        *cache = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    async fn get_auth_headers(&self) -> Result<HeaderMap, OciError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Accept",
            HeaderValue::from_static("application/vnd.oci.image.manifest.v1+json"),
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
            // Some registries return 400 or 401 for invalid scope, let's treat it as not found or handle error
            Ok(false)
        }
    }

    pub async fn push_blob(&self, file_path: &Path) -> Result<String, OciError> {
        let mut file = File::open(file_path).await?;
        let mut hasher = sha2::Sha256::default();
        let mut buffer = vec![0; 64 * 1024];
        let mut size = 0u64;

        loop {
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            use sha2::Digest;
            hasher.update(&buffer[..n]);
            size += n as u64;
        }

        use sha2::Digest;
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
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file_stream));

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

    pub async fn get_manifest(&self, tag: &str) -> Result<Option<String>, OciError> {
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
            let body = resp.text().await?;
            Ok(Some(body))
        } else {
            Ok(None)
        }
    }

    pub async fn push_manifest(&self, tag: &str, manifest: &str) -> Result<(), OciError> {
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
            HeaderValue::from_static("application/vnd.oci.image.manifest.v1+json"),
        );

        let resp = self
            .client
            .put(&url)
            .headers(headers)
            .body(manifest.to_string())
            .send()
            .await?;

        let status = resp.status();
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
