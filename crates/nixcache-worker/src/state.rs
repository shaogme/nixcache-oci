use arc_swap::ArcSwapOption;
use nixcache_core::{
    CacheIndexData, IndexEntry, NarDigest, RunSessionManifest, StoreHash, build_nar_lookup_map,
};
use scc::HashMap as SccHashMap;
use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};

pub const L1_MEM_TTL_MS: f64 = 10_000.0;
pub const DEBOUNCE_THRESHOLD_MS: f64 = 500.0;

/// 收敛的 Worker 全局内存状态
pub struct WorkerState {
    pub hot_entries: SccHashMap<StoreHash, Arc<IndexEntry>>,
    pub hot_nar_lookup: SccHashMap<String, NarDigest>,
    pub mem_session_cache:
        SccHashMap<String, Arc<(RunSessionManifest, HashMap<String, NarDigest>, f64)>>,
    pub mem_baseline_cache: ArcSwapOption<(CacheIndexData, HashMap<String, NarDigest>, f64)>,
    pub last_ghcr_check_ms: AtomicU64,
}

static GLOBAL_STATE: LazyLock<WorkerState> = LazyLock::new(|| WorkerState {
    hot_entries: SccHashMap::new(),
    hot_nar_lookup: SccHashMap::new(),
    mem_session_cache: SccHashMap::new(),
    mem_baseline_cache: ArcSwapOption::from(None),
    last_ghcr_check_ms: AtomicU64::new(0),
});

impl WorkerState {
    pub fn global() -> &'static Self {
        &GLOBAL_STATE
    }

    /// 动态注册 Tier 0 热条目
    pub fn register_hot(&self, entries: HashMap<StoreHash, IndexEntry>) {
        if entries.is_empty() {
            return;
        }
        let nar_map = build_nar_lookup_map(&entries);
        for (k, v) in entries {
            let _ = self.hot_entries.upsert_sync(k, Arc::new(v));
        }
        for (k, v) in nar_map {
            let _ = self.hot_nar_lookup.upsert_sync(k, v);
        }
    }

    /// 清空所有 L1 内存缓存
    pub fn clear_l1_caches(&self) {
        self.mem_session_cache.clear_sync();
        self.mem_baseline_cache.store(None);
    }

    /// 原子抢占 GHCR 刷新权限
    pub fn try_acquire_ghcr_check(&self, now_ms: u64, debounce_ms: u64) -> bool {
        let last = self.last_ghcr_check_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) > debounce_ms {
            self.last_ghcr_check_ms
                .compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        } else {
            false
        }
    }
}
