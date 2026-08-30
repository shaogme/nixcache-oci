use crate::types::{CacheIndexData, IndexEntry, NarDigest, StoreHash, SystemArch};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CascadeMode {
    #[default]
    Exact,
    Dependents,
    Transitive,
    FullTree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeFilter {
    Before(DateTime<Utc>),
    After(DateTime<Utc>),
    Between(DateTime<Utc>, DateTime<Utc>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SizeFilter {
    MinBytes(u64),
    MaxBytes(u64),
    Range(u64, u64),
}

#[derive(Debug, Clone, Default)]
pub struct CachePurgeFilter {
    pub purge_all: bool,
    pub store_hashes: HashSet<StoreHash>,
    pub patterns: Vec<String>,
    pub systems: HashSet<SystemArch>,
    pub time_filter: Option<TimeFilter>,
    pub size_filter: Option<SizeFilter>,
    pub origin_jobs: HashSet<String>,
    pub origin_runs: HashSet<u64>,
    pub cascade_mode: CascadeMode,
    pub protect_gc_roots: bool,
}

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

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p_bytes = pattern.as_bytes();
    let t_bytes = text.as_bytes();
    let (p_len, t_len) = (p_bytes.len(), t_bytes.len());
    let mut p_idx = 0;
    let mut t_idx = 0;
    let mut star_idx = None;
    let mut match_idx = 0;

    while t_idx < t_len {
        if p_idx < p_len && (p_bytes[p_idx] == b'?' || p_bytes[p_idx] == t_bytes[t_idx]) {
            p_idx += 1;
            t_idx += 1;
        } else if p_idx < p_len && p_bytes[p_idx] == b'*' {
            star_idx = Some(p_idx);
            match_idx = t_idx;
            p_idx += 1;
        } else if let Some(star) = star_idx {
            p_idx = star + 1;
            match_idx += 1;
            t_idx = match_idx;
        } else {
            return false;
        }
    }

    while p_idx < p_len && p_bytes[p_idx] == b'*' {
        p_idx += 1;
    }

    p_idx == p_len
}

pub fn matches_pattern(pattern: &str, text: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        wildcard_match(pattern, text)
    } else {
        text.contains(pattern)
    }
}

/// 纯函数：多维构建缓存清理与失效评估计算
pub fn evaluate_cache_purge(
    index: &CacheIndexData,
    filter: &CachePurgeFilter,
) -> PurgeEvaluationResult {
    // 1. 构建正向依赖图 (A -> References) 与 反向依赖图 (B -> Dependents of B)
    let mut forward_graph: HashMap<StoreHash, Vec<StoreHash>> = HashMap::new();
    let mut reverse_graph: HashMap<StoreHash, Vec<StoreHash>> = HashMap::new();

    for (hash, entry) in &index.entries {
        let deps: Vec<StoreHash> = entry.narinfo_meta.reference_hashes().collect();
        for dep in &deps {
            reverse_graph
                .entry(dep.clone())
                .or_default()
                .push(hash.clone());
        }
        forward_graph.insert(hash.clone(), deps);
    }

    // 1.1 若启用了 protect_gc_roots，先计算活跃 GC Roots 的完整正向可达闭包
    let mut protected_reachable: HashSet<StoreHash> = HashSet::new();
    if filter.protect_gc_roots {
        let mut root_queue: VecDeque<StoreHash> = VecDeque::new();
        for (sys, roots) in &index.gc_roots {
            if filter.systems.is_empty() || filter.systems.contains(sys) {
                for root in roots {
                    root_queue.push_back(root.clone());
                }
            }
        }
        while let Some(curr) = root_queue.pop_front() {
            if protected_reachable.insert(curr.clone())
                && let Some(deps) = forward_graph.get(&curr)
            {
                for dep in deps {
                    if !protected_reachable.contains(dep) {
                        root_queue.push_back(dep.clone());
                    }
                }
            }
        }
    }

    // 1.2 无保护的全量清理快速路径
    if filter.purge_all && !filter.protect_gc_roots {
        let mut purged_entries = HashMap::new();
        let mut purged_hashes = Vec::new();
        let mut purged_nar_digests = Vec::new();
        let mut freed = 0u64;

        for (hash, entry) in &index.entries {
            purged_entries.insert(hash.clone(), entry.clone());
            purged_hashes.push(hash.clone());
            purged_nar_digests.push(entry.nar_digest.clone());
            freed += entry.nar_size;
        }

        purged_hashes.sort();
        return PurgeEvaluationResult {
            kept_entries: HashMap::new(),
            purged_entries,
            purged_hashes,
            purged_nar_digests,
            updated_gc_roots: HashMap::new(),
            estimated_freed_bytes: freed,
            reason_map: HashMap::new(),
        };
    }

    // 2. 第一轮：匹配初始命中目标 (Initial Matches)
    let mut initial_purged: HashSet<StoreHash> = HashSet::new();
    let mut reason_map: HashMap<StoreHash, String> = HashMap::new();

    let only_system_filter = !filter.systems.is_empty()
        && !filter.purge_all
        && filter.store_hashes.is_empty()
        && filter.patterns.is_empty()
        && filter.time_filter.is_none()
        && filter.size_filter.is_none()
        && filter.origin_jobs.is_empty()
        && filter.origin_runs.is_empty();

    for (hash, entry) in &index.entries {
        // 2.0 若受 GC Roots 保护且处于可达闭包中，绝对不作为初始清理目标
        if filter.protect_gc_roots && protected_reachable.contains(hash) {
            continue;
        }

        // 2.1 系统架构前置过滤
        if !filter.systems.is_empty() {
            if let Some(sys) = &entry.system {
                if !filter.systems.contains(sys) {
                    continue;
                }
            } else {
                continue;
            }
        }

        // 2.2 全量清理 (受 GC Roots 保护模式下仅清理孤立项)
        if filter.purge_all {
            initial_purged.insert(hash.clone());
            reason_map.insert(hash.clone(), "Purge All (Unreachable Roots)".to_string());
            continue;
        }

        if only_system_filter {
            initial_purged.insert(hash.clone());
            reason_map.insert(hash.clone(), "System Architecture Reset Match".to_string());
            continue;
        }

        let mut matched = false;
        let mut reasons = Vec::new();

        // 2.3 精确 Hash 匹配
        if filter.store_hashes.contains(hash) {
            matched = true;
            reasons.push("Explicit Hash Match");
        }

        // 2.4 名称 / 路径模式匹配 (Glob / Substring)
        for pat in &filter.patterns {
            if matches_pattern(pat, &entry.name)
                || matches_pattern(pat, &entry.narinfo_meta.store_path)
                || matches_pattern(pat, &entry.narinfo_meta.nar_basename)
            {
                matched = true;
                reasons.push("Pattern Match");
                break;
            }
        }

        // 2.5 时间范围匹配
        if let Some(ref tf) = filter.time_filter
            && let Ok(added_dt) = DateTime::parse_from_rfc3339(&entry.added)
        {
            let added_utc = added_dt.with_timezone(&Utc);
            let time_matched = match tf {
                TimeFilter::Before(dt) => added_utc < *dt,
                TimeFilter::After(dt) => added_utc > *dt,
                TimeFilter::Between(start, end) => added_utc >= *start && added_utc <= *end,
            };
            if time_matched {
                matched = true;
                reasons.push("Time Range Match");
            }
        }

        // 2.6 大小阈值匹配
        if let Some(ref sf) = filter.size_filter {
            let size_matched = match sf {
                SizeFilter::MinBytes(min) => entry.nar_size >= *min,
                SizeFilter::MaxBytes(max) => entry.nar_size <= *max,
                SizeFilter::Range(min, max) => entry.nar_size >= *min && entry.nar_size <= *max,
            };
            if size_matched {
                matched = true;
                reasons.push("Size Threshold Match");
            }
        }

        // 2.7 CI Job / Run ID 匹配
        if let Some(ref job) = entry.origin_job {
            if !filter.origin_jobs.is_empty() && filter.origin_jobs.contains(job) {
                matched = true;
                reasons.push("Origin Job Match");
            }
            for run_id in &filter.origin_runs {
                let run_prefix = format!("run:{}", run_id);
                let run_str = format!("{}", run_id);
                if job.contains(&run_prefix) || job.contains(&run_str) {
                    matched = true;
                    reasons.push("Origin Run Match");
                    break;
                }
            }
        }

        if matched {
            initial_purged.insert(hash.clone());
            reason_map.insert(hash.clone(), reasons.join(", "));
        }
    }

    // 3. 第二轮：依据 CascadeMode 展开级联闭包
    let mut total_purged: HashSet<StoreHash> = initial_purged.clone();

    match filter.cascade_mode {
        CascadeMode::Exact => {
            // 仅清理自身
        }
        CascadeMode::Dependents => {
            // 广度优先遍历反向依赖图（所有依赖被清理项的上层产物一并级联清理）
            let mut queue: VecDeque<StoreHash> = initial_purged.iter().cloned().collect();
            while let Some(curr) = queue.pop_front() {
                if let Some(dependents) = reverse_graph.get(&curr) {
                    for dep in dependents {
                        if filter.protect_gc_roots && protected_reachable.contains(dep) {
                            continue;
                        }
                        if total_purged.insert(dep.clone()) {
                            reason_map
                                .insert(dep.clone(), format!("Cascade dependent of {}", curr));
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }
        }
        CascadeMode::Transitive => {
            // 广度优先遍历前向依赖闭包
            let mut queue: VecDeque<StoreHash> = initial_purged.iter().cloned().collect();
            while let Some(curr) = queue.pop_front() {
                if let Some(deps) = forward_graph.get(&curr) {
                    for dep in deps {
                        if filter.protect_gc_roots && protected_reachable.contains(dep) {
                            continue;
                        }
                        if total_purged.insert(dep.clone()) {
                            reason_map.insert(
                                dep.clone(),
                                format!("Cascade transitive dependency of {}", curr),
                            );
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }
        }
        CascadeMode::FullTree => {
            // 双向全闭包：反向依赖 + 前向闭包
            let mut queue: VecDeque<StoreHash> = initial_purged.iter().cloned().collect();
            while let Some(curr) = queue.pop_front() {
                if let Some(dependents) = reverse_graph.get(&curr) {
                    for dep in dependents {
                        if filter.protect_gc_roots && protected_reachable.contains(dep) {
                            continue;
                        }
                        if total_purged.insert(dep.clone()) {
                            reason_map
                                .insert(dep.clone(), format!("Cascade dependent of {}", curr));
                            queue.push_back(dep.clone());
                        }
                    }
                }
                if let Some(deps) = forward_graph.get(&curr) {
                    for dep in deps {
                        if filter.protect_gc_roots && protected_reachable.contains(dep) {
                            continue;
                        }
                        if total_purged.insert(dep.clone()) {
                            reason_map.insert(
                                dep.clone(),
                                format!("Cascade transitive dependency of {}", curr),
                            );
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }
        }
    }

    // 4. 分类收集
    let mut kept_entries = HashMap::new();
    let mut purged_entries = HashMap::new();
    let mut purged_hashes = Vec::new();
    let mut purged_nar_digests = Vec::new();
    let mut freed_bytes = 0u64;

    for (hash, entry) in &index.entries {
        if total_purged.contains(hash) {
            purged_entries.insert(hash.clone(), entry.clone());
            purged_hashes.push(hash.clone());
            purged_nar_digests.push(entry.nar_digest.clone());
            freed_bytes += entry.nar_size;
        } else {
            kept_entries.insert(hash.clone(), entry.clone());
        }
    }

    purged_hashes.sort();

    // 5. 同步净化与重构 GC Roots
    let mut updated_gc_roots = HashMap::new();
    for (sys, roots) in &index.gc_roots {
        let mut valid_roots = Vec::new();
        for root in roots {
            if total_purged.contains(root) {
                continue;
            }

            // 验证该 root 的所有传递依赖未发生断链
            let mut is_sound = true;
            let mut visited = HashSet::new();
            let mut q = VecDeque::new();
            q.push_back(root.clone());
            visited.insert(root.clone());

            while let Some(curr) = q.pop_front() {
                if let Some(deps) = forward_graph.get(&curr) {
                    for dep in deps {
                        if total_purged.contains(dep) {
                            is_sound = false;
                            break;
                        }
                        if visited.insert(dep.clone()) {
                            q.push_back(dep.clone());
                        }
                    }
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

    PurgeEvaluationResult {
        kept_entries,
        purged_entries,
        purged_hashes,
        purged_nar_digests,
        updated_gc_roots,
        estimated_freed_bytes: freed_bytes,
        reason_map,
    }
}
