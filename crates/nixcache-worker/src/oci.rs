use crate::store::{CacheIndexData, RunSessionManifest};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Mutex;
use worker::{Fetch, Headers, Method, Request, RequestInit, Response, js_sys::Date};

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
            if let Some((ref token, expiry)) = *cache
                && now < expiry
            {
                return Ok(token.clone());
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
        headers
            .set("Accept", "application/json")
            .map_err(|e| e.to_string())?;

        if !self.github_token.is_empty() {
            let auth_str = format!("token:{}", self.github_token);
            let b64 = STANDARD.encode(auth_str);
            headers
                .set("Authorization", &format!("Basic {}", b64))
                .map_err(|e| e.to_string())?;
        }

        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Get);
        req_init.with_headers(headers);

        let req = Request::new_with_init(&token_url, &req_init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if resp.status_code() == 200 {
            let text = resp.text().await.map_err(|e| e.to_string())?;
            if let Ok(data) = serde_json::from_str::<TokenResponse>(&text)
                && let Some(token) = data.token
            {
                let mut cache = TOKEN_CACHE.lock().map_err(|e| e.to_string())?;
                // Token is usually valid for 1 hour (3600 seconds). Cache it for 4 minutes (240 seconds).
                *cache = Some((token.clone(), now + 240000.0));
                return Ok(token);
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
        headers
            .set("Cache-Control", "no-cache, no-store")
            .map_err(|e| e.to_string())?;
        headers
            .set("Pragma", "no-cache")
            .map_err(|e| e.to_string())?;

        let token = self.get_token().await?;
        if !token.is_empty() {
            headers
                .set("Authorization", &format!("Bearer {}", token))
                .map_err(|e| e.to_string())?;
        }
        Ok(headers)
    }

    pub async fn get_manifest_with_digest(
        &self,
        tag: &str,
    ) -> Result<Option<(String, String)>, String> {
        let url = format!(
            "{}://{}/v2/{}/nix-cache/manifests/{}",
            self.url_scheme(),
            self.registry,
            self.repo,
            tag
        );

        let headers = self.get_auth_headers().await?;
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Get);
        req_init.with_headers(headers);

        let req = Request::new_with_init(&url, &req_init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if resp.status_code() == 200 {
            let digest = resp
                .headers()
                .get("Docker-Content-Digest")
                .ok()
                .flatten()
                .unwrap_or_default();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            Ok(Some((body, digest)))
        } else if resp.status_code() == 404 {
            Ok(None)
        } else {
            Err(format!(
                "OCI registry returned HTTP status {}",
                resp.status_code()
            ))
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
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Get);
        req_init.with_headers(headers);

        let req = Request::new_with_init(&url, &req_init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if resp.status_code() == 200 {
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            Ok(bytes)
        } else {
            Err(format!(
                "Failed to get blob, status: {}",
                resp.status_code()
            ))
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
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Get);
        req_init.with_headers(headers);

        let req = Request::new_with_init(&url, &req_init).map_err(|e| e.to_string())?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        Ok(resp)
    }

    pub async fn get_session_manifest(
        &self,
        tag: &str,
    ) -> Result<Option<(RunSessionManifest, String)>, String> {
        match self.get_manifest_with_digest(tag).await? {
            Some((manifest_json, manifest_digest)) => {
                let manifest =
                    serde_json::from_str::<Value>(&manifest_json).map_err(|e| e.to_string())?;
                let layers = manifest
                    .get("layers")
                    .and_then(|l| l.as_array())
                    .ok_or_else(|| "Session manifest missing layers".to_string())?;

                if layers.is_empty() {
                    return Err("Session manifest layers empty".to_string());
                }

                let blob_digest = layers[0]
                    .get("digest")
                    .and_then(|d| d.as_str())
                    .ok_or_else(|| "Session layer digest missing".to_string())?;

                let blob_bytes = self.get_blob(blob_digest).await?;
                let mut session: RunSessionManifest =
                    serde_json::from_slice(&blob_bytes).map_err(|e| e.to_string())?;
                session.rebuild_lookup_table();
                Ok(Some((session, manifest_digest)))
            }
            None => Ok(None),
        }
    }

    pub async fn get_cache_index(
        &self,
        tag: &str,
    ) -> Result<Option<(CacheIndexData, String)>, String> {
        match self.get_manifest_with_digest(tag).await? {
            Some((manifest_json, manifest_digest)) => {
                let manifest =
                    serde_json::from_str::<Value>(&manifest_json).map_err(|e| e.to_string())?;
                let layers = manifest
                    .get("layers")
                    .and_then(|l| l.as_array())
                    .ok_or_else(|| "Index manifest missing layers".to_string())?;

                if layers.is_empty() {
                    return Err("Index manifest layers empty".to_string());
                }

                let blob_digest = layers[0]
                    .get("digest")
                    .and_then(|d| d.as_str())
                    .ok_or_else(|| "Index layer digest missing".to_string())?;

                let blob_bytes = self.get_blob(blob_digest).await?;
                let mut index: CacheIndexData =
                    serde_json::from_slice(&blob_bytes).map_err(|e| e.to_string())?;
                index.manifest_digest = manifest_digest.clone();
                index.rebuild_lookup_table();
                Ok(Some((index, manifest_digest)))
            }
            None => Ok(None),
        }
    }
}
