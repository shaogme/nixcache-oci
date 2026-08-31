use arc_swap::ArcSwapOption;
use nixcache_core::{
    DeltaPatchData, FastBlockedBloomFilter, IndexEntry, NarDigest, ShardDataPayload,
    ShardedArchCacheIndexData, StoreHash, build_nar_lookup_map,
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

#[derive(Clone, Debug)]
pub struct CachedSessionEntry {
    pub delta: DeltaPatchData,
    pub nar_lookup: HashMap<String, NarDigest>,
    pub expires_at: f64,
}

#[derive(Clone, Debug)]
pub struct CachedBaselineEntry {
    pub root: ShardedArchCacheIndexData,
    pub bloom_filter: Arc<FastBlockedBloomFilter>,
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

/// 收敛的 Worker 全局内存状态 (Schema v5 SMRI with Bloom Filter)
pub struct WorkerState {
    pub hot_entries: SccHashMap<StoreHash, Arc<IndexEntry>>,
    pub hot_nar_lookup: SccHashMap<String, NarDigest>,
    pub mem_session_cache: SccHashMap<String, Arc<CachedSessionEntry>>,
    pub mem_baseline_cache: ArcSwapOption<CachedBaselineEntry>,
    pub mem_shard_cache: SccHashMap<u16, Arc<CachedShardEntry>>,
    pub last_ghcr_check_ms: AtomicU64,
}

static GLOBAL_STATE: LazyLock<WorkerState> = LazyLock::new(|| WorkerState {
    hot_entries: SccHashMap::new(),
    hot_nar_lookup: SccHashMap::new(),
    mem_session_cache: SccHashMap::new(),
    mem_baseline_cache: ArcSwapOption::from(None),
    mem_shard_cache: SccHashMap::new(),
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
        self.mem_shard_cache.clear_sync();
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
