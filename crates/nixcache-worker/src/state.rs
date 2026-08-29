use nixcache_core::{
    CacheIndexData, IndexEntry, NarDigest, RunSessionManifest, StoreHash, build_nar_lookup_map,
};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

pub const L1_MEM_TTL_MS: f64 = 10_000.0;
pub const DEBOUNCE_THRESHOLD_MS: f64 = 500.0;

/// 收敛的 Worker 全局内存状态
pub struct WorkerState {
    pub hot_entries: HashMap<StoreHash, IndexEntry>,
    pub hot_nar_lookup: HashMap<String, NarDigest>,
    pub mem_session_cache: HashMap<String, (RunSessionManifest, HashMap<String, NarDigest>, f64)>,
    pub mem_baseline_cache: Option<(CacheIndexData, HashMap<String, NarDigest>, f64)>,
    pub last_ghcr_check: f64,
}

impl Default for WorkerState {
    fn default() -> Self {
        Self {
            hot_entries: HashMap::new(),
            hot_nar_lookup: HashMap::new(),
            mem_session_cache: HashMap::new(),
            mem_baseline_cache: None,
            last_ghcr_check: 0.0,
        }
    }
}

static STATE: LazyLock<Mutex<WorkerState>> = LazyLock::new(|| {
    Mutex::new(WorkerState {
        hot_entries: HashMap::new(),
        hot_nar_lookup: HashMap::new(),
        mem_session_cache: HashMap::new(),
        mem_baseline_cache: None,
        last_ghcr_check: 0.0,
    })
});

impl WorkerState {
    /// 获取全局状态锁
    pub fn global() -> &'static Mutex<Self> {
        &STATE
    }

    /// 动态注册 Tier 0 热条目
    pub fn register_hot(&mut self, entries: HashMap<StoreHash, IndexEntry>) {
        if entries.is_empty() {
            return;
        }
        let nar_map = build_nar_lookup_map(&entries);
        self.hot_entries.extend(entries);
        self.hot_nar_lookup.extend(nar_map);
    }

    /// 清空所有 L1 内存缓存
    pub fn clear_l1_caches(&mut self) {
        self.mem_session_cache.clear();
        self.mem_baseline_cache = None;
    }
}
