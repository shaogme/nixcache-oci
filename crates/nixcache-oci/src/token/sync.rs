//! TokenManager 并发与同步原语抽象层

#[cfg(not(any(loom, feature = "loom")))]
mod imp {
    use crate::error::OciError;
    use arc_swap::ArcSwapOption;
    use std::{
        fmt,
        sync::{
            Arc,
            atomic::{AtomicU8, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::watch;
    use web_time::Instant;

    const STATE_IDLE: u8 = 0;
    const STATE_FETCHING: u8 = 1;

    #[derive(Clone)]
    pub struct InFlightState {
        inner: Arc<AtomicU8>,
    }

    impl fmt::Debug for InFlightState {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("InFlightState").finish_non_exhaustive()
        }
    }

    impl Default for InFlightState {
        fn default() -> Self {
            Self::new()
        }
    }

    impl InFlightState {
        pub fn new() -> Self {
            Self {
                inner: Arc::new(AtomicU8::new(STATE_IDLE)),
            }
        }

        pub fn try_acquire_leader(&self) -> bool {
            self.inner
                .compare_exchange(
                    STATE_IDLE,
                    STATE_FETCHING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        }

        pub fn release_leader(&self) {
            self.inner.store(STATE_IDLE, Ordering::Release);
        }
    }

    #[derive(Clone)]
    struct CachedToken {
        token: String,
        created_at: Instant,
    }

    #[derive(Clone)]
    pub struct TokenStorage {
        inner: Arc<ArcSwapOption<CachedToken>>,
    }

    impl fmt::Debug for TokenStorage {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("TokenStorage").finish_non_exhaustive()
        }
    }

    impl Default for TokenStorage {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TokenStorage {
        pub fn new() -> Self {
            Self {
                inner: Arc::new(ArcSwapOption::from(None)),
            }
        }

        pub fn load(&self) -> Option<String> {
            self.inner
                .load_full()
                .filter(|c| c.created_at.elapsed() < Duration::from_secs(240))
                .map(|c| c.token.clone())
        }

        pub fn store(&self, token: String) {
            self.inner.store(Some(Arc::new(CachedToken {
                token,
                created_at: Instant::now(),
            })));
        }
    }

    #[derive(Clone)]
    pub struct TokenBroadcaster {
        tx: Arc<watch::Sender<Option<String>>>,
        rx: watch::Receiver<Option<String>>,
    }

    impl fmt::Debug for TokenBroadcaster {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("TokenBroadcaster").finish_non_exhaustive()
        }
    }

    impl Default for TokenBroadcaster {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TokenBroadcaster {
        pub fn new() -> Self {
            let (tx, rx) = watch::channel(None);
            Self {
                tx: Arc::new(tx),
                rx,
            }
        }

        pub fn broadcast(&self, token: String) {
            let _ = self.tx.send(Some(token));
        }

        pub async fn wait(&self) -> Result<String, OciError> {
            let mut rx = self.rx.clone();
            if let Some(token) = rx.borrow().as_ref() {
                return Ok(token.clone());
            }
            if rx.changed().await.is_err() {
                return Err(OciError::AuthFailed);
            }
            rx.borrow().as_ref().cloned().ok_or(OciError::AuthFailed)
        }
    }
}

#[cfg(any(loom, feature = "loom"))]
mod imp {
    use crate::error::OciError;
    use loom::sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU8, Ordering},
    };
    use std::fmt;

    const STATE_IDLE: u8 = 0;
    const STATE_FETCHING: u8 = 1;

    #[derive(Clone)]
    pub struct InFlightState {
        inner: Arc<AtomicU8>,
    }

    impl fmt::Debug for InFlightState {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("InFlightState").finish_non_exhaustive()
        }
    }

    impl InFlightState {
        pub fn new() -> Self {
            Self {
                inner: Arc::new(AtomicU8::new(STATE_IDLE)),
            }
        }

        pub fn try_acquire_leader(&self) -> bool {
            self.inner
                .compare_exchange(
                    STATE_IDLE,
                    STATE_FETCHING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        }

        pub fn release_leader(&self) {
            self.inner.store(STATE_IDLE, Ordering::Release);
        }
    }

    #[derive(Clone)]
    pub struct TokenStorage {
        inner: Arc<Mutex<Option<String>>>,
    }

    impl fmt::Debug for TokenStorage {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("TokenStorage").finish_non_exhaustive()
        }
    }

    impl TokenStorage {
        pub fn new() -> Self {
            Self {
                inner: Arc::new(Mutex::new(None)),
            }
        }

        pub fn load(&self) -> Option<String> {
            self.inner.lock().unwrap().clone()
        }

        pub fn store(&self, token: String) {
            *self.inner.lock().unwrap() = Some(token);
        }
    }

    #[derive(Clone)]
    pub struct TokenBroadcaster {
        channel: Arc<(Mutex<Option<String>>, Condvar)>,
    }

    impl fmt::Debug for TokenBroadcaster {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("TokenBroadcaster").finish_non_exhaustive()
        }
    }

    impl TokenBroadcaster {
        pub fn new() -> Self {
            Self {
                channel: Arc::new((Mutex::new(None), Condvar::new())),
            }
        }

        pub fn broadcast(&self, token: String) {
            let (lock, cvar) = &*self.channel;
            let mut broadcast = lock.lock().unwrap();
            *broadcast = Some(token);
            cvar.notify_all();
        }

        pub async fn wait(&self) -> Result<String, OciError> {
            let (lock, cvar) = &*self.channel;
            let mut broadcast = lock.lock().unwrap();
            if let Some(val) = broadcast.clone() {
                return Ok(val);
            }
            broadcast = cvar.wait(broadcast).unwrap();
            broadcast.clone().ok_or(OciError::AuthFailed)
        }
    }
}

pub use imp::*;
