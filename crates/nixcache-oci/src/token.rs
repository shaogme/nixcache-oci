use crate::error::OciError;
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::Client;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TokenManager {
    registry: String,
    repo: String,
    github_token: String,
    write_access: bool,
    token_cache: Arc<Mutex<Option<(String, Instant)>>>,
}

impl TokenManager {
    pub fn new(registry: &str, repo: &str, github_token: &str, write_access: bool) -> Self {
        Self {
            registry: registry.to_string(),
            repo: repo.to_string(),
            github_token: github_token.to_string(),
            write_access,
            token_cache: Arc::new(Mutex::new(None)),
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

    pub async fn get_token(&self, http_client: &Client) -> Result<String, OciError> {
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

        let mut req = http_client.get(&token_url);

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
}
