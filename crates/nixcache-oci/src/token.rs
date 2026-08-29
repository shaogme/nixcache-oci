use crate::{error::OciError, transport::OciTransport};
use base64::{Engine, engine::general_purpose::STANDARD};
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use std::{fmt, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, watch},
    time::Instant,
};

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
}

#[derive(Clone)]
enum InFlightState {
    Idle,
    Fetching(watch::Receiver<Option<String>>),
}

struct InnerTokenManager {
    cached: Option<(String, Instant)>,
    in_flight: InFlightState,
}

#[derive(Clone)]
pub struct TokenManager {
    registry: String,
    repo: String,
    github_token: String,
    write_access: bool,
    state: Arc<Mutex<InnerTokenManager>>,
}

impl fmt::Debug for TokenManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenManager")
            .field("registry", &self.registry)
            .field("repo", &self.repo)
            .field("write_access", &self.write_access)
            .finish_non_exhaustive()
    }
}

impl TokenManager {
    pub fn new(registry: &str, repo: &str, github_token: &str, write_access: bool) -> Self {
        Self {
            registry: registry.to_string(),
            repo: repo.to_string(),
            github_token: github_token.to_string(),
            write_access,
            state: Arc::new(Mutex::new(InnerTokenManager {
                cached: None,
                in_flight: InFlightState::Idle,
            })),
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

    pub async fn get_token<T: OciTransport>(&self, transport: &T) -> Result<String, OciError> {
        let rx_to_await;

        {
            let mut state = self.state.lock().await;

            // 1. Fast path: 缓存有效且在 240s 生命周期内直接复用
            if let Some((ref token, ref instant)) = state.cached
                && instant.elapsed() < Duration::from_secs(240)
            {
                return Ok(token.clone());
            }

            // 2. Singleflight 防击穿：检查是否已有在途网络请求
            match &mut state.in_flight {
                InFlightState::Fetching(rx) => {
                    rx_to_await = Some(rx.clone());
                }
                InFlightState::Idle => {
                    let (tx, rx) = watch::channel(None);
                    state.in_flight = InFlightState::Fetching(rx);
                    drop(state);

                    // 获准成为 Leader 执行网络获取
                    let token = self.fetch_token_network(transport).await;

                    let mut state = self.state.lock().await;
                    state.in_flight = InFlightState::Idle;
                    state.cached = Some((token.clone(), Instant::now()));
                    let _ = tx.send(Some(token.clone()));
                    return Ok(token);
                }
            }
        }

        // 3. Follower 任务等待 Leader 广播获取结果
        if let Some(mut rx) = rx_to_await {
            loop {
                if let Some(ref token) = *rx.borrow() {
                    return Ok(token.clone());
                }
                if rx.changed().await.is_err() {
                    // 若 Leader 意外断开 channel，回退重新尝试
                    return Box::pin(self.get_token(transport)).await;
                }
            }
        }

        Ok(self.github_token.clone())
    }

    async fn fetch_token_network<T: OciTransport>(&self, transport: &T) -> String {
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

        let mut headers = HeaderMap::new();
        if !self.github_token.is_empty() {
            let auth_str = format!("token:{}", self.github_token);
            let b64 = STANDARD.encode(auth_str);
            if let Ok(val) = HeaderValue::from_str(&format!("Basic {}", b64)) {
                headers.insert("Authorization", val);
            }
        }

        match transport.get(&token_url, headers).await {
            Ok((status, _resp_headers, bytes)) => {
                if status.is_success() {
                    if let Ok(data) = serde_json::from_slice::<TokenResponse>(&bytes) {
                        data.token.unwrap_or_else(|| self.github_token.clone())
                    } else {
                        self.github_token.clone()
                    }
                } else {
                    self.github_token.clone()
                }
            }
            Err(_) => self.github_token.clone(),
        }
    }
}
