use crate::{
    filter::{CacheSelector, CascadeMode, TimeFilter},
    purge::evaluate_cache_purge,
    types::{CacheIndexData, IndexEntry, StoreHash},
};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet, VecDeque};

/// 垃圾回收评估结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcEvaluationResult {
    pub kept_entries: HashMap<StoreHash, IndexEntry>,
    pub deleted_hashes: Vec<StoreHash>,
    pub reachable_roots: HashSet<StoreHash>,
    pub cutoff_utc: String,
}

/// 纯函数：多架构可达性依赖图与保留期垃圾回收计算 (直接复用统一的 selector/purge 评估引擎)
pub fn evaluate_multi_arch_gc(
    index: &CacheIndexData,
    cutoff: &DateTime<Utc>,
) -> GcEvaluationResult {
    let selector = CacheSelector {
        time_filter: Some(TimeFilter::Before(*cutoff)),
        protect_gc_roots: true,
        cascade_mode: CascadeMode::Exact,
        ..Default::default()
    };

    let purge_res = evaluate_cache_purge(index, &selector);

    // 计算跨所有架构系统收集的顶层活跃根及传递可达集合
    let mut initial_roots = HashSet::new();
    for roots in index.gc_roots.values() {
        initial_roots.extend(roots.iter().cloned());
    }

    let mut reachable: HashSet<StoreHash> = HashSet::new();
    let mut queue: VecDeque<StoreHash> = initial_roots.into_iter().collect();

    while let Some(current_hash) = queue.pop_front() {
        if reachable.insert(current_hash.clone())
            && let Some(entry) = index.entries.get(&current_hash)
        {
            for dep_hash in entry.narinfo_meta.reference_hashes() {
                if !reachable.contains(&dep_hash) {
                    queue.push_back(dep_hash);
                }
            }
        }
    }

    GcEvaluationResult {
        kept_entries: purge_res.kept_entries,
        deleted_hashes: purge_res.purged_hashes,
        reachable_roots: reachable,
        cutoff_utc: cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}
