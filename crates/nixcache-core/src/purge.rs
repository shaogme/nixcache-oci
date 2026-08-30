use crate::{
    filter::{CacheQueryResult, CacheSelector, evaluate_cache_query},
    types::{IndexEntry, NarDigest, StoreHash, SystemArch},
};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeEvaluationResult {
    pub kept_entries: HashMap<StoreHash, IndexEntry>,
    pub purged_entries: HashMap<StoreHash, IndexEntry>,
    pub purged_hashes: Vec<StoreHash>,
    pub purged_nar_digests: Vec<NarDigest>,
    pub updated_gc_roots: HashMap<SystemArch, Vec<StoreHash>>,
    pub estimated_freed_bytes: u64,
    pub reason_map: HashMap<StoreHash, String>,
}

/// 净化并修剪失效/断链的 GC Roots (Prune broken roots)
pub fn prune_broken_gc_roots(
    entries: &HashMap<StoreHash, IndexEntry>,
    gc_roots: &HashMap<SystemArch, Vec<StoreHash>>,
    purged_hashes: &HashSet<StoreHash>,
) -> HashMap<SystemArch, Vec<StoreHash>> {
    let mut forward_graph: HashMap<StoreHash, Vec<StoreHash>> = HashMap::new();
    for (hash, entry) in entries {
        if !purged_hashes.contains(hash) {
            forward_graph.insert(
                hash.clone(),
                entry.narinfo_meta.reference_hashes().collect(),
            );
        }
    }

    let mut updated_gc_roots = HashMap::new();
    for (sys, roots) in gc_roots {
        let mut valid_roots = Vec::new();
        for root in roots {
            if purged_hashes.contains(root) {
                continue;
            }

            // 验证该 root 的所有传递依赖均存在于保留集合中
            let mut is_sound = true;
            let mut visited = HashSet::new();
            let mut q = VecDeque::new();
            q.push_back(root.clone());
            visited.insert(root.clone());

            while let Some(curr) = q.pop_front() {
                if let Some(deps) = forward_graph.get(&curr) {
                    for dep in deps {
                        if purged_hashes.contains(dep) || !entries.contains_key(dep) {
                            is_sound = false;
                            break;
                        }
                        if visited.insert(dep.clone()) {
                            q.push_back(dep.clone());
                        }
                    }
                } else if !entries.contains_key(&curr) {
                    is_sound = false;
                    break;
                }
                if !is_sound {
                    break;
                }
            }

            if is_sound {
                valid_roots.push(root.clone());
            }
        }
        if !valid_roots.is_empty() {
            updated_gc_roots.insert(*sys, valid_roots);
        }
    }

    updated_gc_roots
}

/// 单架构缓存清理与失效评估计算
pub fn evaluate_arch_cache_purge(
    entries: &HashMap<StoreHash, IndexEntry>,
    gc_roots: &[StoreHash],
    system: SystemArch,
    selector: &CacheSelector,
) -> PurgeEvaluationResult {
    let mut roots_map = HashMap::new();
    if !gc_roots.is_empty() {
        roots_map.insert(system, gc_roots.to_vec());
    }
    evaluate_cache_purge(entries, &roots_map, selector)
}

/// 纯函数：多维构建缓存清理与失效评估计算 (基于通用 evaluate_cache_query)
pub fn evaluate_cache_purge(
    entries: &HashMap<StoreHash, IndexEntry>,
    gc_roots: &HashMap<SystemArch, Vec<StoreHash>>,
    selector: &CacheSelector,
) -> PurgeEvaluationResult {
    let query_res: CacheQueryResult = evaluate_cache_query(entries, gc_roots, selector);

    let purged_hashes_set: HashSet<StoreHash> = query_res.matched_entries.keys().cloned().collect();
    let updated_gc_roots = prune_broken_gc_roots(entries, gc_roots, &purged_hashes_set);
    let purged_nar_digests: Vec<NarDigest> = query_res
        .matched_entries
        .values()
        .map(|e| e.nar_digest.clone())
        .collect();

    PurgeEvaluationResult {
        kept_entries: query_res.unmatched_entries,
        purged_entries: query_res.matched_entries,
        purged_hashes: query_res.final_matched_hashes,
        purged_nar_digests,
        updated_gc_roots,
        estimated_freed_bytes: query_res.matched_bytes,
        reason_map: query_res.reason_map,
    }
}
