use crate::{error::OciError, transport::OciTransport};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_channel::oneshot;
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};
use web_time::Instant;

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
}

enum InFlightState {
    Idle,
    Fetching(Vec<oneshot::Sender<String>>),
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
        let rx_to_await = {
            let mut state = self.state.lock().expect("mutex poisoned");

            // 1. Fast path: 缓存有效且在 240s 生命周期内直接复用
            if let Some((ref token, ref instant)) = state.cached
                && instant.elapsed() < Duration::from_secs(240)
            {
                return Ok(token.clone());
            }

            // 2. Singleflight 判定
            match &mut state.in_flight {
                InFlightState::Fetching(waiters) => {
                    let (tx, rx) = oneshot::channel();
                    waiters.push(tx);
                    Some(rx)
                }
                InFlightState::Idle => {
                    state.in_flight = InFlightState::Fetching(Vec::new());
                    None
                }
            }
        };

        // 3. Follower 协程等待 Leader 广播
        if let Some(rx) = rx_to_await {
            if let Ok(token) = rx.await {
                return Ok(token);
            }
            // 若 Leader 异常中断，重试一次
            return Box::pin(self.get_token(transport)).await;
        }

        // 4. Leader 协程发起网络获取
        let token = self.fetch_token_network(transport).await;

        // 5. 广播结果并更新缓存
        let mut state = self.state.lock().expect("mutex poisoned");
        let old_in_flight = std::mem::replace(&mut state.in_flight, InFlightState::Idle);
        state.cached = Some((token.clone(), Instant::now()));

        if let InFlightState::Fetching(waiters) = old_in_flight {
            for tx in waiters {
                let _ = tx.send(token.clone());
            }
        }

        Ok(token)
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
            Ok((status, _resp_headers, bytes)) if status.is_success() => {
                serde_json::from_slice::<TokenResponse>(&bytes)
                    .ok()
                    .and_then(|r| r.token)
                    .unwrap_or_else(|| self.github_token.clone())
            }
            _ => self.github_token.clone(),
        }
    }
}
