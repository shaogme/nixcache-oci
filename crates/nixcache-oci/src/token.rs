pub mod sync;

#[cfg(loom)]
use crate::token::sync::{InFlightState, TokenBroadcaster, TokenStorage};
use crate::{
    auth::{BearerChallenge, RegistryCredentials},
    backend::driver::OciDriver,
    error::OciError,
    transport::OciTransport,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
#[cfg(not(loom))]
use std::collections::HashMap;
#[cfg(not(loom))]
use std::time::Instant;
use std::{sync::Arc, time::Duration};

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<u64>,
    issued_at: Option<String>,
}

pub(crate) use crate::auth::SecretToken;

#[cfg(not(loom))]
#[derive(Clone, PartialEq, Eq, Hash)]
struct ChallengeKey {
    realm: String,
    service: String,
    scope: String,
}

#[cfg(not(loom))]
struct CachedChallengeToken {
    token: Arc<str>,
    expires_at: Instant,
}

#[cfg(not(loom))]
struct ChallengeState {
    cache: tokio::sync::Mutex<HashMap<ChallengeKey, CachedChallengeToken>>,
    leaders: std::sync::Mutex<std::collections::HashSet<ChallengeKey>>,
    wake: tokio::sync::Notify,
    last_key: tokio::sync::Mutex<Option<ChallengeKey>>,
}

#[cfg(not(loom))]
impl ChallengeState {
    fn new() -> Self {
        Self {
            cache: tokio::sync::Mutex::new(HashMap::new()),
            leaders: std::sync::Mutex::new(std::collections::HashSet::new()),
            wake: tokio::sync::Notify::new(),
            last_key: tokio::sync::Mutex::new(None),
        }
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
    #[cfg(loom)]
    storage: TokenStorage,
    #[cfg(loom)]
    in_flight: InFlightState,
    #[cfg(loom)]
    broadcaster: TokenBroadcaster,
    #[cfg(not(loom))]
    challenge_state: Arc<ChallengeState>,
}

impl TokenManager {
    pub fn new(
        registry: &str,
        repo: &str,
        credentials: impl Into<RegistryCredentials>,
        write_access: bool,
        driver: impl Into<OciDriver>,
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
            #[cfg(loom)]
            storage: TokenStorage::new(),
            #[cfg(loom)]
            in_flight: InFlightState::new(),
            #[cfg(loom)]
            broadcaster: TokenBroadcaster::new(),
            #[cfg(not(loom))]
            challenge_state: Arc::new(ChallengeState::new()),
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
        #[cfg(not(loom))]
        {
            let key = self.challenge_state.last_key.lock().await.clone()?;
            let cache = self.challenge_state.cache.lock().await;
            cache
                .get(&key)
                .filter(|entry| entry.expires_at > Instant::now())
                .map(|entry| Arc::clone(&entry.token))
        }
        #[cfg(loom)]
        {
            None
        }
    }

    /// 按 Registry 返回的 challenge 获取 token，并以 realm/service/scope 隔离缓存。
    pub async fn get_token_for_challenge<T: OciTransport>(
        &self,
        transport: &T,
        challenge: &BearerChallenge,
        force_refresh: bool,
    ) -> Result<Arc<str>, OciError> {
        challenge.validate()?;
        if challenge.is_insecure_non_localhost() && self.credentials.has_secret() {
            return Err(OciError::AuthChallengeInvalid {
                details: "refusing to send long-lived credentials to an insecure token realm"
                    .to_string(),
            });
        }

        #[cfg(loom)]
        {
            if !force_refresh && let Some(cached) = self.storage.load() {
                return Ok(cached);
            }
            if !force_refresh && let Some(cached) = self.broadcaster.load() {
                self.storage.store(Arc::clone(&cached));
                return Ok(cached);
            }
            let leader = self.in_flight.try_acquire_leader();
            if leader {
                if !force_refresh {
                    if let Some(cached) = self.storage.load() {
                        self.in_flight.release_leader();
                        return Ok(cached);
                    }
                    if let Some(cached) = self.broadcaster.load() {
                        self.storage.store(Arc::clone(&cached));
                        self.in_flight.release_leader();
                        return Ok(cached);
                    }
                }
                let result = self.fetch_token_network(transport, challenge).await;
                match result {
                    Ok((token, _)) => {
                        self.storage.store(Arc::clone(&token));
                        self.broadcaster.broadcast(Arc::clone(&token));
                        self.in_flight.release_leader();
                        return Ok(token);
                    }
                    Err(error) => {
                        self.broadcaster.broadcast_error();
                        self.in_flight.release_leader();
                        return Err(error);
                    }
                }
            }
            return self.broadcaster.wait().await;
        }

        #[cfg(not(loom))]
        {
            let key = self.challenge_key(challenge);
            let mut force_refresh = force_refresh;
            loop {
                if !force_refresh && let Some(token) = self.load_challenge_token(&key).await {
                    return Ok(token);
                }

                let notified = self.challenge_state.wake.notified();
                let mut notified = std::pin::pin!(notified);
                notified.as_mut().enable();
                let leader = {
                    let mut leaders = self
                        .challenge_state
                        .leaders
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    leaders.insert(key.clone())
                };
                if !leader {
                    notified.await;
                    force_refresh = false;
                    continue;
                }

                if !force_refresh && let Some(token) = self.load_challenge_token(&key).await {
                    self.challenge_state
                        .leaders
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&key);
                    self.challenge_state.wake.notify_waiters();
                    return Ok(token);
                }

                let guard = LeaderGuard::new(Arc::clone(&self.challenge_state), key.clone());
                let result = self.fetch_token_network(transport, challenge).await;
                match result {
                    Ok((token, ttl)) => {
                        self.store_challenge_token(&key, Arc::clone(&token), ttl)
                            .await;
                        guard.finish();
                        return Ok(token);
                    }
                    Err(error) => {
                        guard.finish();
                        return Err(error);
                    }
                }
            }
        }
    }

    pub(crate) async fn invalidate_challenge(&self, _challenge: &BearerChallenge) {
        #[cfg(not(loom))]
        {
            let key = self.challenge_key(_challenge);
            self.challenge_state.cache.lock().await.remove(&key);
        }
    }

    #[cfg(not(loom))]
    fn challenge_key(&self, challenge: &BearerChallenge) -> ChallengeKey {
        let default_scope = self.default_scope();
        let (realm, service, scope) = challenge.cache_key(self.default_service(), &default_scope);
        ChallengeKey {
            realm,
            service,
            scope,
        }
    }

    #[cfg(not(loom))]
    async fn load_challenge_token(&self, key: &ChallengeKey) -> Option<Arc<str>> {
        let mut cache = self.challenge_state.cache.lock().await;
        let entry = cache.get(key)?;
        if entry.expires_at <= Instant::now() {
            cache.remove(key);
            return None;
        }
        Some(Arc::clone(&entry.token))
    }

    #[cfg(not(loom))]
    async fn store_challenge_token(&self, key: &ChallengeKey, token: Arc<str>, ttl: Duration) {
        self.challenge_state.cache.lock().await.insert(
            key.clone(),
            CachedChallengeToken {
                token,
                expires_at: Instant::now() + ttl,
            },
        );
        *self.challenge_state.last_key.lock().await = Some(key.clone());
    }

    /// 基于调用方已获得的 Bearer challenge 获取 token。
    pub async fn get_token<T: OciTransport>(
        &self,
        transport: &T,
        challenge: &BearerChallenge,
    ) -> Result<Arc<str>, OciError> {
        self.get_token_for_challenge(transport, challenge, false)
            .await
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
            return Err(OciError::Token(crate::error::TokenError::ExchangeFailed {
                realm: challenge.realm.clone(),
                status,
            }));
        }

        let response: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| {
            OciError::Token(crate::error::TokenError::InvalidResponse {
                realm: challenge.realm.clone(),
                details: "invalid JSON token response",
            })
        })?;
        let token = response
            .token
            .or(response.access_token)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                OciError::Token(crate::error::TokenError::InvalidResponse {
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

#[cfg(not(loom))]
struct LeaderGuard {
    state: Arc<ChallengeState>,
    key: Option<ChallengeKey>,
}

#[cfg(not(loom))]
impl LeaderGuard {
    fn new(state: Arc<ChallengeState>, key: ChallengeKey) -> Self {
        Self {
            state,
            key: Some(key),
        }
    }

    fn finish(mut self) {
        if let Some(key) = self.key.take() {
            self.state
                .leaders
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&key);
            self.state.wake.notify_waiters();
        }
    }
}

#[cfg(not(loom))]
impl Drop for LeaderGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        self.state
            .leaders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        self.state.wake.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::TokenManager;
    use crate::{
        auth::BearerChallenge,
        backend::driver::{GhcrDriver, detect_driver},
        mock::{MockResponse, MockRouterTransport},
    };
    use bytes::Bytes;
    use http::{HeaderMap, StatusCode};
    use std::sync::{Arc, atomic::Ordering};

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
