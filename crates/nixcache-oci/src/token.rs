#[cfg(loom)]
pub mod sync;

#[cfg(not(loom))]
mod sync;

use crate::{
    auth::{BearerChallenge, RegistryCredentials},
    backend::driver::OciDriver,
    error::{OciError, TokenError},
    transport::OciTransport,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use sync::{ChallengeKey, FlightOutcome, FlightRegistry, Leader, Waiter};
use web_time::Instant;

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<u64>,
    issued_at: Option<String>,
}

pub(crate) use crate::auth::SecretToken;

trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// OCI 注册表鉴权令牌管理器。
#[derive(Clone)]
pub struct TokenManager {
    registry: String,
    repo: String,
    credentials: RegistryCredentials,
    write_access: bool,
    driver: OciDriver,
    flights: FlightRegistry,
    clock: Arc<dyn Clock>,
}

impl TokenManager {
    pub fn new(
        registry: &str,
        repo: &str,
        credentials: impl Into<RegistryCredentials>,
        write_access: bool,
        driver: impl Into<OciDriver>,
    ) -> Self {
        Self::new_with_clock(
            registry,
            repo,
            credentials,
            write_access,
            driver,
            Arc::new(SystemClock),
        )
    }

    fn new_with_clock(
        registry: &str,
        repo: &str,
        credentials: impl Into<RegistryCredentials>,
        write_access: bool,
        driver: impl Into<OciDriver>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let driver = driver.into();
        let clean_registry = driver.canonicalize_endpoint(registry);
        let clean_repo = driver.canonicalize_repository(repo);
        Self {
            registry: clean_registry,
            repo: clean_repo,
            credentials: credentials.into(),
            write_access,
            driver,
            flights: FlightRegistry::new(),
            clock,
        }
    }

    pub(crate) fn auth_token(&self) -> &str {
        self.credentials.secret().unwrap_or_default()
    }

    pub(crate) fn default_scope(&self) -> String {
        self.driver.format_auth_scope(&self.repo, self.write_access)
    }

    pub(crate) fn default_service(&self) -> &str {
        &self.registry
    }

    pub(crate) async fn cached_token(&self) -> Option<Arc<str>> {
        self.flights.load_last(self.clock.now())
    }

    /// 按 Registry 返回的 challenge 获取 token，并以 realm/service/scope 隔离缓存。
    pub async fn get_token_for_challenge<T: OciTransport>(
        &self,
        transport: &T,
        challenge: &BearerChallenge,
    ) -> Result<Arc<str>, OciError> {
        self.validate_challenge(challenge)?;
        let key = self.challenge_key(challenge);
        self.get_token_for_key(transport, challenge, key).await
    }

    /// 使当前 challenge 的 token 失效，并只获取递增 generation 的 token。
    pub async fn refresh_token<T: OciTransport>(
        &self,
        transport: &T,
        challenge: &BearerChallenge,
    ) -> Result<Arc<str>, OciError> {
        self.validate_challenge(challenge)?;
        let key = self.challenge_key(challenge);
        self.invalidate_challenge(challenge);
        self.get_token_for_key(transport, challenge, key).await
    }

    pub(crate) fn invalidate_challenge(&self, challenge: &BearerChallenge) {
        let key = self.challenge_key(challenge);
        self.flights.invalidate(key);
    }

    fn validate_challenge(&self, challenge: &BearerChallenge) -> Result<(), OciError> {
        challenge.validate()?;
        if challenge.is_insecure_non_localhost() && self.credentials.has_secret() {
            return Err(OciError::AuthChallengeInvalid {
                details: "refusing to send long-lived credentials to an insecure token realm"
                    .to_string(),
            });
        }
        Ok(())
    }

    fn challenge_key(&self, challenge: &BearerChallenge) -> ChallengeKey {
        let default_scope = self.default_scope();
        let (realm, service, scope) = challenge.cache_key(self.default_service(), &default_scope);
        ChallengeKey {
            realm,
            service,
            scope,
        }
    }

    async fn get_token_for_key<T: OciTransport>(
        &self,
        transport: &T,
        challenge: &BearerChallenge,
        key: ChallengeKey,
    ) -> Result<Arc<str>, OciError> {
        loop {
            if let Some(token) = self.flights.load(&key, self.clock.now()) {
                return Ok(token);
            }

            match self.flights.acquire(key.clone()) {
                Leader(leader) => {
                    // acquire 已经在同一个同步临界区内创建了 Guard；这里之后的每个 await
                    // 都受 Guard 保护，包括这个必要的二次缓存检查。
                    if let Some(token) = self.flights.load(&key, self.clock.now()) {
                        leader.finish(FlightOutcome::Succeeded);
                        return Ok(token);
                    }

                    match self.fetch_token_network(transport, challenge).await {
                        Ok((token, ttl)) => {
                            let expires_at = self.clock.now() + ttl;
                            if self.flights.store_if_current(
                                key.clone(),
                                leader.generation(),
                                Arc::clone(&token),
                                expires_at,
                            ) {
                                leader.finish(FlightOutcome::Succeeded);
                                return Ok(token);
                            }
                            // invalidate/refresh 已经推进了 generation；旧结果不能回填或
                            // 满足当前请求，丢弃后重新读取当前状态。
                            leader.finish(FlightOutcome::Cancelled);
                        }
                        Err(error) => {
                            leader.finish(FlightOutcome::Failed);
                            return Err(error);
                        }
                    }
                }
                Waiter(waiter) => match waiter.wait().await {
                    FlightOutcome::Succeeded | FlightOutcome::Cancelled => continue,
                    FlightOutcome::Failed => {
                        return Err(OciError::Token(TokenError::FlightFailed));
                    }
                },
            }
        }
    }

    /// 基于调用方已获得的 Bearer challenge 获取 token。
    pub async fn get_token<T: OciTransport>(
        &self,
        transport: &T,
        challenge: &BearerChallenge,
    ) -> Result<Arc<str>, OciError> {
        self.get_token_for_challenge(transport, challenge).await
    }

    async fn fetch_token_network<T: OciTransport>(
        &self,
        transport: &T,
        challenge: &BearerChallenge,
    ) -> Result<(Arc<str>, Duration), OciError> {
        let token_url = challenge.token_url(self.default_service(), &self.default_scope());
        let mut headers = HeaderMap::new();
        if let Some(secret) = self.credentials.secret()
            && let Some(username) = self
                .credentials
                .username()
                .or_else(|| self.driver.default_basic_username())
        {
            let auth_str = format!("{}:{}", username, secret);
            let b64 = STANDARD.encode(auth_str);
            let auth = HeaderValue::from_str(&format!("Basic {}", b64)).map_err(|_| {
                OciError::AuthChallengeInvalid {
                    details: "invalid Basic authentication header".to_string(),
                }
            })?;
            headers.insert("Authorization", auth);
        }

        let (status, _response_headers, bytes) = transport
            .get(&token_url, headers)
            .await
            .map_err(OciError::Transport)?;
        if !status.is_success() {
            return Err(OciError::Token(TokenError::ExchangeFailed {
                realm: challenge.realm.clone(),
                status,
            }));
        }

        let response: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| {
            OciError::Token(TokenError::InvalidResponse {
                realm: challenge.realm.clone(),
                details: "invalid JSON token response",
            })
        })?;
        let token = response
            .token
            .or(response.access_token)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                OciError::Token(TokenError::InvalidResponse {
                    realm: challenge.realm.clone(),
                    details: "response does not contain token or access_token",
                })
            })?;
        let lifetime_secs = response.expires_in.unwrap_or(300).max(1);
        let elapsed_secs = response
            .issued_at
            .as_deref()
            .and_then(|issued_at| DateTime::parse_from_rfc3339(issued_at).ok())
            .map(|issued_at| {
                let issued_at = issued_at.with_timezone(&Utc);
                Utc::now()
                    .signed_duration_since(issued_at)
                    .num_seconds()
                    .max(0) as u64
            })
            .unwrap_or(0);
        let lifetime = Duration::from_secs(lifetime_secs.saturating_sub(elapsed_secs).max(1));
        let skew = lifetime.min(Duration::from_secs(30));
        let ttl = lifetime.saturating_sub(skew).max(Duration::from_secs(1));
        Ok((Arc::from(token), ttl))
    }
}

#[cfg(test)]
mod tests {
    use super::TokenManager;
    use crate::{
        auth::BearerChallenge,
        backend::driver::{GhcrDriver, detect_driver},
        mock::{MockResponse, MockRouterTransport, MockTokenGate},
    };
    use bytes::Bytes;
    use http::{HeaderMap, StatusCode};
    use std::{
        sync::{Arc, Mutex, atomic::Ordering},
        time::Duration,
    };
    use web_time::Instant;

    #[derive(Clone)]
    struct FakeClock {
        now: Arc<Mutex<Instant>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Arc::new(Mutex::new(Instant::now())),
            }
        }

        fn advance(&self, duration: Duration) {
            let mut now = self
                .now
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *now += duration;
        }
    }

    impl super::Clock for FakeClock {
        fn now(&self) -> Instant {
            *self
                .now
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
    }

    fn test_challenge() -> BearerChallenge {
        BearerChallenge::new(
            "https://auth.example.test/token",
            Some("test.registry.io".to_string()),
            Some("repository:test/repo/nix-cache:pull,push".to_string()),
        )
        .unwrap()
    }

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
            handles.push(tokio::spawn(async move {
                mgr.get_token(&*tr, &test_challenge()).await
            }));
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
        let t1 = token_mgr
            .get_token(&transport, &test_challenge())
            .await
            .unwrap();
        assert_eq!(t1.as_ref(), "singleflight-jwt-token");
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);

        // 第二次调用：0 锁快路径原子快照直接返回（网络调用次数依然为 1）
        let t2 = token_mgr
            .get_token(&transport, &test_challenge())
            .await
            .unwrap();
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

        // 首次调用网络失败时直接返回错误，不把长期凭据冒充成 Bearer token。
        assert!(
            token_mgr
                .get_token(&transport, &test_challenge())
                .await
                .is_err()
        );

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
        let t2 = token_mgr
            .get_token(&transport, &test_challenge())
            .await
            .unwrap();
        assert_eq!(t2.as_ref(), "recovered-token");
    }

    #[tokio::test]
    async fn test_token_manager_error_wakes_waiter_and_allows_retry() {
        let transport = Arc::new(MockRouterTransport::default());
        transport.add_token_response(MockResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        });
        transport.add_token_response(MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(r#"{"token":"recovered-after-error","expires_in":300}"#),
        });
        let gate = MockTokenGate::new();
        transport.set_token_gate(gate.clone());
        let driver = detect_driver("test.registry.io");
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            driver,
        ));

        let leader = {
            let mgr = Arc::clone(&token_mgr);
            let tr = Arc::clone(&transport);
            tokio::spawn(async move { mgr.get_token(&*tr, &test_challenge()).await })
        };
        gate.wait_until_entered().await;
        let waiter = {
            let mgr = Arc::clone(&token_mgr);
            let tr = Arc::clone(&transport);
            tokio::spawn(async move { mgr.get_token(&*tr, &test_challenge()).await })
        };
        tokio::task::yield_now().await;
        gate.release();

        assert!(leader.await.expect("leader task must not panic").is_err());
        let waiter_result = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter must be woken by failed flight")
            .expect("waiter task must not panic");
        assert!(waiter_result.is_err());

        let recovered = token_mgr
            .get_token(&*transport, &test_challenge())
            .await
            .expect("next request must be able to create a new flight");
        assert_eq!(recovered.as_ref(), "recovered-after-error");
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_token_manager_leader_abort_releases_waiters() {
        let transport = Arc::new(make_test_transport());
        let gate = MockTokenGate::new();
        transport.set_token_gate(gate.clone());
        let driver = detect_driver("test.registry.io");
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            driver,
        ));

        let leader = {
            let mgr = Arc::clone(&token_mgr);
            let tr = Arc::clone(&transport);
            tokio::spawn(async move { mgr.get_token(&*tr, &test_challenge()).await })
        };
        gate.wait_until_entered().await;

        let waiter = {
            let mgr = Arc::clone(&token_mgr);
            let tr = Arc::clone(&transport);
            tokio::spawn(async move { mgr.get_token(&*tr, &test_challenge()).await })
        };
        tokio::task::yield_now().await;
        leader.abort();
        gate.release();
        assert!(leader.await.is_err());

        let result = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter must not remain blocked")
            .expect("waiter task must not panic")
            .expect("waiter must recover after leader cancellation");
        assert_eq!(result.as_ref(), "singleflight-jwt-token");
    }

    #[tokio::test]
    async fn test_token_manager_leader_panic_releases_flight() {
        let transport = make_test_transport();
        transport.set_panic_on_token(true);
        let driver = detect_driver("test.registry.io");
        let token_mgr =
            TokenManager::new("test.registry.io", "test/repo", "secret_tok", false, driver);

        let task = {
            let mgr = token_mgr.clone();
            let tr = transport.clone();
            tokio::spawn(async move { mgr.get_token(&tr, &test_challenge()).await })
        };
        assert!(task.await.is_err(), "panic must be isolated to leader task");

        transport.set_panic_on_token(false);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            token_mgr.get_token(&transport, &test_challenge()),
        )
        .await
        .expect("post-panic request must not remain blocked")
        .expect("post-panic request must recover");
        assert_eq!(result.as_ref(), "singleflight-jwt-token");
    }

    #[tokio::test]
    async fn test_token_manager_waiter_abort_does_not_release_leader() {
        let transport = Arc::new(make_test_transport());
        let gate = MockTokenGate::new();
        transport.set_token_gate(gate.clone());
        let driver = detect_driver("test.registry.io");
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            driver,
        ));

        let leader = {
            let mgr = Arc::clone(&token_mgr);
            let tr = Arc::clone(&transport);
            tokio::spawn(async move { mgr.get_token(&*tr, &test_challenge()).await })
        };
        gate.wait_until_entered().await;

        let waiter = {
            let mgr = Arc::clone(&token_mgr);
            let tr = Arc::clone(&transport);
            tokio::spawn(async move { mgr.get_token(&*tr, &test_challenge()).await })
        };
        let mut waiter = waiter;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err()
        );
        waiter.abort();

        gate.release();
        let result = tokio::time::timeout(Duration::from_secs(2), leader)
            .await
            .expect("leader must finish")
            .expect("leader task must not panic")
            .expect("leader must retain ownership after waiter cancellation");
        assert_eq!(result.as_ref(), "singleflight-jwt-token");
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_token_manager_ttl_expires_with_fake_clock() {
        let transport = MockRouterTransport::default();
        transport.add_token_response(MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(r#"{"token":"before-expiry","expires_in":300}"#),
        });
        transport.add_token_response(MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(r#"{"token":"after-expiry","expires_in":300}"#),
        });
        let clock = Arc::new(FakeClock::new());
        let driver = detect_driver("test.registry.io");
        let token_mgr = TokenManager::new_with_clock(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            driver,
            clock.clone(),
        );

        let first = token_mgr
            .get_token(&transport, &test_challenge())
            .await
            .unwrap();
        assert_eq!(first.as_ref(), "before-expiry");
        assert_eq!(
            token_mgr.cached_token().await.as_deref(),
            Some("before-expiry")
        );

        // expires_in=300 减去 30 秒提前失效窗口后，TTL 为 270 秒。
        clock.advance(Duration::from_secs(270));
        let second = token_mgr
            .get_token(&transport, &test_challenge())
            .await
            .unwrap();
        assert_eq!(second.as_ref(), "after-expiry");
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_refresh_token_does_not_return_old_generation() {
        let transport = Arc::new(MockRouterTransport::default());
        transport.add_token_response(MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(r#"{"token":"old-generation","expires_in":300}"#),
        });
        transport.add_token_response(MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(r#"{"token":"new-generation","expires_in":300}"#),
        });
        let gate = MockTokenGate::new();
        transport.set_token_gate(gate.clone());
        let driver = detect_driver("test.registry.io");
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            driver,
        ));

        let initial = {
            let mgr = Arc::clone(&token_mgr);
            let tr = Arc::clone(&transport);
            tokio::spawn(async move { mgr.get_token(&*tr, &test_challenge()).await })
        };
        gate.wait_until_entered().await;

        let refresh = {
            let mgr = Arc::clone(&token_mgr);
            let tr = Arc::clone(&transport);
            tokio::spawn(async move { mgr.refresh_token(&*tr, &test_challenge()).await })
        };
        tokio::task::yield_now().await;
        gate.release();

        let refreshed = tokio::time::timeout(Duration::from_secs(2), refresh)
            .await
            .expect("refresh must not remain blocked")
            .expect("refresh task must not panic")
            .expect("refresh must succeed");
        assert_eq!(refreshed.as_ref(), "new-generation");
        let initial = initial.await.expect("initial task must not panic").unwrap();
        assert_eq!(initial.as_ref(), "new-generation");
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_token_manager_scope_and_write_access() {
        let transport = make_test_transport();
        let driver = GhcrDriver;
        let write_mgr = TokenManager::new("ghcr.io", "org/repo", "token", true, driver);
        let read_mgr = TokenManager::new("ghcr.io", "org/repo", "token", false, driver);

        let t_write = write_mgr
            .get_token(&transport, &test_challenge())
            .await
            .unwrap();
        let t_read = read_mgr
            .get_token(&transport, &test_challenge())
            .await
            .unwrap();

        assert_eq!(t_write.as_ref(), "singleflight-jwt-token");
        assert_eq!(t_read.as_ref(), "singleflight-jwt-token");
    }
}
