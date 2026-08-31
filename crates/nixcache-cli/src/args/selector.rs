use chrono::{DateTime, Duration, Utc};
use clap::Args;
use nixcache_core::{
    CacheSelector, CascadeMode, FilterPredicates, SelectionScope, SizeFilter, StoreHash,
    SystemArch, TimeFilter,
};
use nixcache_utils::Env;
use std::collections::HashSet;

/// 未指定具体过滤条件时的默认回退策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultScopePolicy {
    /// 默认全选（用于 list 只读查询，若用户未输入条件则展示全量内容）
    SelectAll,
    /// 需显式指定（用于 purge/delete 破坏性操作，未指定条件且未传 --all 时拒绝匹配）
    RequireExplicit,
}

/// 统一构建缓存过滤与选择器参数组
#[derive(Args, Debug, Clone, Default)]
pub struct CacheSelectorArgs {
    #[arg(
        long,
        help = "Match all cache entries [env: NIXCACHE_FILTER_ALL, NIXCACHE_PURGE_ALL]"
    )]
    pub all: bool,

    #[arg(
        long,
        help = "Specific store hashes (comma/space separated) [env: NIXCACHE_FILTER_HASHES, NIXCACHE_PURGE_HASHES]"
    )]
    pub hashes: Option<String>,

    #[arg(
        long,
        help = "Name/Path glob or substring patterns [env: NIXCACHE_FILTER_PATTERNS, NIXCACHE_PURGE_PATTERNS]"
    )]
    pub patterns: Vec<String>,

    #[arg(
        long,
        help = "Filter by target system architecture(s) [env: NIXCACHE_SYSTEM]"
    )]
    pub system: Vec<String>,

    #[arg(
        long,
        help = "Path to flake directory to evaluate outputs [env: NIXCACHE_FLAKE_PATH]"
    )]
    pub flake_path: Option<String>,

    #[arg(
        long,
        help = "Flake attributes to evaluate and match (comma/space separated) [env: NIXCACHE_ATTRIBUTES]"
    )]
    pub attributes: Option<String>,

    #[arg(
        long,
        help = "Entries added before duration/timestamp (e.g., 30d, 12h, 2026-01-01T00:00:00Z) [env: NIXCACHE_OLDER_THAN]"
    )]
    pub older_than: Option<String>,

    #[arg(
        long,
        help = "Entries added after duration/timestamp (e.g., 7d, 2h, 2026-01-01T00:00:00Z) [env: NIXCACHE_NEWER_THAN]"
    )]
    pub newer_than: Option<String>,

    #[arg(
        long,
        help = "Minimum NAR size threshold (e.g., 500M, 1G, 1048576) [env: NIXCACHE_MIN_SIZE]"
    )]
    pub min_size: Option<String>,

    #[arg(
        long,
        help = "Maximum NAR size threshold (e.g., 100M, 2G, 5242880) [env: NIXCACHE_MAX_SIZE]"
    )]
    pub max_size: Option<String>,

    #[arg(
        long,
        help = "Filter by origin CI Job ID(s) [env: NIXCACHE_ORIGIN_JOB]"
    )]
    pub origin_job: Vec<String>,

    #[arg(
        long,
        help = "Filter by origin CI Run ID(s) [env: NIXCACHE_ORIGIN_RUN]"
    )]
    pub origin_run: Vec<u64>,

    #[arg(
        long,
        help = "Cascade mode: exact, dependents, transitive, full [env: NIXCACHE_CASCADE]"
    )]
    pub cascade: Option<String>,

    #[arg(
        long,
        help = "Protect reachable closures of active GC roots from matching [env: NIXCACHE_PROTECT_GC_ROOTS]"
    )]
    pub protect_gc_roots: bool,
}

pub fn parse_size_str(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (num_part, unit) = if let Some(last) = trimmed.chars().last() {
        if last.is_alphabetic() {
            (
                &trimmed[..trimmed.len() - 1],
                Some(last.to_ascii_uppercase()),
            )
        } else {
            (trimmed, None)
        }
    } else {
        (trimmed, None)
    };

    let base: u64 = num_part.trim().parse().ok()?;
    match unit {
        Some('B') => Some(base),
        Some('K') => Some(base * 1024),
        Some('M') => Some(base * 1024 * 1024),
        Some('G') => Some(base * 1024 * 1024 * 1024),
        Some('T') => Some(base * 1024 * 1024 * 1024 * 1024),
        None => Some(base),
        _ => None,
    }
}

pub fn parse_duration_or_datetime(s: &str) -> Option<DateTime<Utc>> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.with_timezone(&Utc));
    }

    let (num_str, unit) = trimmed.split_at(trimmed.len() - 1);
    if let Ok(val) = num_str.trim().parse::<i64>() {
        let duration = match unit.to_ascii_lowercase().as_str() {
            "d" => Some(Duration::days(val)),
            "h" => Some(Duration::hours(val)),
            "m" => Some(Duration::minutes(val)),
            "s" => Some(Duration::seconds(val)),
            _ => None,
        };
        if let Some(d) = duration {
            return Some(Utc::now() - d);
        }
    }

    None
}

impl CacheSelectorArgs {
    pub fn resolve_all(&self) -> bool {
        if self.all {
            return true;
        }
        Env::get_bool("NIXCACHE_FILTER_ALL")
            .or_else(|| Env::get_bool("NIXCACHE_PURGE_ALL"))
            .unwrap_or(false)
    }

    pub fn resolve_hashes(&self) -> Vec<StoreHash> {
        let hash_str = self
            .hashes
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get_first(&["NIXCACHE_FILTER_HASHES", "NIXCACHE_PURGE_HASHES"]))
            .unwrap_or_default();

        hash_str
            .split([',', ' '])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(StoreHash::new_unchecked)
            .collect()
    }

    pub fn resolve_patterns(&self) -> Vec<String> {
        let mut patterns = self.patterns.clone();
        if patterns.is_empty()
            && let Some(env_pats) =
                Env::get_first(&["NIXCACHE_FILTER_PATTERNS", "NIXCACHE_PURGE_PATTERNS"])
        {
            patterns.extend(
                env_pats
                    .split([',', ' '])
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
        patterns
    }

    pub fn resolve_systems(&self) -> Vec<SystemArch> {
        let mut systems = Vec::new();
        for sys_str in &self.system {
            systems.push(SystemArch::from(sys_str.as_str()));
        }
        if systems.is_empty()
            && let Some(env_sys) = Env::get("NIXCACHE_SYSTEM")
        {
            for s in env_sys.split([',', ' ']) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    systems.push(SystemArch::from(trimmed));
                }
            }
        }
        systems
    }

    pub fn resolve_flake_path(&self) -> Option<String> {
        self.flake_path
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_FLAKE_PATH"))
    }

    pub fn resolve_attributes(&self) -> Vec<String> {
        let attr_str = self
            .attributes
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_ATTRIBUTES"))
            .unwrap_or_default();

        attr_str
            .split([',', ' '])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    pub fn resolve_older_than(&self) -> Option<DateTime<Utc>> {
        self.older_than
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_OLDER_THAN"))
            .and_then(|s| parse_duration_or_datetime(&s))
    }

    pub fn resolve_newer_than(&self) -> Option<DateTime<Utc>> {
        self.newer_than
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_NEWER_THAN"))
            .and_then(|s| parse_duration_or_datetime(&s))
    }

    pub fn resolve_time_filter(&self) -> Option<TimeFilter> {
        let older = self.resolve_older_than();
        let newer = self.resolve_newer_than();

        match (older, newer) {
            (Some(o), Some(n)) => {
                if n <= o {
                    Some(TimeFilter::Between(n, o))
                } else {
                    Some(TimeFilter::Between(o, n))
                }
            }
            (Some(o), None) => Some(TimeFilter::Before(o)),
            (None, Some(n)) => Some(TimeFilter::After(n)),
            (None, None) => None,
        }
    }

    pub fn resolve_min_size(&self) -> Option<u64> {
        self.min_size
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_MIN_SIZE"))
            .and_then(|s| parse_size_str(&s))
    }

    pub fn resolve_max_size(&self) -> Option<u64> {
        self.max_size
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_MAX_SIZE"))
            .and_then(|s| parse_size_str(&s))
    }

    pub fn resolve_size_filter(&self) -> Option<SizeFilter> {
        let min = self.resolve_min_size();
        let max = self.resolve_max_size();

        match (min, max) {
            (Some(mi), Some(ma)) => Some(SizeFilter::Range(mi, ma)),
            (Some(mi), None) => Some(SizeFilter::MinBytes(mi)),
            (None, Some(ma)) => Some(SizeFilter::MaxBytes(ma)),
            (None, None) => None,
        }
    }

    pub fn resolve_cascade(&self, default_mode: CascadeMode) -> CascadeMode {
        let mode_str = self
            .cascade
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_ascii_lowercase())
            .or_else(|| Env::get("NIXCACHE_CASCADE").map(|s| s.to_ascii_lowercase()));

        if let Some(m) = mode_str {
            match m.as_str() {
                "exact" | "none" => CascadeMode::Exact,
                "dependents" | "dependent" | "downstream" => CascadeMode::Dependents,
                "transitive" | "forward" | "dependencies" => CascadeMode::Transitive,
                "full" | "full-tree" | "all" => CascadeMode::FullTree,
                _ => default_mode,
            }
        } else {
            default_mode
        }
    }

    pub fn resolve_protect_gc_roots(&self) -> bool {
        if self.protect_gc_roots {
            return true;
        }
        Env::get_bool("NIXCACHE_PROTECT_GC_ROOTS").unwrap_or(false)
    }

    pub fn resolve_origin_jobs(&self) -> HashSet<String> {
        let mut jobs = HashSet::new();
        for j in &self.origin_job {
            jobs.insert(j.clone());
        }
        if jobs.is_empty()
            && let Some(env_job) = Env::get("NIXCACHE_ORIGIN_JOB")
        {
            for s in env_job.split([',', ' ']) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    jobs.insert(trimmed.to_string());
                }
            }
        }
        jobs
    }

    pub fn resolve_origin_runs(&self) -> HashSet<u64> {
        let mut runs = HashSet::new();
        for r in &self.origin_run {
            runs.insert(*r);
        }
        if runs.is_empty()
            && let Some(env_runs) = Env::get("NIXCACHE_ORIGIN_RUN")
        {
            for s in env_runs.split([',', ' ']) {
                if let Ok(id) = s.trim().parse::<u64>() {
                    runs.insert(id);
                }
            }
        }
        runs
    }

    /// 提取解析后的纯过滤谓词集合
    pub fn resolve_predicates(&self, extra_hashes: &[StoreHash]) -> FilterPredicates {
        let mut store_hashes: HashSet<StoreHash> = self.resolve_hashes().into_iter().collect();
        store_hashes.extend(extra_hashes.iter().cloned());

        FilterPredicates {
            store_hashes,
            patterns: self.resolve_patterns(),
            systems: self.resolve_systems().into_iter().collect(),
            time_filter: self.resolve_time_filter(),
            size_filter: self.resolve_size_filter(),
            origin_jobs: self.resolve_origin_jobs(),
            origin_runs: self.resolve_origin_runs(),
        }
    }

    /// 转换为专用于 list 只读查询的选择器（默认策略：SelectAll）
    pub fn to_list_selector(&self, extra_hashes: &[StoreHash]) -> CacheSelector {
        self.to_selector_with_policy(
            extra_hashes,
            CascadeMode::Exact,
            DefaultScopePolicy::SelectAll,
        )
    }

    /// 转换为专用于 purge 清理操作的选择器（默认策略：RequireExplicit，默认级联：Dependents）
    pub fn to_purge_selector(&self, extra_hashes: &[StoreHash]) -> CacheSelector {
        self.to_selector_with_policy(
            extra_hashes,
            CascadeMode::Dependents,
            DefaultScopePolicy::RequireExplicit,
        )
    }

    /// 基于明确策略将 CLI 参数转换为领域模型 CacheSelector
    pub fn to_selector_with_policy(
        &self,
        extra_hashes: &[StoreHash],
        default_cascade: CascadeMode,
        policy: DefaultScopePolicy,
    ) -> CacheSelector {
        let is_all = self.resolve_all();
        let predicates = self.resolve_predicates(extra_hashes);
        let systems = predicates.systems.clone();

        let scope = if is_all {
            SelectionScope::All { systems }
        } else if predicates.is_empty() {
            match policy {
                DefaultScopePolicy::SelectAll => SelectionScope::All { systems },
                DefaultScopePolicy::RequireExplicit => SelectionScope::None,
            }
        } else {
            SelectionScope::Filtered(Box::new(predicates))
        };

        CacheSelector {
            scope,
            cascade_mode: self.resolve_cascade(default_cascade),
            protect_gc_roots: self.resolve_protect_gc_roots(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_selector_args_resolution() {
        let args = CacheSelectorArgs {
            all: false,
            hashes: Some(
                "s66mzxpvicwk07gjbjfw9izjfa797vsw,00000000000000000000000000000001".to_string(),
            ),
            patterns: vec!["*chromium*".to_string()],
            system: vec!["x86_64-linux".to_string()],
            older_than: Some("30d".to_string()),
            newer_than: Some("7d".to_string()),
            cascade: Some("transitive".to_string()),
            min_size: Some("500M".to_string()),
            max_size: Some("2G".to_string()),
            origin_job: vec!["job1".to_string()],
            origin_run: vec![12345],
            protect_gc_roots: true,
            ..Default::default()
        };

        assert!(!args.resolve_all());
        assert_eq!(args.resolve_hashes().len(), 2);
        assert_eq!(args.resolve_patterns(), vec!["*chromium*"]);
        assert_eq!(args.resolve_systems(), vec![SystemArch::X86_64Linux]);
        assert!(args.resolve_time_filter().is_some());
        assert_eq!(
            args.resolve_cascade(CascadeMode::Exact),
            CascadeMode::Transitive
        );
        assert_eq!(
            args.resolve_size_filter(),
            Some(SizeFilter::Range(500 * 1024 * 1024, 2 * 1024 * 1024 * 1024))
        );
        assert!(args.resolve_origin_jobs().contains("job1"));
        assert!(args.resolve_origin_runs().contains(&12345));
        assert!(args.resolve_protect_gc_roots());

        let selector = args.to_list_selector(&[]);
        assert_eq!(selector.cascade_mode, CascadeMode::Transitive);
        assert!(selector.protect_gc_roots);
        if let SelectionScope::Filtered(predicates) = &selector.scope {
            assert_eq!(predicates.store_hashes.len(), 2);
            assert_eq!(
                predicates.size_filter,
                Some(SizeFilter::Range(500 * 1024 * 1024, 2 * 1024 * 1024 * 1024))
            );
        } else {
            panic!("Expected SelectionScope::Filtered");
        }
    }

    #[test]
    fn test_selector_args_policies() {
        let empty_args = CacheSelectorArgs::default();

        // list selector defaults to All
        let list_sel = empty_args.to_list_selector(&[]);
        assert_eq!(list_sel.cascade_mode, CascadeMode::Exact);
        assert!(matches!(list_sel.scope, SelectionScope::All { .. }));

        // purge selector defaults to None (safe reject)
        let purge_sel = empty_args.to_purge_selector(&[]);
        assert_eq!(purge_sel.cascade_mode, CascadeMode::Dependents);
        assert_eq!(purge_sel.scope, SelectionScope::None);

        // explicit all
        let all_args = CacheSelectorArgs {
            all: true,
            ..Default::default()
        };
        let purge_all_sel = all_args.to_purge_selector(&[]);
        assert!(matches!(purge_all_sel.scope, SelectionScope::All { .. }));
    }
}
