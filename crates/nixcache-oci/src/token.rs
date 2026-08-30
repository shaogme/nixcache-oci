pub mod sync;

use crate::{
    backend::driver::OciDriver,
    error::OciError,
    token::sync::{InFlightState, TokenBroadcaster, TokenStorage},
    transport::OciTransport,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

/// OCI 注册表鉴权令牌管理器（零锁 Singleflight 状态机）
#[derive(Clone, Debug)]
pub struct TokenManager {
    registry: String,
    repo: String,
    github_token: String,
    write_access: bool,
    driver: OciDriver,
    storage: TokenStorage,
    in_flight: InFlightState,
    broadcaster: TokenBroadcaster,
}

impl TokenManager {
    pub fn new(
        registry: &str,
        repo: &str,
        github_token: &str,
        write_access: bool,
        driver: impl Into<OciDriver>,
    ) -> Self {
        let driver = driver.into();
        let clean_registry = driver.canonicalize_endpoint(registry);
        let clean_repo = driver.canonicalize_repository(repo);
        Self {
            registry: clean_registry,
            repo: clean_repo,
            github_token: github_token.to_string(),
            write_access,
            driver,
            storage: TokenStorage::new(),
            in_flight: InFlightState::new(),
            broadcaster: TokenBroadcaster::new(),
        }
    }

    /// 核心鉴权方法：99.9% 场景为 Wait-Free 无锁读取，返回不可变共享 Arc<str>
    pub async fn get_token<T: OciTransport>(&self, transport: &T) -> Result<Arc<str>, OciError> {
        // 1. Fast Path: Wait-Free 读取原子快照 (0 锁争用，0 堆内存深拷贝)
        if let Some(cached) = self.storage.load() {
            return Ok(cached);
        }

        // 2. Slow Path: CAS 抢占 Leader 状态
        if self.in_flight.try_acquire_leader() {
            // Double-Check: 检查在 CAS 抢占期间是否已有刚刚退出的 Leader 填充了有效缓存
            if let Some(cached) = self.storage.load() {
                self.in_flight.release_leader();
                return Ok(cached);
            }

            // Leader: 发起网络获取
            let maybe_fetched = self.fetch_token_network(transport).await;
            let result_token: Arc<str> = match maybe_fetched {
                Some(tok) => {
                    let arc_tok: Arc<str> = Arc::from(tok.as_str());
                    self.storage.store(Arc::clone(&arc_tok));
                    arc_tok
                }
                None => Arc::from(self.github_token.as_str()),
            };

            // 广播通知所有等待中的协程
            self.broadcaster.broadcast(Arc::clone(&result_token));
            self.in_flight.release_leader();

            return Ok(result_token);
        }

        // 3. Follower: 订阅等待 Leader 广播
        match self.broadcaster.wait().await {
            Ok(token) => Ok(token),
            Err(_) => {
                // Leader 异常断开，安全重试
                Box::pin(self.get_token(transport)).await
            }
        }
    }

    async fn fetch_token_network<T: OciTransport>(&self, transport: &T) -> Option<String> {
        let token_url =
            self.driver
                .resolve_token_endpoint(&self.registry, &self.repo, self.write_access);

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
                let parsed = serde_json::from_slice::<TokenResponse>(&bytes).ok();
                let token_opt = parsed.and_then(|r| r.token.or(r.access_token));
                token_opt.or_else(|| {
                    if !self.github_token.is_empty() {
                        Some(self.github_token.clone())
                    } else {
                        None
                    }
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TokenManager;
    use crate::{
        backend::driver::{GhcrDriver, detect_driver},
        mock::{MockResponse, MockRouterTransport},
    };
    use bytes::Bytes;
    use http::{HeaderMap, StatusCode};
    use std::sync::{Arc, atomic::Ordering};

    fn make_test_transport() -> MockRouterTransport {
        let transport = MockRouterTransport::default();
        transport.add_route(
            "GET",
            "/token",
            MockResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::from(r#"{"token": "singleflight-jwt-token"}"#),
            },
        );
        transport
    }

    #[tokio::test]
    async fn test_token_manager_singleflight_concurrent_storm() {
        let transport = Arc::new(make_test_transport());
        let driver = detect_driver("test.registry.io");
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            driver,
        ));

        let mut handles = Vec::new();
        for _ in 0..50 {
            let mgr = token_mgr.clone();
            let tr = transport.clone();
            handles.push(tokio::spawn(async move { mgr.get_token(&*tr).await }));
        }

        let mut tokens = Vec::new();
        for h in handles {
            let res = h.await.unwrap();
            assert!(res.is_ok());
            tokens.push(res.unwrap());
        }

        // 验证 1: 50 个高并发请求下，网络 token fetch 仅仅发生了 1 次（CAS 完美单飞）
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);

        // 验证 2: 所有 50 个协程获取到的 Token 必定一致且正确
        for tok in &tokens {
            assert_eq!(tok.as_ref(), "singleflight-jwt-token");
        }
    }

    #[tokio::test]
    async fn test_token_manager_fast_path_cache_and_double_check() {
        let transport = make_test_transport();
        let driver = detect_driver("test.registry.io");
        let token_mgr =
            TokenManager::new("test.registry.io", "test/repo", "secret_tok", true, driver);

        // 第一次调用：执行网络 Fetch
        let t1 = token_mgr.get_token(&transport).await.unwrap();
        assert_eq!(t1.as_ref(), "singleflight-jwt-token");
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);

        // 第二次调用：0 锁快路径原子快照直接返回（网络调用次数依然为 1）
        let t2 = token_mgr.get_token(&transport).await.unwrap();
        assert_eq!(t2.as_ref(), "singleflight-jwt-token");
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_token_manager_failure_recovery_and_liveness() {
        let transport = MockRouterTransport::default();
        transport.add_route(
            "GET",
            "/token",
            MockResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
        );

        let driver = detect_driver("test.registry.io");
        let token_mgr = TokenManager::new(
            "test.registry.io",
            "test/repo",
            "fallback_token",
            false,
            driver,
        );

        // 首次调用网络失败，安全回退到 fallback token，不发生死锁或 panic
        let t1 = token_mgr.get_token(&transport).await.unwrap();
        assert_eq!(t1.as_ref(), "fallback_token");

        // 路由恢复正常
        transport.add_route(
            "GET",
            "/token",
            MockResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::from(r#"{"token": "recovered-token"}"#),
            },
        );

        // 随后的请求能够重新竞选 Leader 并成功获取新 Token
        let t2 = token_mgr.get_token(&transport).await.unwrap();
        assert_eq!(t2.as_ref(), "recovered-token");
    }

    #[tokio::test]
    async fn test_token_manager_scope_and_write_access() {
        let transport = MockRouterTransport::default();
        let driver = GhcrDriver;
        let write_mgr = TokenManager::new("ghcr.io", "org/repo", "token", true, driver);
        let read_mgr = TokenManager::new("ghcr.io", "org/repo", "token", false, driver);

        let t_write = write_mgr.get_token(&transport).await.unwrap();
        let t_read = read_mgr.get_token(&transport).await.unwrap();

        assert_eq!(t_write.as_ref(), "token");
        assert_eq!(t_read.as_ref(), "token");
    }
}
