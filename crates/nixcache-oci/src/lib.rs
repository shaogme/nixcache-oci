mod types;

pub use types::{
    BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, RECEIPT_VERSION,
};

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
        } else if resp.status() == StatusCode::NOT_FOUND {
            Ok(None)
        } else {
            Err(OciError::Other(format!(
                "OCI registry manifest request failed with status: {}",
                resp.status()
            )))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
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
        assert_eq!(bytes, b"hello blob content");

        Mock::given(method("GET"))
            .and(path("/v2/test/repo/nix-cache/blobs/sha256:notfound"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = client.get_blob("sha256:notfound").await.unwrap_err();
        assert!(matches!(err, OciError::UploadFailed(StatusCode::NOT_FOUND)));
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

        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
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

        let upload_err = OciError::UploadFailed(StatusCode::BAD_REQUEST);
        assert!(format!("{}", upload_err).contains("400"));

        let manifest_err = OciError::ManifestPushFailed(StatusCode::INTERNAL_SERVER_ERROR);
        assert!(format!("{}", manifest_err).contains("500"));

        let custom_err = OciError::Other("custom failure".to_string());
        assert_eq!(format!("{}", custom_err), "Other error: custom failure");

        let url_err = OciError::InvalidUrl("invalid".to_string());
        assert_eq!(format!("{}", url_err), "Invalid URL: invalid");
    }
}
