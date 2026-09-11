use super::{RegistryEndpoint, RegistryEndpointError};
use crate::backend::kind::{
    BlobUploadStrategy, ManifestCasSupport, PackageDeletionSupport, RegistryCapabilities,
    RegistryDeletionStrategy, RegistryKind,
};
use std::{fmt::Debug, sync::Arc};

/// OCI 后端抽象驱动 (Rust 2024 原生 Trait)
pub trait OciBackendDriver: Send + Sync + Debug + 'static {
    /// 获取当前驱动所属后端类型
    fn kind(&self) -> RegistryKind;

    /// 获取当前后端的静态特性能力矩阵
    fn capabilities(&self) -> &'static RegistryCapabilities;

    /// 规范化 Registry 主机域名与端点 (例如将 docker.io 转换为 registry-1.docker.io)
    fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError>;

    /// 规范化存储库路径 (例如为 Docker Hub 补充 library/ 前缀)
    fn canonicalize_repository(&self, repo: &str) -> String;

    /// 构造特定后端的 Scope 字符串
    fn format_auth_scope(&self, repo: &str, write: bool) -> String;

    /// token exchange 使用的默认 Basic 用户名。Generic OCI 不猜测用户名。
    fn default_basic_username(&self) -> Option<&'static str> {
        None
    }
}

pub static GHCR_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: false,
    supports_monolithic_post_1rtt: false,
    manifest_cas_support: ManifestCasSupport::Unsupported,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::FixedTwoStepPut,
    deletion_strategy: RegistryDeletionStrategy::GitHubPackagesRestApi,
    supports_blob_physical_deletion: false,
    package_deletion_support: PackageDeletionSupport::NativeComplete,
};

/// GitHub Container Registry 专属驱动 (ghcr.io)
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GhcrDriver;

impl GhcrDriver {
    #[inline(always)]
    pub fn kind(&self) -> RegistryKind {
        RegistryKind::Ghcr
    }

    #[inline(always)]
    pub fn capabilities(&self) -> &'static RegistryCapabilities {
        &GHCR_CAPABILITIES
    }

    #[inline(always)]
    pub fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        let registry = if registry.trim().is_empty() {
            "ghcr.io"
        } else {
            registry
        };
        RegistryEndpoint::parse(registry)
    }

    #[inline(always)]
    pub fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_lowercase()
    }

    #[inline(always)]
    pub fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }
}

impl OciBackendDriver for GhcrDriver {
    fn kind(&self) -> RegistryKind {
        GhcrDriver::kind(self)
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        GhcrDriver::capabilities(self)
    }

    fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        GhcrDriver::canonicalize_endpoint(self, registry)
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        GhcrDriver::canonicalize_repository(self, repo)
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        GhcrDriver::format_auth_scope(self, repo, write)
    }

    fn default_basic_username(&self) -> Option<&'static str> {
        Some("token")
    }
}

pub static DOCKER_HUB_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: true,
    supports_monolithic_post_1rtt: true,
    manifest_cas_support: ManifestCasSupport::IfMatch,
    requires_library_namespace_expansion: true,
    fixed_upload_strategy: BlobUploadStrategy::PreferMonolithicPost,
    deletion_strategy: RegistryDeletionStrategy::DockerHubRestApi,
    supports_blob_physical_deletion: false,
    package_deletion_support: PackageDeletionSupport::Unsupported,
};

/// Docker Hub 专属驱动 (docker.io / registry-1.docker.io)
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DockerHubDriver;

impl DockerHubDriver {
    #[inline(always)]
    pub fn kind(&self) -> RegistryKind {
        RegistryKind::DockerHub
    }

    #[inline(always)]
    pub fn capabilities(&self) -> &'static RegistryCapabilities {
        &DOCKER_HUB_CAPABILITIES
    }

    #[inline(always)]
    pub fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        let registry = if registry.trim().is_empty() {
            "registry-1.docker.io"
        } else {
            registry
        };
        let endpoint = RegistryEndpoint::parse(registry)?;
        if matches!(endpoint.host(), "docker.io" | "index.docker.io") {
            Ok(endpoint.replace_host("registry-1.docker.io"))
        } else {
            Ok(endpoint)
        }
    }

    #[inline(always)]
    pub fn canonicalize_repository(&self, repo: &str) -> String {
        let clean = repo.trim_matches('/').to_lowercase();
        if !clean.contains('/') {
            format!("library/{}", clean)
        } else {
            clean
        }
    }

    #[inline(always)]
    pub fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }
}

impl OciBackendDriver for DockerHubDriver {
    fn kind(&self) -> RegistryKind {
        DockerHubDriver::kind(self)
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        DockerHubDriver::capabilities(self)
    }

    fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        DockerHubDriver::canonicalize_endpoint(self, registry)
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        DockerHubDriver::canonicalize_repository(self, repo)
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        DockerHubDriver::format_auth_scope(self, repo, write)
    }
}

pub static AWS_ECR_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: false,
    supports_monolithic_post_1rtt: true,
    manifest_cas_support: ManifestCasSupport::Unsupported,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::PreferMonolithicPost,
    deletion_strategy: RegistryDeletionStrategy::AwsEcrApi,
    supports_blob_physical_deletion: false,
    package_deletion_support: PackageDeletionSupport::Unsupported,
};

/// AWS ECR 驱动 (*.dkr.ecr.*.amazonaws.com)
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AwsEcrDriver;

impl AwsEcrDriver {
    #[inline(always)]
    pub fn kind(&self) -> RegistryKind {
        RegistryKind::AwsEcr
    }

    #[inline(always)]
    pub fn capabilities(&self) -> &'static RegistryCapabilities {
        &AWS_ECR_CAPABILITIES
    }

    #[inline(always)]
    pub fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        RegistryEndpoint::parse(registry)
    }

    #[inline(always)]
    pub fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_lowercase()
    }

    #[inline(always)]
    pub fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }
}

impl OciBackendDriver for AwsEcrDriver {
    fn kind(&self) -> RegistryKind {
        AwsEcrDriver::kind(self)
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        AwsEcrDriver::capabilities(self)
    }

    fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        AwsEcrDriver::canonicalize_endpoint(self, registry)
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        AwsEcrDriver::canonicalize_repository(self, repo)
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        AwsEcrDriver::format_auth_scope(self, repo, write)
    }

    fn default_basic_username(&self) -> Option<&'static str> {
        Some("AWS")
    }
}

pub static GCP_GAR_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: true,
    supports_monolithic_post_1rtt: true,
    manifest_cas_support: ManifestCasSupport::IfMatch,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::PreferMonolithicPost,
    deletion_strategy: RegistryDeletionStrategy::StandardOciDelete,
    supports_blob_physical_deletion: true,
    package_deletion_support: PackageDeletionSupport::TaggedGraphOnly,
};

/// Google Cloud Artifact Registry 驱动 (*-docker.pkg.dev)
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GcpArtifactRegistryDriver;

impl GcpArtifactRegistryDriver {
    #[inline(always)]
    pub fn kind(&self) -> RegistryKind {
        RegistryKind::GcpArtifactRegistry
    }

    #[inline(always)]
    pub fn capabilities(&self) -> &'static RegistryCapabilities {
        &GCP_GAR_CAPABILITIES
    }

    #[inline(always)]
    pub fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        RegistryEndpoint::parse(registry)
    }

    #[inline(always)]
    pub fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_lowercase()
    }

    #[inline(always)]
    pub fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }
}

impl OciBackendDriver for GcpArtifactRegistryDriver {
    fn kind(&self) -> RegistryKind {
        GcpArtifactRegistryDriver::kind(self)
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        GcpArtifactRegistryDriver::capabilities(self)
    }

    fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        GcpArtifactRegistryDriver::canonicalize_endpoint(self, registry)
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        GcpArtifactRegistryDriver::canonicalize_repository(self, repo)
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        GcpArtifactRegistryDriver::format_auth_scope(self, repo, write)
    }
}

pub static AZURE_ACR_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: true,
    supports_monolithic_post_1rtt: true,
    manifest_cas_support: ManifestCasSupport::IfMatch,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::PreferMonolithicPost,
    deletion_strategy: RegistryDeletionStrategy::StandardOciDelete,
    supports_blob_physical_deletion: true,
    package_deletion_support: PackageDeletionSupport::TaggedGraphOnly,
};

/// Azure Container Registry 驱动 (*.azurecr.io)
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AzureAcrDriver;

impl AzureAcrDriver {
    #[inline(always)]
    pub fn kind(&self) -> RegistryKind {
        RegistryKind::AzureAcr
    }

    #[inline(always)]
    pub fn capabilities(&self) -> &'static RegistryCapabilities {
        &AZURE_ACR_CAPABILITIES
    }

    #[inline(always)]
    pub fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        RegistryEndpoint::parse(registry)
    }

    #[inline(always)]
    pub fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_lowercase()
    }

    #[inline(always)]
    pub fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }
}

impl OciBackendDriver for AzureAcrDriver {
    fn kind(&self) -> RegistryKind {
        AzureAcrDriver::kind(self)
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        AzureAcrDriver::capabilities(self)
    }

    fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        AzureAcrDriver::canonicalize_endpoint(self, registry)
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        AzureAcrDriver::canonicalize_repository(self, repo)
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        AzureAcrDriver::format_auth_scope(self, repo, write)
    }
}

pub static GENERIC_OCI_CAPABILITIES: RegistryCapabilities = RegistryCapabilities {
    supports_chunked_patch: true,
    supports_monolithic_post_1rtt: true,
    manifest_cas_support: ManifestCasSupport::Unsupported,
    requires_library_namespace_expansion: false,
    fixed_upload_strategy: BlobUploadStrategy::ResumableChunkedPatch,
    deletion_strategy: RegistryDeletionStrategy::StandardOciDelete,
    supports_blob_physical_deletion: true,
    package_deletion_support: PackageDeletionSupport::TaggedGraphOnly,
};

/// 通用符合 OCI 标准的驱动 (Harbor, Zot, Distribution, Quay 等)
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GenericOciDriver;

impl GenericOciDriver {
    #[inline(always)]
    pub fn kind(&self) -> RegistryKind {
        RegistryKind::GenericOci
    }

    #[inline(always)]
    pub fn capabilities(&self) -> &'static RegistryCapabilities {
        &GENERIC_OCI_CAPABILITIES
    }

    #[inline(always)]
    pub fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        RegistryEndpoint::parse(registry)
    }

    #[inline(always)]
    pub fn canonicalize_repository(&self, repo: &str) -> String {
        repo.trim_matches('/').to_string()
    }

    #[inline(always)]
    pub fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        let canonical_repo = self.canonicalize_repository(repo);
        let target_repo = if canonical_repo.ends_with("/nix-cache") {
            canonical_repo
        } else {
            format!("{}/nix-cache", canonical_repo)
        };
        let action = if write { "pull,push" } else { "pull" };
        format!("repository:{}:{}", target_repo, action)
    }
}

impl OciBackendDriver for GenericOciDriver {
    fn kind(&self) -> RegistryKind {
        GenericOciDriver::kind(self)
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        GenericOciDriver::capabilities(self)
    }

    fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        GenericOciDriver::canonicalize_endpoint(self, registry)
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        GenericOciDriver::canonicalize_repository(self, repo)
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        GenericOciDriver::format_auth_scope(self, repo, write)
    }
}

/// OCI 驱动统一枚举分发 (零开销 Enum Dispatch，消除虚表与动态堆分配)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OciDriver {
    Ghcr(GhcrDriver),
    DockerHub(DockerHubDriver),
    AwsEcr(AwsEcrDriver),
    Gcp(GcpArtifactRegistryDriver),
    Azure(AzureAcrDriver),
    Generic(GenericOciDriver),
}

impl Default for OciDriver {
    #[inline(always)]
    fn default() -> Self {
        Self::Ghcr(GhcrDriver)
    }
}

impl From<RegistryKind> for OciDriver {
    #[inline(always)]
    fn from(kind: RegistryKind) -> Self {
        driver_for_kind(kind)
    }
}

impl From<GhcrDriver> for OciDriver {
    #[inline(always)]
    fn from(d: GhcrDriver) -> Self {
        Self::Ghcr(d)
    }
}

impl From<DockerHubDriver> for OciDriver {
    #[inline(always)]
    fn from(d: DockerHubDriver) -> Self {
        Self::DockerHub(d)
    }
}

impl From<AwsEcrDriver> for OciDriver {
    #[inline(always)]
    fn from(d: AwsEcrDriver) -> Self {
        Self::AwsEcr(d)
    }
}

impl From<GcpArtifactRegistryDriver> for OciDriver {
    #[inline(always)]
    fn from(d: GcpArtifactRegistryDriver) -> Self {
        Self::Gcp(d)
    }
}

impl From<AzureAcrDriver> for OciDriver {
    #[inline(always)]
    fn from(d: AzureAcrDriver) -> Self {
        Self::Azure(d)
    }
}

impl From<GenericOciDriver> for OciDriver {
    #[inline(always)]
    fn from(d: GenericOciDriver) -> Self {
        Self::Generic(d)
    }
}

impl<T: Into<OciDriver> + Copy> From<&T> for OciDriver {
    #[inline(always)]
    fn from(d: &T) -> Self {
        (*d).into()
    }
}

impl<T: Into<OciDriver> + Copy> From<Arc<T>> for OciDriver {
    #[inline(always)]
    fn from(d: Arc<T>) -> Self {
        (*d).into()
    }
}

impl OciDriver {
    #[inline(always)]
    pub fn kind(&self) -> RegistryKind {
        match self {
            Self::Ghcr(d) => d.kind(),
            Self::DockerHub(d) => d.kind(),
            Self::AwsEcr(d) => d.kind(),
            Self::Gcp(d) => d.kind(),
            Self::Azure(d) => d.kind(),
            Self::Generic(d) => d.kind(),
        }
    }

    #[inline(always)]
    pub fn default_basic_username(&self) -> Option<&'static str> {
        match self {
            Self::Ghcr(d) => d.default_basic_username(),
            Self::DockerHub(d) => d.default_basic_username(),
            Self::AwsEcr(d) => d.default_basic_username(),
            Self::Gcp(d) => d.default_basic_username(),
            Self::Azure(d) => d.default_basic_username(),
            Self::Generic(d) => d.default_basic_username(),
        }
    }

    #[inline(always)]
    pub fn capabilities(&self) -> &'static RegistryCapabilities {
        match self {
            Self::Ghcr(_) => &GHCR_CAPABILITIES,
            Self::DockerHub(_) => &DOCKER_HUB_CAPABILITIES,
            Self::AwsEcr(_) => &AWS_ECR_CAPABILITIES,
            Self::Gcp(_) => &GCP_GAR_CAPABILITIES,
            Self::Azure(_) => &AZURE_ACR_CAPABILITIES,
            Self::Generic(_) => &GENERIC_OCI_CAPABILITIES,
        }
    }

    #[inline(always)]
    pub fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        match self {
            Self::Ghcr(d) => d.canonicalize_endpoint(registry),
            Self::DockerHub(d) => d.canonicalize_endpoint(registry),
            Self::AwsEcr(d) => d.canonicalize_endpoint(registry),
            Self::Gcp(d) => d.canonicalize_endpoint(registry),
            Self::Azure(d) => d.canonicalize_endpoint(registry),
            Self::Generic(d) => d.canonicalize_endpoint(registry),
        }
    }

    #[inline(always)]
    pub fn canonicalize_repository(&self, repo: &str) -> String {
        match self {
            Self::Ghcr(d) => d.canonicalize_repository(repo),
            Self::DockerHub(d) => d.canonicalize_repository(repo),
            Self::AwsEcr(d) => d.canonicalize_repository(repo),
            Self::Gcp(d) => d.canonicalize_repository(repo),
            Self::Azure(d) => d.canonicalize_repository(repo),
            Self::Generic(d) => d.canonicalize_repository(repo),
        }
    }

    #[inline(always)]
    pub fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        match self {
            Self::Ghcr(d) => d.format_auth_scope(repo, write),
            Self::DockerHub(d) => d.format_auth_scope(repo, write),
            Self::AwsEcr(d) => d.format_auth_scope(repo, write),
            Self::Gcp(d) => d.format_auth_scope(repo, write),
            Self::Azure(d) => d.format_auth_scope(repo, write),
            Self::Generic(d) => d.format_auth_scope(repo, write),
        }
    }
}

impl OciBackendDriver for OciDriver {
    fn kind(&self) -> RegistryKind {
        self.kind()
    }

    fn capabilities(&self) -> &'static RegistryCapabilities {
        self.capabilities()
    }

    fn canonicalize_endpoint(
        &self,
        registry: &str,
    ) -> Result<RegistryEndpoint, RegistryEndpointError> {
        self.canonicalize_endpoint(registry)
    }

    fn canonicalize_repository(&self, repo: &str) -> String {
        self.canonicalize_repository(repo)
    }

    fn format_auth_scope(&self, repo: &str, write: bool) -> String {
        self.format_auth_scope(repo, write)
    }

    fn default_basic_username(&self) -> Option<&'static str> {
        self.default_basic_username()
    }
}

/// 根据后端类型获取对应的驱动实例
#[inline(always)]
pub fn driver_for_kind(kind: RegistryKind) -> OciDriver {
    match kind {
        RegistryKind::Ghcr => OciDriver::Ghcr(GhcrDriver),
        RegistryKind::DockerHub => OciDriver::DockerHub(DockerHubDriver),
        RegistryKind::AwsEcr => OciDriver::AwsEcr(AwsEcrDriver),
        RegistryKind::GcpArtifactRegistry => OciDriver::Gcp(GcpArtifactRegistryDriver),
        RegistryKind::AzureAcr => OciDriver::Azure(AzureAcrDriver),
        RegistryKind::GenericOci => OciDriver::Generic(GenericOciDriver),
    }
}

/// 根据注册表域名推导并返回驱动实例
#[inline(always)]
pub fn detect_driver(registry: &str) -> OciDriver {
    let kind = RegistryKind::detect(registry);
    driver_for_kind(kind)
}
