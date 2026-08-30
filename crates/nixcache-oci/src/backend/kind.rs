use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// 强类型 OCI 注册表后端种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RegistryKind {
    /// GitHub Container Registry (ghcr.io)
    #[default]
    Ghcr,
    /// Docker Hub (docker.io / registry-1.docker.io)
    DockerHub,
    /// AWS Elastic Container Registry (*.dkr.ecr.*.amazonaws.com)
    AwsEcr,
    /// Google Cloud Artifact Registry (*-docker.pkg.dev)
    GcpArtifactRegistry,
    /// Azure Container Registry (*.azurecr.io)
    AzureAcr,
    /// 通用符合 OCI 规范的注册表 (Harbor, Zot, Distribution, Quay 等)
    GenericOci,
}

impl fmt::Display for RegistryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ghcr => write!(f, "ghcr"),
            Self::DockerHub => write!(f, "docker_hub"),
            Self::AwsEcr => write!(f, "aws_ecr"),
            Self::GcpArtifactRegistry => write!(f, "gcp_artifact_registry"),
            Self::AzureAcr => write!(f, "azure_acr"),
            Self::GenericOci => write!(f, "generic_oci"),
        }
    }
}

impl FromStr for RegistryKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let clean = s.trim().to_lowercase().replace('-', "_");
        match clean.as_str() {
            "ghcr" | "ghcr.io" | "github" => Ok(Self::Ghcr),
            "docker" | "dockerhub" | "docker_hub" | "docker.io" => Ok(Self::DockerHub),
            "aws" | "ecr" | "aws_ecr" | "awsecr" => Ok(Self::AwsEcr),
            "gcp" | "gar" | "gcr" | "gcp_artifact_registry" | "artifactregistry" => {
                Ok(Self::GcpArtifactRegistry)
            }
            "azure" | "acr" | "azure_acr" | "azureacr" => Ok(Self::AzureAcr),
            "generic" | "generic_oci" | "genericoci" | "harbor" | "zot" | "quay" => {
                Ok(Self::GenericOci)
            }
            _ => Err(format!("Unknown registry kind: '{}'", s)),
        }
    }
}

impl RegistryKind {
    /// 基于注册表域名或端点字符串自动探测推导后端种类
    pub fn detect(registry: &str) -> Self {
        let clean = registry.trim().to_lowercase();
        let host = clean
            .strip_prefix("https://")
            .or_else(|| clean.strip_prefix("http://"))
            .unwrap_or(&clean);
        let host = host.split('/').next().unwrap_or(host);
        let host = host.split(':').next().unwrap_or(host);

        if host == "ghcr.io" {
            Self::Ghcr
        } else if host == "docker.io" || host == "index.docker.io" || host == "registry-1.docker.io"
        {
            Self::DockerHub
        } else if host.contains(".dkr.ecr.") && host.ends_with(".amazonaws.com") {
            Self::AwsEcr
        } else if host.ends_with("-docker.pkg.dev") || host == "gcr.io" {
            Self::GcpArtifactRegistry
        } else if host.ends_with(".azurecr.io") {
            Self::AzureAcr
        } else {
            Self::GenericOci
        }
    }
}

/// 固化的 Blob 上传协议模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlobUploadStrategy {
    /// 固化强制两阶段 Monolithic PUT (POST /uploads/ -> PUT <url>?digest=...)
    /// 适用于 GHCR，严禁发送 PATCH，禁止失败降级
    #[default]
    FixedTwoStepPut,
    /// 优先 1-RTT Monolithic POST 直传，失败时确定性走两阶段 PUT
    PreferMonolithicPost,
    /// 允许分块断点续传 (适用于支持分块 PATCH 的大型包与自建 Registry)
    ResumableChunkedPatch,
}

impl fmt::Display for BlobUploadStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FixedTwoStepPut => write!(f, "fixed_two_step_put"),
            Self::PreferMonolithicPost => write!(f, "prefer_monolithic_post"),
            Self::ResumableChunkedPatch => write!(f, "resumable_chunked_patch"),
        }
    }
}

impl FromStr for BlobUploadStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let clean = s.trim().to_lowercase().replace('-', "_");
        match clean.as_str() {
            "fixed_two_step_put" | "two_step_put" | "twostep" => Ok(Self::FixedTwoStepPut),
            "prefer_monolithic_post" | "monolithic_post" | "monolithic" => {
                Ok(Self::PreferMonolithicPost)
            }
            "resumable_chunked_patch" | "chunked_patch" | "chunked" => {
                Ok(Self::ResumableChunkedPatch)
            }
            _ => Err(format!("Unknown blob upload strategy: '{}'", s)),
        }
    }
}

/// 后端静态能力描述符 (编译期/初始化时确定，拒绝运行时嗅探)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryCapabilities {
    /// 是否支持 PATCH 分块上传
    pub supports_chunked_patch: bool,
    /// 是否支持 1-RTT Monolithic POST 直传
    pub supports_monolithic_post_1rtt: bool,
    /// 是否支持 Manifest CAS 提交 (If-Match 头)
    pub supports_manifest_cas_if_match: bool,
    /// 是否需要官方命名空间自动补齐 (例如 docker.io 的 library/)
    pub requires_library_namespace_expansion: bool,
    /// 固定的上传协议策略
    pub fixed_upload_strategy: BlobUploadStrategy,
    /// 专用 Token Auth Server 覆盖地址
    pub custom_auth_endpoint: Option<&'static str>,

    // === 新增核心删除能力字段 ===
    /// 当前后端采用的删除调度策略
    pub deletion_strategy: RegistryDeletionStrategy,
    /// 是否支持物理删除 OCI NAR Blobs
    pub supports_blob_physical_deletion: bool,
    /// 是否支持物理删除整个 Package / Repository
    pub supports_package_deletion: bool,
}

/// 注册表后端删除策略分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RegistryDeletionStrategy {
    /// 基于 GitHub Packages REST API 进行包、版本与 Tag 物理删除 (GHCR)
    #[default]
    GitHubPackagesRestApi,
    /// 基于 Docker Hub 专用 REST API 进行 Tag 删除 (Docker Hub)
    DockerHubRestApi,
    /// 基于 AWS ECR API (BatchDeleteImage) 进行删除
    AwsEcrApi,
    /// 遵循标准 OCI Distribution Spec 1.1 的 HTTP DELETE 端点 (Generic OCI, Harbor, Zot, Azure ACR 等)
    StandardOciDelete,
    /// 明确不支持任何物理删除操作的后端
    Unsupported,
}

impl fmt::Display for RegistryDeletionStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitHubPackagesRestApi => write!(f, "github_packages_rest_api"),
            Self::DockerHubRestApi => write!(f, "docker_hub_rest_api"),
            Self::AwsEcrApi => write!(f, "aws_ecr_api"),
            Self::StandardOciDelete => write!(f, "standard_oci_delete"),
            Self::Unsupported => write!(f, "unsupported"),
        }
    }
}

impl FromStr for RegistryDeletionStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let clean = s.trim().to_lowercase().replace('-', "_");
        match clean.as_str() {
            "github_packages_rest_api" | "ghcr" | "github" => Ok(Self::GitHubPackagesRestApi),
            "docker_hub_rest_api" | "dockerhub" | "docker" => Ok(Self::DockerHubRestApi),
            "aws_ecr_api" | "ecr" | "aws" => Ok(Self::AwsEcrApi),
            "standard_oci_delete" | "standard" | "oci" => Ok(Self::StandardOciDelete),
            "unsupported" | "none" => Ok(Self::Unsupported),
            _ => Err(format!("Unknown registry deletion strategy: '{}'", s)),
        }
    }
}
