use crate::backend::kind::{BlobUploadStrategy, RegistryCapabilities, RegistryKind};
use std::{fmt::Debug, sync::Arc};

/// OCI 后端抽象驱动 (Rust 2024 原生 Trait)
pub trait OciBackendDriver: Send + Sync + Debug + 'static {
    /// 获取当前驱动所属后端类型
    fn kind(&self) -> RegistryKind;

    /// 获取当前后端的静态特性能力矩阵
    fn capabilities(&self) -> &'static RegistryCapabilities;

    /// 规范化 Registry 主机域名与端点 (例如将 docker.io 转换为 registry-1.docker.io)
    fn canonicalize_endpoint(&self, registry: &str) -> String;

    /// 规范化存储库路径 (例如为 Docker Hub 补充 library/ 前缀)
    fn canonicalize_repository(&self, repo: &str) -> String;

    /// 构造特定后端的 Scope 字符串
    fn format_auth_scope(&self, repo: &str, write: bool) -> String;

    /// 获取 Token 认证端点 URL
    fn resolve_token_endpoint(&self, registry: &str, repo: &str, write: bool) -> String;
}

static GHCR_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: false,
    supports_monolithic_post_1rtt: false,
    supports_manifest_cas_if_match: false,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::FixedTwoStepPut,
    custom_auth_endpoint: Some("https://ghcr.io/token"),
};

/// GitHub Container Registry 专属驱动 (ghcr.io)
#[derive(Debug, Default, Clone)]
pub struct GhcrDriver;

impl OciBackendDriver for GhcrDriver {
    fn kind(&self) -> RegistryKind {
        RegistryKind::Ghcr
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        &GHCR_CAPABILITIES
    }

    fn canonicalize_endpoint(&self, registry: &str) -> String {
        let clean = registry.trim().to_lowercase();
        if clean.is_empty() {
            "ghcr.io".to_string()
        } else {
            clean
        }
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_lowercase()
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }

    fn resolve_token_endpoint(&self, _registry: &str, repo: &str, write: bool) -> String {
        let scope = self.format_auth_scope(repo, write);
        format!("https://ghcr.io/token?service=ghcr.io&scope={}", scope)
    }
}

static DOCKER_HUB_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: true,
    supports_monolithic_post_1rtt: true,
    supports_manifest_cas_if_match: true,
    requires_library_namespace_expansion: true,
    fixed_upload_strategy: BlobUploadStrategy::PreferMonolithicPost,
    custom_auth_endpoint: Some("https://auth.docker.io/token"),
};

/// Docker Hub 专属驱动 (docker.io / registry-1.docker.io)
#[derive(Debug, Default, Clone)]
pub struct DockerHubDriver;

impl OciBackendDriver for DockerHubDriver {
    fn kind(&self) -> RegistryKind {
        RegistryKind::DockerHub
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        &DOCKER_HUB_CAPABILITIES
    }

    fn canonicalize_endpoint(&self, registry: &str) -> String {
        let clean = registry.trim().to_lowercase();
        if clean == "docker.io" || clean == "index.docker.io" || clean.is_empty() {
            "registry-1.docker.io".to_string()
        } else {
            clean
        }
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        let clean = repo.trim_matches('/').to_lowercase();
        if !clean.contains('/') {
            format!("library/{}", clean)
        } else {
            clean
        }
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }

    fn resolve_token_endpoint(&self, _registry: &str, repo: &str, write: bool) -> String {
        let scope = self.format_auth_scope(repo, write);
        format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope={}",
            scope
        )
    }
}

static AWS_ECR_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: false,
    supports_monolithic_post_1rtt: true,
    supports_manifest_cas_if_match: false,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::PreferMonolithicPost,
    custom_auth_endpoint: None,
};

/// AWS ECR 驱动 (*.dkr.ecr.*.amazonaws.com)
#[derive(Debug, Default, Clone)]
pub struct AwsEcrDriver;

impl OciBackendDriver for AwsEcrDriver {
    fn kind(&self) -> RegistryKind {
        RegistryKind::AwsEcr
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        &AWS_ECR_CAPABILITIES
    }

    fn canonicalize_endpoint(&self, registry: &str) -> String {
        registry.trim().to_lowercase()
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_lowercase()
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }

    fn resolve_token_endpoint(&self, registry: &str, repo: &str, write: bool) -> String {
        let clean_reg = self.canonicalize_endpoint(registry);
        let scope = self.format_auth_scope(repo, write);
        format!(
            "https://{}/v2/token?service={}&scope={}",
            clean_reg, clean_reg, scope
        )
    }
}

static GCP_GAR_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: true,
    supports_monolithic_post_1rtt: true,
    supports_manifest_cas_if_match: true,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::PreferMonolithicPost,
    custom_auth_endpoint: None,
};

/// Google Cloud Artifact Registry 驱动 (*-docker.pkg.dev)
#[derive(Debug, Default, Clone)]
pub struct GcpArtifactRegistryDriver;

impl OciBackendDriver for GcpArtifactRegistryDriver {
    fn kind(&self) -> RegistryKind {
        RegistryKind::GcpArtifactRegistry
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        &GCP_GAR_CAPABILITIES
    }

    fn canonicalize_endpoint(&self, registry: &str) -> String {
        registry.trim().to_lowercase()
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_lowercase()
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }

    fn resolve_token_endpoint(&self, registry: &str, repo: &str, write: bool) -> String {
        let clean_reg = self.canonicalize_endpoint(registry);
        let scope = self.format_auth_scope(repo, write);
        format!(
            "https://{}/v2/token?service={}&scope={}",
            clean_reg, clean_reg, scope
        )
    }
}

static AZURE_ACR_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: true,
    supports_monolithic_post_1rtt: true,
    supports_manifest_cas_if_match: true,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::PreferMonolithicPost,
    custom_auth_endpoint: None,
};

/// Azure Container Registry 驱动 (*.azurecr.io)
#[derive(Debug, Default, Clone)]
pub struct AzureAcrDriver;

impl OciBackendDriver for AzureAcrDriver {
    fn kind(&self) -> RegistryKind {
        RegistryKind::AzureAcr
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        &AZURE_ACR_CAPABILITIES
    }

    fn canonicalize_endpoint(&self, registry: &str) -> String {
        registry.trim().to_lowercase()
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_lowercase()
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }

    fn resolve_token_endpoint(&self, registry: &str, repo: &str, write: bool) -> String {
        let clean_reg = self.canonicalize_endpoint(registry);
        let scope = self.format_auth_scope(repo, write);
        format!(
            "https://{}/oauth2/token?service={}&scope={}",
            clean_reg, clean_reg, scope
        )
    }
}

static GENERIC_OCI_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: true,
    supports_monolithic_post_1rtt: true,
    supports_manifest_cas_if_match: true,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::PreferMonolithicPost,
    custom_auth_endpoint: None,
};

/// 通用符合 OCI 标准的驱动 (Harbor, Zot, Distribution, Quay 等)
#[derive(Debug, Default, Clone)]
pub struct GenericOciDriver;

impl OciBackendDriver for GenericOciDriver {
    fn kind(&self) -> RegistryKind {
        RegistryKind::GenericOci
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        &GENERIC_OCI_CAPABILITIES
    }

    fn canonicalize_endpoint(&self, registry: &str) -> String {
        registry.trim().to_lowercase()
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_string()
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }

    fn resolve_token_endpoint(&self, registry: &str, repo: &str, write: bool) -> String {
        let clean_reg = self.canonicalize_endpoint(registry);
        let scope = self.format_auth_scope(repo, write);
        let scheme = if clean_reg.starts_with("localhost") || clean_reg.starts_with("127.0.0.1") {
            "http"
        } else {
            "https"
        };
        format!(
            "{}://{}/token?service={}&scope={}",
            scheme, clean_reg, clean_reg, scope
        )
    }
}

/// 根据后端类型获取对应的单例/共享驱动实例
pub fn driver_for_kind(kind: RegistryKind) -> Arc<dyn OciBackendDriver> {
    match kind {
        RegistryKind::Ghcr => Arc::new(GhcrDriver),
        RegistryKind::DockerHub => Arc::new(DockerHubDriver),
        RegistryKind::AwsEcr => Arc::new(AwsEcrDriver),
        RegistryKind::GcpArtifactRegistry => Arc::new(GcpArtifactRegistryDriver),
        RegistryKind::AzureAcr => Arc::new(AzureAcrDriver),
        RegistryKind::GenericOci => Arc::new(GenericOciDriver),
    }
}

/// 根据注册表域名推导并返回驱动实例
pub fn detect_driver(registry: &str) -> Arc<dyn OciBackendDriver> {
    let kind = RegistryKind::detect(registry);
    driver_for_kind(kind)
}
