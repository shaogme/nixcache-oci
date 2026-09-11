//! TokenManager generation-flight 的 Loom 验证。
//!
//! Loom 只验证可观测的生命周期语义：等待者在 flight 完成后重新读取当前
//! cache，旧 generation 不能覆盖新 generation，也不能把历史通知值当 token。

#![cfg(loom)]

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use loom::{model::Builder, sync::atomic::Ordering, thread};
use nixcache_oci::{
    BearerChallenge, GenericOciDriver, MockResponse, MockRouterTransport, RegistryEndpoint,
    TokenManager,
    token::sync::{LoomTestAcquire, LoomTestRegistry},
};
use std::{
    future::Future,
    pin::pin,
    ptr,
    sync::Arc,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

fn challenge(scope: &str) -> BearerChallenge {
    BearerChallenge::new(
        "https://auth.example.test/token",
        Some("test.registry.io".to_string()),
        Some(format!("repository:test/repo/nix-cache:{scope}")),
    )
    .unwrap()
}

fn test_endpoint() -> RegistryEndpoint {
    RegistryEndpoint::parse("test.registry.io").unwrap()
}

fn loom_block_on<F: Future>(fut: F) -> F::Output {
    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
    let raw = RawWaker::new(ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut pinned = pin!(fut);
    loop {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::yield_now(),
        }
    }
}

fn transport_with_route(status: StatusCode, token: &str) -> Arc<MockRouterTransport> {
    let transport = Arc::new(MockRouterTransport::default());
    transport.add_route(
        "GET",
        "/token",
        MockResponse {
            status,
            headers: HeaderMap::new(),
            body: if status.is_success() {
                Bytes::from(format!(r#"{{"token":"{token}","expires_in":300}}"#))
            } else {
                Bytes::new()
            },
        },
    );
    transport
}

#[test]
fn loom_verify_same_challenge_has_one_current_leader() {
    loom::model(|| {
        let transport = transport_with_route(StatusCode::OK, "loom-token");
        let endpoint = test_endpoint();
        let manager = Arc::new(TokenManager::new(
            &endpoint,
            "test/repo",
            "secret",
            false,
            GenericOciDriver,
        ));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let manager = Arc::clone(&manager);
                let transport = Arc::clone(&transport);
                thread::spawn(move || {
                    loom_block_on(manager.get_token(&*transport, &challenge("pull")))
                })
            })
            .collect();

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect();
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|token| token.as_ref() == "loom-token"));
    });
}

#[test]
fn loom_verify_different_challenges_do_not_share_flight_or_cache() {
    let mut builder = Builder::new();
    builder.preemption_bound = Some(4);
    builder.check(|| {
        let transport = transport_with_route(StatusCode::OK, "scoped-token");
        let endpoint = test_endpoint();
        let manager = Arc::new(TokenManager::new(
            &endpoint,
            "test/repo",
            "secret",
            false,
            GenericOciDriver,
        ));
        let first = {
            let manager = Arc::clone(&manager);
            let transport = Arc::clone(&transport);
            thread::spawn(move || loom_block_on(manager.get_token(&*transport, &challenge("pull"))))
        };
        let second = {
            let manager = Arc::clone(&manager);
            let transport = Arc::clone(&transport);
            thread::spawn(move || loom_block_on(manager.get_token(&*transport, &challenge("push"))))
        };
        assert_eq!(first.join().unwrap().unwrap().as_ref(), "scoped-token");
        assert_eq!(second.join().unwrap().unwrap().as_ref(), "scoped-token");
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn loom_verify_failed_flight_wakes_waiter_without_stale_result() {
    loom::model(|| {
        let transport = transport_with_route(StatusCode::INTERNAL_SERVER_ERROR, "unused");
        let endpoint = test_endpoint();
        let manager = Arc::new(TokenManager::new(
            &endpoint,
            "test/repo",
            "secret",
            false,
            GenericOciDriver,
        ));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let manager = Arc::clone(&manager);
                let transport = Arc::clone(&transport);
                thread::spawn(move || {
                    loom_block_on(manager.get_token(&*transport, &challenge("pull"))).is_ok()
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(results.iter().all(|ok| !ok));
        assert!(transport.call_count.load(Ordering::SeqCst) <= 2);
    });
}

#[test]
fn loom_verify_refresh_starts_new_generation() {
    loom::model(|| {
        let transport = Arc::new(MockRouterTransport::default());
        transport.add_token_response(MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(r#"{"token":"generation-0","expires_in":300}"#),
        });
        transport.add_token_response(MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(r#"{"token":"generation-1","expires_in":300}"#),
        });
        let endpoint = test_endpoint();
        let manager = TokenManager::new(&endpoint, "test/repo", "secret", false, GenericOciDriver);
        let first = loom_block_on(manager.get_token(&*transport, &challenge("pull"))).unwrap();
        let refreshed =
            loom_block_on(manager.refresh_token(&*transport, &challenge("pull"))).unwrap();
        assert_eq!(first.as_ref(), "generation-0");
        assert_eq!(refreshed.as_ref(), "generation-1");
        assert_ne!(first, refreshed);
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn loom_verify_dropped_leader_releases_current_flight() {
    loom::model(|| {
        let registry = LoomTestRegistry::new();
        let leader = match registry.acquire() {
            LoomTestAcquire::Leader(leader) => leader,
            LoomTestAcquire::Waiter => panic!("first caller must become leader"),
        };
        drop(leader);

        assert!(matches!(registry.acquire(), LoomTestAcquire::Leader(_)));
    });
}

#[test]
fn loom_verify_logical_expiry_requires_a_new_fetch() {
    loom::model(|| {
        let registry = LoomTestRegistry::new();
        let leader = match registry.acquire() {
            LoomTestAcquire::Leader(leader) => leader,
            LoomTestAcquire::Waiter => panic!("first caller must become leader"),
        };
        assert!(leader.store(&registry, "before-expiry", 1));
        let generation = leader.generation();
        leader.finish();
        assert_eq!(registry.load().as_deref(), Some("before-expiry"));

        registry.advance(1);
        assert!(registry.load().is_none());
        let next = match registry.acquire() {
            LoomTestAcquire::Leader(leader) => leader,
            LoomTestAcquire::Waiter => panic!("expired cache must not create a waiter"),
        };
        assert_eq!(next.generation(), generation);
        drop(next);
    });
}

#[test]
fn loom_verify_old_guard_cannot_remove_new_generation_flight() {
    loom::model(|| {
        let registry = LoomTestRegistry::new();
        let old = match registry.acquire() {
            LoomTestAcquire::Leader(leader) => leader,
            LoomTestAcquire::Waiter => panic!("first caller must become leader"),
        };
        let old_generation = old.generation();
        registry.invalidate();
        assert!(!old.store(&registry, "stale", 10));

        let new = match registry.acquire() {
            LoomTestAcquire::Leader(leader) => leader,
            LoomTestAcquire::Waiter => panic!("invalidated flight must be replaced"),
        };
        assert_eq!(new.generation(), old_generation + 1);
        drop(old);
        assert!(matches!(registry.acquire(), LoomTestAcquire::Waiter));
        drop(new);
    });
}
