//! TokenManager Loom 形式化并发验证集成测试
//!
//! 在 Loom 确定性调度器下遍历所有线程交错调度分支，形式化证明：
//! 1. 单飞互斥性（Singleflight Invariant）：并发访问未命中缓存时至多 1 次网络请求
//! 2. 零死锁（Deadlock-Free）：线程调度在有限步内收敛，无死锁与挂起
//! 3. 内存一致性（Data Race Freedom & Memory Ordering）：无数据竞争与乱序读写
//! 4. 故障回退与广播同步（Fault Tolerance & Broadcast Sync）：网络失败时 Fallback 状态机正确传播并复位
//! 5. 同步原语级验证（Primitive Correctness）：针对 InFlightState、TokenStorage、TokenBroadcaster 单独进行模型检验

#![cfg(any(loom, feature = "loom"))]

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use loom::{model::Builder, sync::atomic::Ordering, thread};
use nixcache_oci::{
    GenericOciDriver, MockResponse, MockRouterTransport, TokenManager,
    token::sync::{InFlightState, TokenBroadcaster, TokenStorage},
};
use std::{
    future::Future,
    pin::pin,
    ptr,
    sync::Arc,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

/// 轻量级 Loom 兼容的 Future 轮询驱动器
fn loom_block_on<F: Future>(fut: F) -> F::Output {
    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
    let raw = RawWaker::new(ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut pinned = pin!(fut);
    loop {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => thread::yield_now(),
        }
    }
}

/// 辅助函数：构造返回成功 JWT 的 Mock 传输层
fn make_success_transport(token: &str) -> Arc<MockRouterTransport> {
    let transport = Arc::new(MockRouterTransport::default());
    transport.add_route(
        "GET",
        "/token",
        MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(format!(r#"{{"token": "{}"}}"#, token)),
        },
    );
    transport
}

/// 辅助函数：构造返回 500 错误的 Mock 传输层
fn make_error_transport() -> Arc<MockRouterTransport> {
    let transport = Arc::new(MockRouterTransport::default());
    transport.add_route(
        "GET",
        "/token",
        MockResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    transport
}

// =========================================================================
// 1. TokenManager 端到端并发模型检验
// =========================================================================

/// 场景 1: 双线程并发访问未命中缓存（Singleflight 单飞互斥性）
#[test]
fn loom_verify_token_manager_singleflight_invariant() {
    loom::model(|| {
        let transport = make_success_transport("loom-jwt-token");
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            GenericOciDriver,
        ));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let mgr = token_mgr.clone();
                let tr = transport.clone();
                thread::spawn(move || loom_block_on(mgr.get_token(&*tr)).unwrap())
            })
            .collect();

        let results: Vec<Arc<str>> = threads.into_iter().map(|t| t.join().unwrap()).collect();

        // 验证 1: 严格单飞，并发请求下网络 fetch 发生且仅发生 1 次
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);

        // 验证 2: 内存一致性，所有线程获取结果完全一致
        assert_eq!(results[0].as_ref(), "loom-jwt-token");
        assert_eq!(results[1].as_ref(), "loom-jwt-token");
    });
}

/// 场景 2: 三线程并发竞争风暴（1 Leader + 2 Followers 广播分发）
#[test]
fn loom_verify_token_manager_three_threads_storm() {
    let mut builder = Builder::new();
    builder.preemption_bound = Some(6);
    builder.check(|| {
        let transport = make_success_transport("three-threads-jwt");
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            GenericOciDriver,
        ));

        let threads: Vec<_> = (0..3)
            .map(|_| {
                let mgr = token_mgr.clone();
                let tr = transport.clone();
                thread::spawn(move || loom_block_on(mgr.get_token(&*tr)).unwrap())
            })
            .collect();

        let results: Vec<Arc<str>> = threads.into_iter().map(|t| t.join().unwrap()).collect();

        // 验证 1: 3 线程争抢下网络 fetch 依然严格仅有 1 次
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);

        // 验证 2: 所有 3 个线程全部被唤醒且数据一致
        for res in &results {
            assert_eq!(res.as_ref(), "three-threads-jwt");
        }
    });
}

/// 场景 3: 缓存命中快路径并发读取（Fast-Path 0 锁并发性）
#[test]
fn loom_verify_token_manager_fast_path_cached() {
    loom::model(|| {
        let transport = make_success_transport("fast-path-token");
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            GenericOciDriver,
        ));

        // 预热：首次调用填充缓存
        let initial_tok = loom_block_on(token_mgr.get_token(&*transport)).unwrap();
        assert_eq!(initial_tok.as_ref(), "fast-path-token");
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);

        // 2 个线程并发调用已缓存的 TokenManager
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let mgr = token_mgr.clone();
                let tr = transport.clone();
                thread::spawn(move || loom_block_on(mgr.get_token(&*tr)).unwrap())
            })
            .collect();

        let results: Vec<Arc<str>> = threads.into_iter().map(|t| t.join().unwrap()).collect();

        // 验证: 快路径 0 争用直接返回，无额外网络调用
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(results[0].as_ref(), "fast-path-token");
        assert_eq!(results[1].as_ref(), "fast-path-token");
    });
}

/// 场景 4: 网络故障与回退广播（Fallback Token 同步与状态机复位）
#[test]
fn loom_verify_token_manager_fallback_on_network_failure() {
    loom::model(|| {
        let transport = make_error_transport();
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "github_fallback_key",
            false,
            GenericOciDriver,
        ));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let mgr = token_mgr.clone();
                let tr = transport.clone();
                thread::spawn(move || loom_block_on(mgr.get_token(&*tr)).unwrap())
            })
            .collect();

        let results: Vec<Arc<str>> = threads.into_iter().map(|t| t.join().unwrap()).collect();

        // 验证 1: 发生 1 次或 2 次网络请求失败（并发单飞为 1 次，串行重试为 2 次）
        let calls = transport.call_count.load(Ordering::SeqCst);
        assert!(calls == 1 || calls == 2);

        // 验证 2: 所有线程安全回退到 fallback token，无死锁
        assert_eq!(results[0].as_ref(), "github_fallback_key");
        assert_eq!(results[1].as_ref(), "github_fallback_key");
    });
}

/// 场景 5: 多世代连续并发获取（Sequential Generations 验证）
#[test]
fn loom_verify_token_manager_multi_generation_sequential() {
    loom::model(|| {
        let transport = make_success_transport("multi-gen-token");
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "secret_tok",
            false,
            GenericOciDriver,
        ));

        // 第 1 轮并发获取
        let t1 = {
            let mgr = token_mgr.clone();
            let tr = transport.clone();
            thread::spawn(move || loom_block_on(mgr.get_token(&*tr)).unwrap())
        };
        let t2 = {
            let mgr = token_mgr.clone();
            let tr = transport.clone();
            thread::spawn(move || loom_block_on(mgr.get_token(&*tr)).unwrap())
        };
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        assert_eq!(r1.as_ref(), "multi-gen-token");
        assert_eq!(r2.as_ref(), "multi-gen-token");

        // 第 2 轮并发获取
        let t3 = {
            let mgr = token_mgr.clone();
            let tr = transport.clone();
            thread::spawn(move || loom_block_on(mgr.get_token(&*tr)).unwrap())
        };
        let r3 = t3.join().unwrap();
        assert_eq!(r3.as_ref(), "multi-gen-token");

        // 网络调用总次数依然为 1
        assert_eq!(transport.call_count.load(Ordering::SeqCst), 1);
    });
}

/// 场景 6: 空回退令牌在网络故障下的并发处理
#[test]
fn loom_verify_token_manager_empty_github_token_fallback() {
    loom::model(|| {
        let transport = make_error_transport();
        let token_mgr = Arc::new(TokenManager::new(
            "test.registry.io",
            "test/repo",
            "",
            false,
            GenericOciDriver,
        ));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let mgr = token_mgr.clone();
                let tr = transport.clone();
                thread::spawn(move || loom_block_on(mgr.get_token(&*tr)).unwrap())
            })
            .collect();

        let results: Vec<Arc<str>> = threads.into_iter().map(|t| t.join().unwrap()).collect();

        // 验证: 即使无 token 且网络失败，所有线程均获得空字符串，不产生 panic 或死锁
        assert_eq!(results[0].as_ref(), "");
        assert_eq!(results[1].as_ref(), "");
    });
}

// =========================================================================
// 2. 同步原语模型检验 (Primitives Formal Verification)
// =========================================================================

/// 场景 7: InFlightState CAS 互斥性与生命周期复位验证
#[test]
fn loom_verify_inflight_state_cas_mutual_exclusion() {
    loom::model(|| {
        let inflight = Arc::new(InFlightState::new());

        let t1 = {
            let state = inflight.clone();
            thread::spawn(move || state.try_acquire_leader())
        };
        let t2 = {
            let state = inflight.clone();
            thread::spawn(move || state.try_acquire_leader())
        };

        let res1 = t1.join().unwrap();
        let res2 = t2.join().unwrap();

        // 互斥性验证: 任意交错下有且仅有 1 个线程成功获得 Leader 权限
        assert_ne!(res1, res2);
        assert_eq!(res1 as u8 + res2 as u8, 1);

        // 释放 Leader
        inflight.release_leader();

        // 释放后应能够再次成功竞选 Leader
        assert!(inflight.try_acquire_leader());
        inflight.release_leader();
    });
}

/// 场景 8: InFlightState 3 线程并发争抢 Leader
#[test]
fn loom_verify_inflight_state_three_threads_contention() {
    loom::model(|| {
        let inflight = Arc::new(InFlightState::new());

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let state = inflight.clone();
                thread::spawn(move || state.try_acquire_leader())
            })
            .collect();

        let wins: usize = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&won| won)
            .count();

        // 严格保证有且仅有 1 位获胜者
        assert_eq!(wins, 1);

        inflight.release_leader();
    });
}

/// 场景 9: TokenStorage 并发读写内存一致性
#[test]
fn loom_verify_token_storage_concurrent_load_store() {
    loom::model(|| {
        let storage = Arc::new(TokenStorage::new());

        let reader = {
            let s = storage.clone();
            thread::spawn(move || s.load())
        };

        let writer = {
            let s = storage.clone();
            thread::spawn(move || {
                s.store("stored_token");
            })
        };

        let read_val = reader.join().unwrap();
        writer.join().unwrap();

        // 验证读取结果要么是 None（存储前读取），要么是 Some("stored_token")（存储后读取）
        if let Some(val) = read_val {
            assert_eq!(val.as_ref(), "stored_token");
        }

        // 最终状态必为 Some("stored_token")
        assert_eq!(storage.load().as_deref(), Some("stored_token"));
    });
}

/// 场景 10: TokenBroadcaster 广播与并发等待者唤醒
#[test]
fn loom_verify_token_broadcaster_wait_and_broadcast() {
    loom::model(|| {
        let broadcaster = Arc::new(TokenBroadcaster::new());

        let w1 = {
            let b = broadcaster.clone();
            thread::spawn(move || loom_block_on(b.wait()).unwrap())
        };

        let w2 = {
            let b = broadcaster.clone();
            thread::spawn(move || loom_block_on(b.wait()).unwrap())
        };

        let sender = {
            let b = broadcaster.clone();
            thread::spawn(move || {
                b.broadcast("broadcast_val");
            })
        };

        let res1 = w1.join().unwrap();
        let res2 = w2.join().unwrap();
        sender.join().unwrap();

        assert_eq!(res1.as_ref(), "broadcast_val");
        assert_eq!(res2.as_ref(), "broadcast_val");
    });
}

/// 场景 11: TokenBroadcaster 先广播后等待（Pre-broadcast）
#[test]
fn loom_verify_token_broadcaster_pre_broadcast() {
    loom::model(|| {
        let broadcaster = TokenBroadcaster::new();
        broadcaster.broadcast("pre_broadcast_val");

        let res = loom_block_on(broadcaster.wait()).unwrap();
        assert_eq!(res.as_ref(), "pre_broadcast_val");
    });
}
