use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::sync::Mutex;
use worker::{
    Fetch, Headers, Method, Response,
    js_sys::Date,
};

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
}

#[derive(Clone)]
pub struct OciClient {
    registry: String,
    repo: String,
    github_token: String,
}

static TOKEN_CACHE: Mutex<Option<(String, f64)>> = Mutex::new(None);

impl OciClient {
    pub fn new(registry: &str, repo: &str, github_token: &str) -> Self {
        Self {
            registry: registry.to_string(),
            repo: repo.to_string(),
            github_token: github_token.to_string(),
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

    pub async fn get_token(&self) -> Result<String, String> {
        let now = Date::now(); // Milliseconds since epoch
        {
            let cache = TOKEN_CACHE.lock().map_err(|e| e.to_string())?;
            if let Some((ref token, expiry)) = *cache {
                if now < expiry {
                    return Ok(token.clone());
                }
            }
        }

        let token_url = format!(
            "{}://{}/token?scope=repository:{}/nix-cache:pull&service={}",
            self.url_scheme(),
            self.registry,
            self.repo,
            self.registry
        );

        let headers = Headers::new();
        headers.set("Accept", "application/json").map_err(|e| e.to_string())?;

        if !self.github_token.is_empty() {
            let auth_str = format!("token:{}", self.github_token);
            let b64 = STANDARD.encode(auth_str);
            headers.set("Authorization", &format!("Basic {}", b64)).map_err(|e| e.to_string())?;
        }

        let mut req_init = worker::RequestInit::new();
        req_init.with_method(Method::Get);
        req_init.with_headers(headers);

        let req = worker::Request::new_with_init(&token_url, &req_init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req).send().await.map_err(|e| e.to_string())?;

        if resp.status_code() == 200 {
            let text = resp.text().await.map_err(|e| e.to_string())?;
            if let Ok(data) = serde_json::from_str::<TokenResponse>(&text) {
                if let Some(token) = data.token {
                    let mut cache = TOKEN_CACHE.lock().map_err(|e| e.to_string())?;
                    // Token is usually valid for 1 hour (3600 seconds). Cache it for 4 minutes (240 seconds).
                    *cache = Some((token.clone(), now + 240000.0));
                    return Ok(token);
                }
            }
        }

        if !self.github_token.is_empty() {
            return Ok(self.github_token.clone());
        }

        Err("Failed to retrieve OCI auth token".to_string())
    }

    async fn get_auth_headers(&self) -> Result<Headers, String> {
        let headers = Headers::new();
        headers
            .set("Accept", "application/vnd.oci.image.manifest.v1+json")
            .map_err(|e| e.to_string())?;

        let token = self.get_token().await?;
        if !token.is_empty() {
            headers
                .set("Authorization", &format!("Bearer {}", token))
                .map_err(|e| e.to_string())?;
        }
        Ok(headers)
    }

    pub async fn get_manifest(&self, tag: &str) -> Result<Option<String>, String> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            tag
        );

        let headers = self.get_auth_headers().await?;
        let mut req_init = worker::RequestInit::new();
        req_init.with_method(Method::Get);
        req_init.with_headers(headers);

        let req = worker::Request::new_with_init(&url, &req_init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req).send().await.map_err(|e| e.to_string())?;

        if resp.status_code() == 200 {
            let body = resp.text().await.map_err(|e| e.to_string())?;
            Ok(Some(body))
        } else {
            Ok(None)
        }
    }

    pub async fn get_blob(&self, digest: &str) -> Result<Vec<u8>, String> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let headers = self.get_auth_headers().await?;
        let mut req_init = worker::RequestInit::new();
        req_init.with_method(Method::Get);
        req_init.with_headers(headers);

        let req = worker::Request::new_with_init(&url, &req_init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req).send().await.map_err(|e| e.to_string())?;

        if resp.status_code() == 200 {
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            Ok(bytes)
        } else {
            Err(format!("Failed to get blob, status: {}", resp.status_code()))
        }
    }

    pub async fn fetch_blob_response(&self, digest: &str) -> Result<Response, String> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/blobs/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            digest
        );

        let headers = self.get_auth_headers().await?;
        let mut req_init = worker::RequestInit::new();
        req_init.with_method(Method::Get);
        req_init.with_headers(headers);

        let req = worker::Request::new_with_init(&url, &req_init).map_err(|e| e.to_string())?;
        let resp = Fetch::Request(req).send().await.map_err(|e| e.to_string())?;

        Ok(resp)
    }
}
