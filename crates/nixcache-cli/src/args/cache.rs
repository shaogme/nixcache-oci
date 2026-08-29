use clap::Args;
use nixcache_utils::Env;
use std::path::PathBuf;

pub const DEFAULT_UPSTREAM_CACHE: &str = "https://cache.nixos.org";
pub const DEFAULT_SESSION_TTL: u64 = 10;
pub const DEFAULT_BASELINE_TTL: u64 = 300;
pub const DEFAULT_BASELINE_TAG: &str = "cache-index";
pub const DEFAULT_SNAPSHOT_PATH: &str = "/tmp/nixcache-snapshot-before.txt";

/// 缓存与代理策略参数组
#[derive(Args, Debug, Clone, Default)]
pub struct CachePolicyArgs {
    #[arg(long, help = "Upstream cache URLs [env: NIXCACHE_UPSTREAM]")]
    pub upstream: Option<String>,

    #[arg(
        long,
        help = "Session index TTL in seconds [env: NIXCACHE_SESSION_TTL]"
    )]
    pub session_ttl: Option<u64>,

    #[arg(
        long,
        help = "Baseline index TTL in seconds [env: NIXCACHE_BASELINE_TTL, NIXCACHE_INDEX_TTL]"
    )]
    pub baseline_ttl: Option<u64>,

    #[arg(long, help = "Baseline production tag [env: NIXCACHE_BASELINE_TAG]")]
    pub baseline_tag: Option<String>,

    #[arg(
        long,
        help = "Path to record baseline store snapshot [env: NIXCACHE_SNAPSHOT_PATH]"
    )]
    pub snapshot_path: Option<PathBuf>,

    #[arg(
        long,
        help = "Directory to store cache index [env: NIXCACHE_INDEX_DIR]"
    )]
    pub index_dir: Option<PathBuf>,
}

impl CachePolicyArgs {
    /// 解析上游缓存地址
    pub fn resolve_upstream(&self) -> String {
        self.upstream
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_UPSTREAM"))
            .unwrap_or_else(|| DEFAULT_UPSTREAM_CACHE.to_string())
    }

    /// 解析上游缓存 URL 列表（以空格切分）
    pub fn resolve_upstream_list(&self) -> Vec<String> {
        self.resolve_upstream()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect()
    }

    /// 解析 Session Index TTL（秒）
    pub fn resolve_session_ttl(&self) -> u64 {
        self.session_ttl
            .or_else(|| Env::parse("NIXCACHE_SESSION_TTL"))
            .unwrap_or(DEFAULT_SESSION_TTL)
    }

    /// 解析 Baseline Index TTL（秒，支持 NIXCACHE_BASELINE_TTL 与 NIXCACHE_INDEX_TTL）
    pub fn resolve_baseline_ttl(&self) -> u64 {
        self.baseline_ttl
            .or_else(|| Env::parse_first(&["NIXCACHE_BASELINE_TTL", "NIXCACHE_INDEX_TTL"]))
            .unwrap_or(DEFAULT_BASELINE_TTL)
    }

    /// 解析 Baseline 标签
    pub fn resolve_baseline_tag(&self) -> String {
        self.baseline_tag
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_BASELINE_TAG"))
            .unwrap_or_else(|| DEFAULT_BASELINE_TAG.to_string())
    }

    /// 解析快照路径（默认 /tmp/nixcache-snapshot-before.txt）
    pub fn resolve_snapshot_path(&self) -> PathBuf {
        self.snapshot_path
            .as_deref()
            .and_then(Env::non_empty_path)
            .map(PathBuf::from)
            .or_else(|| Env::get_path("NIXCACHE_SNAPSHOT_PATH"))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SNAPSHOT_PATH))
    }

    /// 解析本地索引缓存目录（支持 NIXCACHE_INDEX_DIR -> CACHE_DIRECTORY -> ~/.cache/nixcache-proxy/<repo>）
    pub fn resolve_index_dir(&self, repo: &str) -> PathBuf {
        if let Some(dir) = self.index_dir.as_deref().and_then(Env::non_empty_path) {
            return dir.to_path_buf();
        }
        if let Some(dir) = Env::get_path_first(&["NIXCACHE_INDEX_DIR", "CACHE_DIRECTORY"]) {
            return dir;
        }

        let home = Env::get_first(&["HOME"]).unwrap_or_else(|| ".".to_string());
        PathBuf::from(home)
            .join(".cache")
            .join("nixcache-proxy")
            .join(repo.replace('/', "--"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CachePolicyArgs, DEFAULT_BASELINE_TAG, DEFAULT_BASELINE_TTL, DEFAULT_SESSION_TTL,
        DEFAULT_SNAPSHOT_PATH, DEFAULT_UPSTREAM_CACHE,
    };
    use std::{env, path::PathBuf};

    #[test]
    fn test_cache_policy_defaults_and_env() {
        let empty = CachePolicyArgs::default();
        assert_eq!(empty.resolve_upstream(), DEFAULT_UPSTREAM_CACHE);
        assert_eq!(empty.resolve_session_ttl(), DEFAULT_SESSION_TTL);
        assert_eq!(empty.resolve_baseline_ttl(), DEFAULT_BASELINE_TTL);
        assert_eq!(empty.resolve_baseline_tag(), DEFAULT_BASELINE_TAG);
        assert_eq!(
            empty.resolve_snapshot_path(),
            PathBuf::from(DEFAULT_SNAPSHOT_PATH)
        );

        unsafe {
            env::set_var("NIXCACHE_SESSION_TTL", "60");
            env::set_var("NIXCACHE_INDEX_TTL", "600");
            env::set_var("NIXCACHE_BASELINE_TAG", "prod-v1");
        }

        assert_eq!(empty.resolve_session_ttl(), 60);
        assert_eq!(empty.resolve_baseline_ttl(), 600);
        assert_eq!(empty.resolve_baseline_tag(), "prod-v1");

        unsafe {
            env::remove_var("NIXCACHE_SESSION_TTL");
            env::remove_var("NIXCACHE_INDEX_TTL");
            env::remove_var("NIXCACHE_BASELINE_TAG");
        }
    }
}
