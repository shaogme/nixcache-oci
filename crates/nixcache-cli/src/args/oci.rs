use clap::Args;
use nixcache_utils::Env;

pub const DEFAULT_NIXCACHE_REPO: &str = "shaogme/nixcache-oci";
pub const DEFAULT_NIXCACHE_REGISTRY: &str = "ghcr.io";

/// OCI 目标存储库与注册表参数组
#[derive(Args, Debug, Clone, Default)]
pub struct OciTargetArgs {
    #[arg(long, help = "OCI repository (owner/repo) [env: NIXCACHE_REPO]")]
    pub repo: Option<String>,

    #[arg(long, help = "OCI registry [env: NIXCACHE_REGISTRY]")]
    pub registry: Option<String>,
}

impl OciTargetArgs {
    /// 获取解析后的 OCI repo（支持默认值 fallback）
    pub fn resolve_repo(&self, default_repo: &str) -> String {
        self.repo
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_REPO"))
            .unwrap_or_else(|| default_repo.to_string())
    }

    /// 获取解析后的 OCI registry（默认 ghcr.io）
    pub fn resolve_registry(&self, default_registry: &str) -> String {
        self.registry
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_REGISTRY"))
            .unwrap_or_else(|| default_registry.to_string())
    }

    /// 同时解析 (repo, registry)
    pub fn resolve(&self, default_repo: &str) -> (String, String) {
        (
            self.resolve_repo(default_repo),
            self.resolve_registry(DEFAULT_NIXCACHE_REGISTRY),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_NIXCACHE_REGISTRY, DEFAULT_NIXCACHE_REPO, OciTargetArgs};
    use std::env;

    #[test]
    fn test_oci_args_resolution() {
        let empty_args = OciTargetArgs::default();
        let (repo, reg) = empty_args.resolve(DEFAULT_NIXCACHE_REPO);
        assert_eq!(repo, DEFAULT_NIXCACHE_REPO);
        assert_eq!(reg, DEFAULT_NIXCACHE_REGISTRY);

        let explicit_args = OciTargetArgs {
            repo: Some("custom/repo".to_string()),
            registry: Some("docker.io".to_string()),
        };
        let (repo, reg) = explicit_args.resolve(DEFAULT_NIXCACHE_REPO);
        assert_eq!(repo, "custom/repo");
        assert_eq!(reg, "docker.io");

        unsafe {
            env::set_var("NIXCACHE_REPO", "env/repo");
            env::set_var("NIXCACHE_REGISTRY", "quay.io");
        }
        let env_resolved = empty_args.resolve(DEFAULT_NIXCACHE_REPO);
        assert_eq!(env_resolved.0, "env/repo");
        assert_eq!(env_resolved.1, "quay.io");

        unsafe {
            env::remove_var("NIXCACHE_REPO");
            env::remove_var("NIXCACHE_REGISTRY");
        }
    }
}
