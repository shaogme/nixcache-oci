use chrono::{DateTime, Utc};
use clap::Args;
use nixcache_core::{CachePurgeFilter, CascadeMode, SizeFilter, StoreHash, SystemArch, TimeFilter};
use nixcache_utils::Env;
use std::collections::HashSet;

/// 构建缓存主动清理与失效参数组
#[derive(Args, Debug, Clone, Default)]
pub struct PurgeFilterArgs {
    #[arg(
        long,
        help = "Purge all baseline cache entries and reset global index [env: NIXCACHE_PURGE_ALL]"
    )]
    pub all: bool,

    #[arg(
        long,
        help = "Specific store hashes to purge (comma/space separated) [env: NIXCACHE_PURGE_HASHES]"
    )]
    pub hashes: Option<String>,

    #[arg(
        long,
        help = "Name/Path glob or regex patterns to match for purging [env: NIXCACHE_PURGE_PATTERNS]"
    )]
    pub patterns: Vec<String>,

    #[arg(
        long,
        help = "Filter by target system architecture(s) [env: NIXCACHE_SYSTEM]"
    )]
    pub system: Vec<String>,

    #[arg(
        long,
        help = "Path to flake directory to evaluate outputs for purging [env: NIXCACHE_FLAKE_PATH]"
    )]
    pub flake_path: Option<String>,

    #[arg(
        long,
        help = "Flake attributes to evaluate and purge (comma/space separated) [env: NIXCACHE_ATTRIBUTES]"
    )]
    pub attributes: Option<String>,

    #[arg(
        long,
        help = "Purge entries added before specified duration or timestamp (e.g., 30d, 12h, 2026-01-01T00:00:00Z) [env: NIXCACHE_OLDER_THAN]"
    )]
    pub older_than: Option<String>,

    #[arg(
        long,
        help = "Cascade purge mode: exact, dependents (default), transitive, full [env: NIXCACHE_CASCADE]"
    )]
    pub cascade: Option<String>,

    #[arg(
        long,
        help = "Minimum NAR size threshold (e.g., 500M, 1G, 1048576) [env: NIXCACHE_MIN_SIZE]"
    )]
    pub min_size: Option<String>,

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
        help = "Attempt physical OCI blob deletion (best-effort) [env: NIXCACHE_DELETE_BLOBS]"
    )]
    pub delete_blobs: bool,

    #[arg(
        long,
        help = "Protect reachable closures of active GC roots from being purged [env: NIXCACHE_PROTECT_GC_ROOTS]"
    )]
    pub protect_gc_roots: bool,

    #[arg(
        long,
        help = "Dry run mode (preview deletions without applying) [env: NIXCACHE_DRY_RUN]"
    )]
    pub dry_run: bool,
}

fn parse_size_str(s: &str) -> Option<u64> {
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
        Some('K') => Some(base * 1024),
        Some('M') => Some(base * 1024 * 1024),
        Some('G') => Some(base * 1024 * 1024 * 1024),
        Some('T') => Some(base * 1024 * 1024 * 1024 * 1024),
        None => Some(base),
        _ => None,
    }
}

fn parse_older_than(s: &str) -> Option<DateTime<Utc>> {
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
            "d" => Some(chrono::Duration::days(val)),
            "h" => Some(chrono::Duration::hours(val)),
            "m" => Some(chrono::Duration::minutes(val)),
            "s" => Some(chrono::Duration::seconds(val)),
            _ => None,
        };
        if let Some(d) = duration {
            return Some(Utc::now() - d);
        }
    }

    None
}

impl PurgeFilterArgs {
    pub fn resolve_all(&self) -> bool {
        if self.all {
            return true;
        }
        Env::get_bool("NIXCACHE_PURGE_ALL").unwrap_or(false)
    }

    pub fn resolve_hashes(&self) -> Vec<StoreHash> {
        let hash_str = self
            .hashes
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_PURGE_HASHES"))
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
            && let Some(env_pats) = Env::get("NIXCACHE_PURGE_PATTERNS")
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
            .and_then(|s| parse_older_than(&s))
    }

    pub fn resolve_cascade(&self) -> CascadeMode {
        let mode_str = self
            .cascade
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_ascii_lowercase())
            .or_else(|| Env::get("NIXCACHE_CASCADE").map(|s| s.to_ascii_lowercase()))
            .unwrap_or_else(|| "dependents".to_string());

        match mode_str.as_str() {
            "exact" => CascadeMode::Exact,
            "dependents" | "dependent" | "downstream" => CascadeMode::Dependents,
            "transitive" | "forward" => CascadeMode::Transitive,
            "full" | "full-tree" | "all" => CascadeMode::FullTree,
            _ => CascadeMode::Dependents,
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

    pub fn resolve_delete_blobs(&self) -> bool {
        if self.delete_blobs {
            return true;
        }
        Env::get_bool("NIXCACHE_DELETE_BLOBS").unwrap_or(false)
    }

    pub fn resolve_protect_gc_roots(&self) -> bool {
        if self.protect_gc_roots {
            return true;
        }
        Env::get_bool("NIXCACHE_PROTECT_GC_ROOTS").unwrap_or(false)
    }

    pub fn resolve_dry_run(&self) -> bool {
        if self.dry_run {
            return true;
        }
        Env::get_bool("NIXCACHE_DRY_RUN").unwrap_or(false)
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

    /// 转换为 nixcache-core 的 CachePurgeFilter 结构体
    pub fn to_purge_filter(&self, extra_hashes: &[StoreHash]) -> CachePurgeFilter {
        let mut store_hashes: HashSet<StoreHash> = self.resolve_hashes().into_iter().collect();
        store_hashes.extend(extra_hashes.iter().cloned());

        let systems: HashSet<SystemArch> = self.resolve_systems().into_iter().collect();
        let time_filter = self.resolve_older_than().map(TimeFilter::Before);
        let size_filter = self.resolve_min_size().map(SizeFilter::MinBytes);

        CachePurgeFilter {
            purge_all: self.resolve_all(),
            store_hashes,
            patterns: self.resolve_patterns(),
            systems,
            time_filter,
            size_filter,
            origin_jobs: self.resolve_origin_jobs(),
            origin_runs: self.resolve_origin_runs(),
            cascade_mode: self.resolve_cascade(),
            protect_gc_roots: self.resolve_protect_gc_roots(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_purge_args_resolution_and_parsing() {
        let args = PurgeFilterArgs {
            all: false,
            hashes: Some(
                "s66mzxpvicwk07gjbjfw9izjfa797vsw,00000000000000000000000000000001".to_string(),
            ),
            patterns: vec!["*chromium*".to_string()],
            system: vec!["x86_64-linux".to_string()],
            older_than: Some("30d".to_string()),
            cascade: Some("transitive".to_string()),
            min_size: Some("500M".to_string()),
            origin_job: vec!["job1".to_string()],
            origin_run: vec![12345],
            delete_blobs: true,
            protect_gc_roots: true,
            dry_run: true,
            ..Default::default()
        };

        assert!(!args.resolve_all());
        assert_eq!(args.resolve_hashes().len(), 2);
        assert_eq!(args.resolve_patterns(), vec!["*chromium*"]);
        assert_eq!(args.resolve_systems(), vec![SystemArch::X86_64Linux]);
        assert!(args.resolve_older_than().is_some());
        assert_eq!(args.resolve_cascade(), CascadeMode::Transitive);
        assert_eq!(args.resolve_min_size(), Some(500 * 1024 * 1024));
        assert!(args.resolve_origin_jobs().contains("job1"));
        assert!(args.resolve_origin_runs().contains(&12345));
        assert!(args.resolve_delete_blobs());
        assert!(args.resolve_protect_gc_roots());
        assert!(args.resolve_dry_run());

        let filter = args.to_purge_filter(&[]);
        assert_eq!(filter.store_hashes.len(), 2);
        assert_eq!(filter.cascade_mode, CascadeMode::Transitive);
        assert!(filter.protect_gc_roots);
        assert_eq!(
            filter.size_filter,
            Some(SizeFilter::MinBytes(500 * 1024 * 1024))
        );
    }
}
