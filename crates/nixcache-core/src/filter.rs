use crate::types::{IndexEntry, StoreHash, SystemArch};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet, VecDeque};

/// 依赖图遍历与级联模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CascadeMode {
    #[default]
    Exact,
    Dependents,
    Transitive,
    FullTree,
}

/// 时间范围过滤
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeFilter {
    Before(DateTime<Utc>),
    After(DateTime<Utc>),
    Between(DateTime<Utc>, DateTime<Utc>),
}

/// 体积范围过滤
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SizeFilter {
    MinBytes(u64),
    MaxBytes(u64),
    Range(u64, u64),
}

/// 排序字段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    AddedDate,
    NarSize,
    Name,
    StoreHash,
}

/// 排序方向
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Desc,
    Asc,
}

/// 统一的不可变声明式缓存选择器
#[derive(Debug, Clone, Default)]
pub struct CacheSelector {
    /// 是否全选（若为 true 且无 root 保护，则全量匹配）
    pub select_all: bool,
    /// 显式匹配的 StoreHash 集合
    pub store_hashes: HashSet<StoreHash>,
    /// 名称、StorePath 或 NarBasename 匹配模式 (Glob / Substring)
    pub patterns: Vec<String>,
    /// 目标系统架构过滤
    pub systems: HashSet<SystemArch>,
    /// 时间范围条件
    pub time_filter: Option<TimeFilter>,
    /// 体积阈值条件
    pub size_filter: Option<SizeFilter>,
    /// CI Job / Run ID 条件
    pub origin_jobs: HashSet<String>,
    pub origin_runs: HashSet<u64>,
    /// 级联展开模式
    pub cascade_mode: CascadeMode,
    /// 是否保护 GC Roots 可达闭包不被选中
    pub protect_gc_roots: bool,
}

/// 统一的查询评估结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheQueryResult {
    /// 匹配命中的缓存条目集合 (按 Hash 索引)
    pub matched_entries: HashMap<StoreHash, IndexEntry>,
    /// 未命中的保留条目集合
    pub unmatched_entries: HashMap<StoreHash, IndexEntry>,
    /// 初始命中的 StoreHash 列表（未展开级联前）
    pub initial_matched_hashes: Vec<StoreHash>,
    /// 最终匹配的 StoreHash 列表（已展开级联）
    pub final_matched_hashes: Vec<StoreHash>,
    /// 命中匹配的原因映射表 (StoreHash -> 命中理由字符串)
    pub reason_map: HashMap<StoreHash, String>,
    /// 匹配命中的总 NAR 体积 (字节)
    pub matched_bytes: u64,
    /// 未命中的总 NAR 体积 (字节)
    pub unmatched_bytes: u64,
    /// 活跃的 GC Roots 集合
    pub active_gc_roots: HashMap<SystemArch, Vec<StoreHash>>,
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

/// 单架构缓存选择与依赖图闭包展开评估
pub fn evaluate_arch_cache_query(
    entries: &HashMap<StoreHash, IndexEntry>,
    gc_roots: &[StoreHash],
    system: SystemArch,
    selector: &CacheSelector,
) -> CacheQueryResult {
    let mut roots_map = HashMap::new();
    if !gc_roots.is_empty() {
        roots_map.insert(system, gc_roots.to_vec());
    }
    evaluate_cache_query(entries, &roots_map, selector)
}

/// 纯函数：多维构建缓存选择与依赖图闭包展开引擎
pub fn evaluate_cache_query(
    entries: &HashMap<StoreHash, IndexEntry>,
    gc_roots: &HashMap<SystemArch, Vec<StoreHash>>,
    selector: &CacheSelector,
) -> CacheQueryResult {
    // 1. 构建正向依赖图 (A -> References) 与 反向依赖图 (B -> Dependents of B)
    let mut forward_graph: HashMap<StoreHash, Vec<StoreHash>> = HashMap::new();
    let mut reverse_graph: HashMap<StoreHash, Vec<StoreHash>> = HashMap::new();

    for (hash, entry) in entries {
        let deps: Vec<StoreHash> = entry.narinfo_meta.reference_hashes().collect();
        for dep in &deps {
            reverse_graph
                .entry(dep.clone())
                .or_default()
                .push(hash.clone());
        }
        forward_graph.insert(hash.clone(), deps);
    }

    // 2. 收集活跃的 GC Roots 并计算正向可达闭包（若启用 protect_gc_roots）
    let mut active_gc_roots: HashMap<SystemArch, Vec<StoreHash>> = HashMap::new();
    let mut protected_reachable: HashSet<StoreHash> = HashSet::new();

    for (sys, roots) in gc_roots {
        if selector.systems.is_empty() || selector.systems.contains(sys) {
            active_gc_roots.insert(*sys, roots.clone());
        }
    }

    if selector.protect_gc_roots {
        let mut root_queue: VecDeque<StoreHash> = VecDeque::new();
        for roots in active_gc_roots.values() {
            for root in roots {
                root_queue.push_back(root.clone());
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

    // 3. 第 1 轮初筛：匹配初始命中目标 (Initial Matches)
    let mut initial_matched: HashSet<StoreHash> = HashSet::new();
    let mut initial_matched_hashes: Vec<StoreHash> = Vec::new();
    let mut reason_map: HashMap<StoreHash, String> = HashMap::new();

    let only_system_filter = !selector.systems.is_empty()
        && !selector.select_all
        && selector.store_hashes.is_empty()
        && selector.patterns.is_empty()
        && selector.time_filter.is_none()
        && selector.size_filter.is_none()
        && selector.origin_jobs.is_empty()
        && selector.origin_runs.is_empty();

    for (hash, entry) in entries {
        // 3.0 若受 GC Roots 保护且处于可达闭包中，绝对不作为初始命中目标
        if selector.protect_gc_roots && protected_reachable.contains(hash) {
            continue;
        }

        // 3.1 系统架构前置过滤
        if !selector.systems.is_empty() {
            if let Some(sys) = &entry.system {
                if !selector.systems.contains(sys) {
                    continue;
                }
            } else {
                continue;
            }
        }

        // 3.2 全选模式 (在 GC Roots 保护模式下匹配所有非根可达项)
        if selector.select_all {
            initial_matched.insert(hash.clone());
            initial_matched_hashes.push(hash.clone());
            let reason = if selector.protect_gc_roots {
                "Select All (Unreachable Roots)".to_string()
            } else {
                "Select All".to_string()
            };
            reason_map.insert(hash.clone(), reason);
            continue;
        }

        // 3.3 仅架构重置/匹配模式
        if only_system_filter {
            initial_matched.insert(hash.clone());
            initial_matched_hashes.push(hash.clone());
            reason_map.insert(hash.clone(), "System Architecture Match".to_string());
            continue;
        }

        let mut matched = false;
        let mut reasons = Vec::new();

        // 3.4 精确 Hash 匹配
        if selector.store_hashes.contains(hash) {
            matched = true;
            reasons.push("Explicit Hash Match");
        }

        // 3.5 名称 / 路径模式匹配 (Glob / Substring)
        for pat in &selector.patterns {
            if matches_pattern(pat, &entry.name)
                || matches_pattern(pat, &entry.narinfo_meta.store_path)
                || matches_pattern(pat, &entry.narinfo_meta.nar_basename)
            {
                matched = true;
                reasons.push("Pattern Match");
                break;
            }
        }

        // 3.6 时间范围匹配
        if let Some(ref tf) = selector.time_filter
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

        // 3.7 体积阈值匹配
        if let Some(ref sf) = selector.size_filter {
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

        // 3.8 CI Job / Run ID 匹配
        if let Some(ref job) = entry.origin_job {
            if !selector.origin_jobs.is_empty() && selector.origin_jobs.contains(job) {
                matched = true;
                reasons.push("Origin Job Match");
            }
            for run_id in &selector.origin_runs {
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
            initial_matched.insert(hash.clone());
            initial_matched_hashes.push(hash.clone());
            reason_map.insert(hash.clone(), reasons.join(", "));
        }
    }

    initial_matched_hashes.sort();

    // 4. 第 2 轮：依据 CascadeMode 展开级联闭包
    let mut total_matched: HashSet<StoreHash> = initial_matched.clone();

    match selector.cascade_mode {
        CascadeMode::Exact => {
            // 仅自身
        }
        CascadeMode::Dependents => {
            // 广度优先遍历反向依赖图（下游消费者）
            let mut queue: VecDeque<StoreHash> = initial_matched.iter().cloned().collect();
            while let Some(curr) = queue.pop_front() {
                if let Some(dependents) = reverse_graph.get(&curr) {
                    for dep in dependents {
                        if selector.protect_gc_roots && protected_reachable.contains(dep) {
                            continue;
                        }
                        if total_matched.insert(dep.clone()) {
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
            let mut queue: VecDeque<StoreHash> = initial_matched.iter().cloned().collect();
            while let Some(curr) = queue.pop_front() {
                if let Some(deps) = forward_graph.get(&curr) {
                    for dep in deps {
                        if selector.protect_gc_roots && protected_reachable.contains(dep) {
                            continue;
                        }
                        if total_matched.insert(dep.clone()) {
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
            let mut queue: VecDeque<StoreHash> = initial_matched.iter().cloned().collect();
            while let Some(curr) = queue.pop_front() {
                if let Some(dependents) = reverse_graph.get(&curr) {
                    for dep in dependents {
                        if selector.protect_gc_roots && protected_reachable.contains(dep) {
                            continue;
                        }
                        if total_matched.insert(dep.clone()) {
                            reason_map
                                .insert(dep.clone(), format!("Cascade dependent of {}", curr));
                            queue.push_back(dep.clone());
                        }
                    }
                }
                if let Some(deps) = forward_graph.get(&curr) {
                    for dep in deps {
                        if selector.protect_gc_roots && protected_reachable.contains(dep) {
                            continue;
                        }
                        if total_matched.insert(dep.clone()) {
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

    // 5. 分类收集与汇总
    let mut matched_entries = HashMap::new();
    let mut unmatched_entries = HashMap::new();
    let mut final_matched_hashes = Vec::new();
    let mut matched_bytes = 0u64;
    let mut unmatched_bytes = 0u64;

    for (hash, entry) in entries {
        if total_matched.contains(hash) {
            matched_entries.insert(hash.clone(), entry.clone());
            final_matched_hashes.push(hash.clone());
            matched_bytes += entry.nar_size;
        } else {
            unmatched_entries.insert(hash.clone(), entry.clone());
            unmatched_bytes += entry.nar_size;
        }
    }

    final_matched_hashes.sort();

    CacheQueryResult {
        matched_entries,
        unmatched_entries,
        initial_matched_hashes,
        final_matched_hashes,
        reason_map,
        matched_bytes,
        unmatched_bytes,
        active_gc_roots,
    }
}
