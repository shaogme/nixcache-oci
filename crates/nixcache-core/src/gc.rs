use crate::types::{CacheIndexData, IndexEntry, StoreHash};
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

/// 纯函数：多架构可达性依赖图与保留期垃圾回收计算 (基于广度优先遍历传递闭包)
pub fn evaluate_multi_arch_gc(
    index: &CacheIndexData,
    cutoff: &DateTime<Utc>,
) -> GcEvaluationResult {
    // 1. 跨所有架构系统收集顶层活跃根节点
    let mut initial_roots = HashSet::new();
    for roots in index.gc_roots.values() {
        initial_roots.extend(roots.iter().cloned());
    }

    // 2. 构建有向可达图并计算完整传递闭包 (Closure Traversal)
    let mut reachable: HashSet<StoreHash> = HashSet::new();
    let mut queue: VecDeque<StoreHash> = initial_roots.into_iter().collect();

    while let Some(current_hash) = queue.pop_front() {
        if reachable.insert(current_hash.clone())
            && let Some(entry) = index.entries.get(&current_hash)
        {
            for dep_hash in &entry.narinfo_meta.references {
                if !reachable.contains(dep_hash) {
                    queue.push_back(dep_hash.clone());
                }
            }
        }
    }

    // 3. 判定保留与删除 (在闭包中 OR 新于保留期截止时间)
    let mut kept_entries = HashMap::new();
    let mut deleted_hashes = Vec::new();

    for (hash, entry) in &index.entries {
        let is_reachable = reachable.contains(hash);
        let added_dt = DateTime::parse_from_rfc3339(&entry.added)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(*cutoff);
        let is_expired = added_dt < *cutoff;

        if !is_reachable && is_expired {
            deleted_hashes.push(hash.clone());
        } else {
            kept_entries.insert(hash.clone(), entry.clone());
        }
    }

    deleted_hashes.sort();

    GcEvaluationResult {
        kept_entries,
        deleted_hashes,
        reachable_roots: reachable,
        cutoff_utc: cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}
