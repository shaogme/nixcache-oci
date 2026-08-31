use arc_swap::{ArcSwap, ArcSwapOption};
use nixcache_core::{
    DeltaPatchData, FastBlockedBloomFilter, IndexEntry, NarDigest, ShardDataPayload,
    ShardedArchCacheIndexData, StoreHash, build_nar_lookup_map,
};
use scc::HashMap as SccHashMap;
use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicUsize, Ordering},
    },
};
use worker::js_sys::Date;

pub const L1_MEM_TTL_MS: f64 = 30_000.0;

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
    pub hot_count: AtomicUsize,
    pub mem_session_cache: SccHashMap<String, Arc<CachedSessionEntry>>,
    pub mem_baseline_cache: ArcSwapOption<CachedBaselineEntry>,
    pub mem_shard_cache: SccHashMap<u16, Arc<CachedShardEntry>>,
    pub remote_status: ArcSwap<RemoteHealthState>,
}

static GLOBAL_STATE: LazyLock<WorkerState> = LazyLock::new(|| WorkerState {
    hot_entries: SccHashMap::new(),
    hot_nar_lookup: SccHashMap::new(),
    hot_count: AtomicUsize::new(0),
    mem_session_cache: SccHashMap::new(),
    mem_baseline_cache: ArcSwapOption::from(None),
    mem_shard_cache: SccHashMap::new(),
    remote_status: ArcSwap::from_pointee(RemoteHealthState::default()),
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

    /// 动态注册 Tier 0 热条目
    pub fn register_hot(&self, entries: HashMap<StoreHash, IndexEntry>) {
        if entries.is_empty() {
            return;
        }
        let nar_map = build_nar_lookup_map(&entries);
        let mut newly_added = 0;
        for (k, v) in entries {
            if self.hot_entries.upsert_sync(k, Arc::new(v)).is_none() {
                newly_added += 1;
            }
        }
        for (k, v) in nar_map {
            let _ = self.hot_nar_lookup.upsert_sync(k, v);
        }
        if newly_added > 0 {
            self.hot_count.fetch_add(newly_added, Ordering::Relaxed);
        }
    }

    /// 清空所有 L1 内存缓存
    pub fn clear_l1_caches(&self) {
        self.mem_session_cache.clear_sync();
        self.mem_baseline_cache.store(None);
        self.mem_shard_cache.clear_sync();
    }
}
