use clap::Args;
use nixcache_oci::{OciDriver, RegistryKind, driver_for_kind};
use nixcache_utils::Env;

pub const DEFAULT_NIXCACHE_REPO: &str = "shaogme/nixcache-oci";
pub const DEFAULT_NIXCACHE_REGISTRY: &str = "ghcr.io";

/// OCI 目标存储库、注册表及强类型后端种类参数组
#[derive(Args, Debug, Clone, Default)]
pub struct OciTargetArgs {
    #[arg(long, help = "OCI repository (owner/repo) [env: NIXCACHE_REPO]")]
    pub repo: Option<String>,

    #[arg(long, help = "OCI registry [env: NIXCACHE_REGISTRY]")]
    pub registry: Option<String>,

    #[arg(
        long = "registry-kind",
        help = "OCI registry backend kind (ghcr, docker_hub, aws_ecr, gcp_artifact_registry, azure_acr, generic_oci) [env: NIXCACHE_REGISTRY_KIND]"
    )]
    pub registry_kind: Option<RegistryKind>,
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

    /// 解析注册表后端种类 (显式参数 > 环境变量 > 基于 Registry 域名自动探测)
    pub fn resolve_kind(&self, default_registry: &str) -> RegistryKind {
        if let Some(kind) = self.registry_kind {
            return kind;
        }

        if let Some(env_kind_str) = Env::get("NIXCACHE_REGISTRY_KIND")
            && let Ok(k) = env_kind_str.parse::<RegistryKind>()
        {
            return k;
        }

        let registry = self.resolve_registry(default_registry);
        RegistryKind::detect(&registry)
    }

    /// 获取对应的 OCI 后端抽象驱动
    pub fn resolve_driver(&self, default_repo: &str) -> (String, String, OciDriver) {
        let repo = self.resolve_repo(default_repo);
        let registry = self.resolve_registry(DEFAULT_NIXCACHE_REGISTRY);
        let kind = self.resolve_kind(DEFAULT_NIXCACHE_REGISTRY);
        let driver = driver_for_kind(kind);
        (repo, registry, driver)
    }

    /// 同时解析 (repo, registry)
    pub fn resolve(&self, default_repo: &str) -> (String, String) {
        (
            self.resolve_repo(default_repo),
            self.resolve_registry(DEFAULT_NIXCACHE_REGISTRY),
        )
    }

    /// 同时解析 (repo, registry, registry_kind)
    pub fn resolve_all(&self, default_repo: &str) -> (String, String, RegistryKind) {
        let (repo, reg) = self.resolve(default_repo);
        let kind = self.resolve_kind(DEFAULT_NIXCACHE_REGISTRY);
        (repo, reg, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_NIXCACHE_REGISTRY, DEFAULT_NIXCACHE_REPO, OciTargetArgs};
    use nixcache_oci::RegistryKind;
    use std::env;

    #[test]
    fn test_oci_args_resolution() {
        let empty_args = OciTargetArgs::default();
        let (repo, reg) = empty_args.resolve(DEFAULT_NIXCACHE_REPO);
        assert_eq!(repo, DEFAULT_NIXCACHE_REPO);
        assert_eq!(reg, DEFAULT_NIXCACHE_REGISTRY);
        assert_eq!(
            empty_args.resolve_kind(DEFAULT_NIXCACHE_REGISTRY),
            RegistryKind::Ghcr
        );

        let explicit_args = OciTargetArgs {
            repo: Some("custom/repo".to_string()),
            registry: Some("docker.io".to_string()),
            registry_kind: None,
        };
        let (repo, reg) = explicit_args.resolve(DEFAULT_NIXCACHE_REPO);
        assert_eq!(repo, "custom/repo");
        assert_eq!(reg, "docker.io");
        assert_eq!(
            explicit_args.resolve_kind(DEFAULT_NIXCACHE_REGISTRY),
            RegistryKind::DockerHub
        );

        let custom_kind_args = OciTargetArgs {
            repo: Some("custom/repo".to_string()),
            registry: Some("registry.internal.corp".to_string()),
            registry_kind: Some(RegistryKind::GenericOci),
        };
        assert_eq!(
            custom_kind_args.resolve_kind(DEFAULT_NIXCACHE_REGISTRY),
            RegistryKind::GenericOci
        );

        unsafe {
            env::set_var("NIXCACHE_REPO", "env/repo");
            env::set_var("NIXCACHE_REGISTRY", "quay.io");
            env::set_var("NIXCACHE_REGISTRY_KIND", "generic_oci");
        }
        let env_resolved = empty_args.resolve_all(DEFAULT_NIXCACHE_REPO);
        assert_eq!(env_resolved.0, "env/repo");
        assert_eq!(env_resolved.1, "quay.io");
        assert_eq!(env_resolved.2, RegistryKind::GenericOci);

        unsafe {
            env::remove_var("NIXCACHE_REPO");
            env::remove_var("NIXCACHE_REGISTRY");
            env::remove_var("NIXCACHE_REGISTRY_KIND");
        }
    }
}
