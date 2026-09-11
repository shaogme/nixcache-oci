//! Token single-flight 的代际状态机和同步适配器。
//!
//! 该模块只传播 flight 的完成状态。token 永远只从带有当前 generation
//! 且未过期的 cache 读取，通知本身不保存也不携带 token。

use std::{collections::HashMap, sync::Arc as TokenArc};

#[cfg(loom)]
use loom::sync::{Arc as SharedArc, Condvar, Mutex as StateMutex, MutexGuard};

#[cfg(not(loom))]
use std::sync::{Arc as SharedArc, Mutex as StateMutex, MutexGuard};

#[cfg(not(loom))]
use tokio::sync::Notify;

use web_time::Instant;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ChallengeKey {
    pub(super) realm: String,
    pub(super) service: String,
    pub(super) scope: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FlightOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub(super) struct CachedToken {
    pub(super) token: TokenArc<str>,
    pub(super) generation: u64,
    pub(super) expires_at: Instant,
}

pub(super) enum Acquire {
    Leader(LeaderGuard),
    Waiter(FlightWaiter),
}

struct FlightState {
    outcome: Option<FlightOutcome>,
}

#[cfg(not(loom))]
struct Flight {
    state: StateMutex<FlightState>,
    notify: Notify,
}

#[cfg(loom)]
struct Flight {
    channel: SharedArc<(StateMutex<FlightState>, Condvar)>,
}

impl Flight {
    #[cfg(not(loom))]
    fn new() -> Self {
        Self {
            state: StateMutex::new(FlightState { outcome: None }),
            notify: Notify::new(),
        }
    }

    #[cfg(loom)]
    fn new() -> Self {
        Self {
            channel: SharedArc::new((
                StateMutex::new(FlightState { outcome: None }),
                Condvar::new(),
            )),
        }
    }

    #[cfg(not(loom))]
    fn state(&self) -> MutexGuard<'_, FlightState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(loom)]
    fn state(&self) -> MutexGuard<'_, FlightState> {
        self.channel.0.lock().unwrap()
    }

    fn outcome(&self) -> Option<FlightOutcome> {
        self.state().outcome
    }

    fn complete(&self, outcome: FlightOutcome) {
        #[cfg(not(loom))]
        {
            let changed = {
                let mut state = self.state();
                if state.outcome.is_none() {
                    state.outcome = Some(outcome);
                    true
                } else {
                    false
                }
            };
            if changed {
                self.notify.notify_waiters();
            }
        }

        #[cfg(loom)]
        {
            let mut state = self.state();
            if state.outcome.is_none() {
                state.outcome = Some(outcome);
                self.channel.1.notify_all();
            }
        }
    }

    #[cfg(not(loom))]
    async fn wait(&self) -> FlightOutcome {
        let notified = self.notify.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        if let Some(outcome) = self.outcome() {
            return outcome;
        }
        notified.await;
        self.outcome().unwrap_or(FlightOutcome::Cancelled)
    }

    #[cfg(loom)]
    async fn wait(&self) -> FlightOutcome {
        let mut state = self.state();
        while state.outcome.is_none() {
            state = self.channel.1.wait(state).unwrap();
        }
        state.outcome.unwrap_or(FlightOutcome::Cancelled)
    }
}

#[derive(Clone)]
pub(super) struct FlightRegistry {
    inner: SharedArc<StateMutex<RegistryState>>,
}

struct RegistryState {
    cache: HashMap<ChallengeKey, CachedToken>,
    flights: HashMap<ChallengeKey, SharedArc<Flight>>,
    generations: HashMap<ChallengeKey, u64>,
    last_key: Option<ChallengeKey>,
}

impl FlightRegistry {
    pub(super) fn new() -> Self {
        Self {
            inner: SharedArc::new(StateMutex::new(RegistryState {
                cache: HashMap::new(),
                flights: HashMap::new(),
                generations: HashMap::new(),
                last_key: None,
            })),
        }
    }

    #[cfg(not(loom))]
    fn state(&self) -> MutexGuard<'_, RegistryState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(loom)]
    fn state(&self) -> MutexGuard<'_, RegistryState> {
        self.inner.lock().unwrap()
    }

    pub(super) fn load(&self, key: &ChallengeKey, now: Instant) -> Option<TokenArc<str>> {
        let mut state = self.state();
        let generation = state.generations.get(key).copied().unwrap_or_default();
        let valid = state
            .cache
            .get(key)
            .is_some_and(|entry| entry.generation == generation && entry.expires_at > now);
        if valid {
            return state
                .cache
                .get(key)
                .map(|entry| TokenArc::clone(&entry.token));
        }
        state.cache.remove(key);
        None
    }

    pub(super) fn load_last(&self, now: Instant) -> Option<TokenArc<str>> {
        let mut state = self.state();
        let key = state.last_key.clone()?;
        let generation = state.generations.get(&key).copied().unwrap_or_default();
        let valid = state
            .cache
            .get(&key)
            .is_some_and(|entry| entry.generation == generation && entry.expires_at > now);
        if valid {
            return state
                .cache
                .get(&key)
                .map(|entry| TokenArc::clone(&entry.token));
        }
        state.cache.remove(&key);
        None
    }

    pub(super) fn acquire(&self, key: ChallengeKey) -> Acquire {
        let mut state = self.state();
        let generation = *state.generations.entry(key.clone()).or_default();
        if let Some(flight) = state.flights.get(&key) {
            return Acquire::Waiter(FlightWaiter {
                flight: SharedArc::clone(flight),
            });
        }

        let flight = SharedArc::new(Flight::new());
        state.flights.insert(key.clone(), SharedArc::clone(&flight));
        Acquire::Leader(LeaderGuard {
            registry: self.clone(),
            key: Some(key),
            generation,
            flight,
        })
    }

    pub(super) fn store_if_current(
        &self,
        key: ChallengeKey,
        generation: u64,
        token: TokenArc<str>,
        expires_at: Instant,
    ) -> bool {
        let mut state = self.state();
        let current_generation = state.generations.get(&key).copied().unwrap_or_default();
        if current_generation != generation {
            return false;
        }
        state.cache.insert(
            key.clone(),
            CachedToken {
                token,
                generation,
                expires_at,
            },
        );
        state.last_key = Some(key);
        true
    }

    pub(super) fn invalidate(&self, key: ChallengeKey) {
        let flight = {
            let mut state = self.state();
            let generation = state.generations.entry(key.clone()).or_default();
            *generation = generation.saturating_add(1);
            state.cache.remove(&key);
            state.flights.remove(&key)
        };
        if let Some(flight) = flight {
            flight.complete(FlightOutcome::Cancelled);
        }
    }

    fn finish(
        &self,
        key: ChallengeKey,
        generation: u64,
        flight: &SharedArc<Flight>,
        outcome: FlightOutcome,
    ) {
        let remove = {
            let mut state = self.state();
            let same_flight = state
                .flights
                .get(&key)
                .is_some_and(|current| SharedArc::ptr_eq(current, flight));
            let same_generation =
                state.generations.get(&key).copied().unwrap_or_default() == generation;
            same_flight && same_generation && state.flights.remove(&key).is_some()
        };
        if remove || flight.outcome().is_none() {
            flight.complete(outcome);
        }
    }
}

pub(super) struct FlightWaiter {
    flight: SharedArc<Flight>,
}

impl FlightWaiter {
    pub(super) async fn wait(self) -> FlightOutcome {
        self.flight.wait().await
    }
}

pub(super) struct LeaderGuard {
    registry: FlightRegistry,
    key: Option<ChallengeKey>,
    generation: u64,
    flight: SharedArc<Flight>,
}

impl LeaderGuard {
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn finish(mut self, outcome: FlightOutcome) {
        self.finish_inner(outcome);
    }

    fn finish_inner(&mut self, outcome: FlightOutcome) {
        let Some(key) = self.key.take() else {
            return;
        };
        self.registry
            .finish(key, self.generation, &self.flight, outcome);
    }
}

impl Drop for LeaderGuard {
    fn drop(&mut self) {
        self.finish_inner(FlightOutcome::Cancelled);
    }
}

pub(super) use Acquire::{Leader, Waiter};

#[cfg(loom)]
#[doc(hidden)]
#[derive(Clone)]
pub struct LoomTestRegistry {
    registry: FlightRegistry,
    key: ChallengeKey,
    base: Instant,
    tick: SharedArc<StateMutex<u64>>,
}

#[cfg(loom)]
impl LoomTestRegistry {
    pub fn new() -> Self {
        Self {
            registry: FlightRegistry::new(),
            key: ChallengeKey {
                realm: "https://auth.example.test/token".to_string(),
                service: "registry.example.test".to_string(),
                scope: "repository:test/repo:pull".to_string(),
            },
            base: Instant::now(),
            tick: SharedArc::new(StateMutex::new(0)),
        }
    }

    pub fn acquire(&self) -> LoomTestAcquire {
        match self.registry.acquire(self.key.clone()) {
            Acquire::Leader(leader) => LoomTestAcquire::Leader(LoomTestLeader { leader }),
            Acquire::Waiter(_) => LoomTestAcquire::Waiter,
        }
    }

    pub fn invalidate(&self) {
        self.registry.invalidate(self.key.clone());
    }

    pub fn advance(&self, ticks: u64) {
        let mut tick = self.tick.lock().unwrap();
        *tick += ticks;
    }

    pub fn load(&self) -> Option<String> {
        self.registry
            .load(&self.key, self.now())
            .map(|token| token.to_string())
    }

    fn now(&self) -> Instant {
        let tick = *self.tick.lock().unwrap();
        self.base + std::time::Duration::from_millis(tick)
    }

    fn store(&self, generation: u64, token: &str, ttl_ticks: u64) -> bool {
        self.registry.store_if_current(
            self.key.clone(),
            generation,
            TokenArc::from(token),
            self.now() + std::time::Duration::from_millis(ttl_ticks),
        )
    }
}

#[cfg(loom)]
#[doc(hidden)]
pub enum LoomTestAcquire {
    Leader(LoomTestLeader),
    Waiter,
}

#[cfg(loom)]
#[doc(hidden)]
pub struct LoomTestLeader {
    leader: LeaderGuard,
}

#[cfg(loom)]
impl LoomTestLeader {
    pub fn generation(&self) -> u64 {
        self.leader.generation
    }

    pub fn store(&self, registry: &LoomTestRegistry, token: &str, ttl_ticks: u64) -> bool {
        registry.store(self.generation(), token, ttl_ticks)
    }

    pub fn finish(self) {
        self.leader.finish(FlightOutcome::Succeeded);
    }
}

#[cfg(test)]
mod tests {
    use super::{Acquire, CachedToken, ChallengeKey, FlightOutcome, FlightRegistry};
    use std::{sync::Arc, time::Duration};
    use web_time::Instant;

    fn key(scope: &str) -> ChallengeKey {
        ChallengeKey {
            realm: "https://auth.example.test/token".to_string(),
            service: "registry.example.test".to_string(),
            scope: scope.to_string(),
        }
    }

    #[test]
    fn stale_generation_cannot_overwrite_current_cache() {
        let registry = FlightRegistry::new();
        let key = key("pull");
        let now = Instant::now();
        let leader = match registry.acquire(key.clone()) {
            Acquire::Leader(leader) => leader,
            Acquire::Waiter(_) => panic!("first caller must be leader"),
        };
        registry.invalidate(key.clone());
        assert!(!registry.store_if_current(
            key,
            leader.generation,
            Arc::from("old"),
            now + Duration::from_secs(300),
        ));
        leader.finish(FlightOutcome::Succeeded);
    }

    #[test]
    fn expired_cache_is_a_miss() {
        let registry = FlightRegistry::new();
        let key = key("pull");
        let now = Instant::now();
        assert!(registry.store_if_current(key.clone(), 0, Arc::from("expired"), now,));
        assert!(registry.load(&key, now).is_none());
        let state = registry.state();
        assert!(state.cache.is_empty());
    }

    #[test]
    fn guard_drop_does_not_remove_new_flight() {
        let registry = FlightRegistry::new();
        let key = key("pull");
        let old = match registry.acquire(key.clone()) {
            Acquire::Leader(leader) => leader,
            Acquire::Waiter(_) => panic!("first caller must be leader"),
        };
        registry.invalidate(key.clone());
        let new = match registry.acquire(key.clone()) {
            Acquire::Leader(leader) => leader,
            Acquire::Waiter(_) => panic!("invalidated caller must be leader"),
        };
        drop(old);
        assert!(matches!(registry.acquire(key), Acquire::Waiter(_)));
        drop(new);
    }

    #[test]
    fn cache_entry_carries_generation_and_expiry() {
        let entry = CachedToken {
            token: Arc::from("token"),
            generation: 3,
            expires_at: Instant::now() + Duration::from_secs(1),
        };
        assert_eq!(entry.generation, 3);
        assert!(entry.expires_at > Instant::now());
    }
}
