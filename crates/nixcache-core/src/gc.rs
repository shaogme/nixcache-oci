use crate::types::{CacheIndexData, IndexEntry};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

/// 垃圾回收评估结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcEvaluationResult {
    pub kept_entries: HashMap<String, IndexEntry>,
    pub deleted_hashes: Vec<String>,
    pub live_roots: HashSet<String>,
    pub cutoff_utc: String,
}

/// 纯函数：多架构可达性依赖图与保留期垃圾回收计算
pub fn evaluate_multi_arch_gc(
    index: &CacheIndexData,
    cutoff: &DateTime<Utc>,
) -> GcEvaluationResult {
    // 1. 跨多架构聚合所有活跃系统的 GC Roots
    let all_live_roots: HashSet<String> = index
        .gc_roots
        .values()
        .flat_map(|roots| roots.iter().cloned())
        .collect();

    let mut kept_entries = HashMap::new();
    let mut deleted_hashes = Vec::new();

    for (hash, entry) in &index.entries {
        let is_live = all_live_roots.contains(hash);
        let added_dt = DateTime::parse_from_rfc3339(&entry.added)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(*cutoff);

        let is_old = added_dt < *cutoff;

        if !is_live && is_old {
            deleted_hashes.push(hash.clone());
        } else {
            kept_entries.insert(hash.clone(), entry.clone());
        }
    }

    deleted_hashes.sort();

    GcEvaluationResult {
        kept_entries,
        deleted_hashes,
        live_roots: all_live_roots,
        cutoff_utc: cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}
