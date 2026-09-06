use arc_swap::{ArcSwap, ArcSwapOption};
use nixcache_core::{DeltaPatchData, NarDigest, ShardDataPayload, ShardedArchCacheIndexData};
use scc::HashMap as SccHashMap;
use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};
use worker::js_sys::Date;

pub const L1_MEM_TTL_MS: f64 = 30_000.0;
pub const REVALIDATE_DEBOUNCE_MS: u64 = 2_000;

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteHealthState {
    pub connected: bool,
    pub last_error: Option<String>,
    pub last_updated_ms: f64,
}

impl Default for RemoteHealthState {
    fn default() -> Self {
        Self {
            connected: true,
            last_error: None,
            last_updated_ms: 0.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CachedSessionEntry {
    pub delta: DeltaPatchData,
    pub nar_lookup: HashMap<String, NarDigest>,
    pub expires_at: f64,
}

#[derive(Clone, Debug)]
pub struct CachedBaselineEntry {
    pub root: ShardedArchCacheIndexData,
    pub manifest_digest: String,
    pub expires_at: f64,
}

#[derive(Clone, Debug)]
pub struct CachedShardEntry {
    pub payload: ShardDataPayload,
    pub nar_lookup: HashMap<String, NarDigest>,
    pub blob_digest: String,
    pub expires_at: f64,
}

/// 收敛的 Worker 全局内存状态 (Schema v6 SMRI with SWR Self-Healing)
pub struct WorkerState {
    pub mem_session_cache: SccHashMap<String, Arc<CachedSessionEntry>>,
    pub mem_baseline_cache: ArcSwapOption<CachedBaselineEntry>,
    pub mem_shard_cache: SccHashMap<u16, Arc<CachedShardEntry>>,
    pub remote_status: ArcSwap<RemoteHealthState>,
    pub last_revalidate_ms: AtomicU64,
}

static GLOBAL_STATE: LazyLock<WorkerState> = LazyLock::new(|| WorkerState {
    mem_session_cache: SccHashMap::new(),
    mem_baseline_cache: ArcSwapOption::from(None),
    mem_shard_cache: SccHashMap::new(),
    remote_status: ArcSwap::from_pointee(RemoteHealthState::default()),
    last_revalidate_ms: AtomicU64::new(0),
});

impl WorkerState {
    pub fn global() -> &'static Self {
        &GLOBAL_STATE
    }

    /// 更新远端 GHCR 健康度状态
    pub fn set_remote_status(&self, connected: bool, error: Option<String>) {
        self.remote_status.store(Arc::new(RemoteHealthState {
            connected,
            last_error: error,
            last_updated_ms: Date::now(),
        }));
    }

    /// 检查是否允许触发 Cache Miss 防抖自愈探查
    pub fn should_revalidate(&self) -> bool {
        let now = Date::now() as u64;
        let last = self.last_revalidate_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= REVALIDATE_DEBOUNCE_MS {
            self.last_revalidate_ms.store(now, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// 清空所有 L1 内存缓存
    pub fn clear_l1_caches(&self) {
        self.mem_session_cache.clear_sync();
        self.mem_baseline_cache.store(None);
        self.mem_shard_cache.clear_sync();
    }
}
