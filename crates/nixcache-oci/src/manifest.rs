use chrono::Utc;
use nixcache_core::SystemArch;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_IMAGE_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";

/// 强类型 OCI NixCache Layer 媒体类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheLayerMediaType {
    IndexV3Zstd,
    SessionV3Zstd,
}

impl CacheLayerMediaType {
    pub const INDEX_V3_ZSTD: &'static str = "application/vnd.nix.cache.index.v3+zstd";
    pub const SESSION_V3_ZSTD: &'static str = "application/vnd.nix.cache.session.v3+zstd";

    /// 从媒体类型字符串严格解析
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            Self::INDEX_V3_ZSTD => Some(Self::IndexV3Zstd),
            Self::SESSION_V3_ZSTD => Some(Self::SessionV3Zstd),
            _ => None,
        }
    }

    /// 转换为静态媒体类型字符串
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::IndexV3Zstd => Self::INDEX_V3_ZSTD,
            Self::SessionV3Zstd => Self::SESSION_V3_ZSTD,
        }
    }

    /// 是否为索引类型
    pub const fn is_index(&self) -> bool {
        matches!(self, Self::IndexV3Zstd)
    }

    /// 是否为会话类型
    pub const fn is_session(&self) -> bool {
        matches!(self, Self::SessionV3Zstd)
    }
}

/// OCI Platform 结构
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct OciPlatform {
    pub architecture: String,
    pub os: String,
    pub variant: Option<String>,
    #[serde(rename = "os.features")]
    pub os_features: Vec<String>,
}

impl OciPlatform {
    pub fn new(os: impl Into<String>, architecture: impl Into<String>) -> Self {
        Self {
            os: os.into(),
            architecture: architecture.into(),
            variant: None,
            os_features: Vec::new(),
        }
    }

    pub fn with_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = Some(variant.into());
        self
    }

    pub fn from_system(system: &SystemArch) -> Self {
        let (os, arch, variant) = system.to_oci_platform_tuple();
        Self {
            os: os.to_string(),
            architecture: arch.to_string(),
            variant: variant.map(|v| v.to_string()),
            os_features: Vec::new(),
        }
    }

    pub fn to_system(&self) -> SystemArch {
        SystemArch::from_oci(&self.os, &self.architecture, self.variant.as_deref())
    }
}

/// 强类型 OCI 内容描述符 (Content Descriptor，支持 Image Index 平台路由)
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OciDescriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    pub platform: Option<OciPlatform>,
    pub annotations: Option<HashMap<String, String>>,
}

/// 强类型 OCI Image Index 规范结构体
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OciImageIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub manifests: Vec<OciDescriptor>,
    pub annotations: Option<HashMap<String, String>>,
}

impl Default for OciImageIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl OciImageIndex {
    pub fn new() -> Self {
        Self {
            schema_version: 2,
            media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
            manifests: Vec::new(),
            annotations: None,
        }
    }

    /// 序列化为 JSON 字符串
    pub fn to_json_string(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 在 Index 中匹配指定 SystemArch 的 Sub-Manifest 描述符
    pub fn find_manifest_for_system(&self, system: &SystemArch) -> Option<&OciDescriptor> {
        let expected_platform = OciPlatform::from_system(system);
        self.manifests.iter().find(|m| {
            if let Some(ref p) = m.platform
                && p == &expected_platform
            {
                return true;
            }
            if let Some(ref ann) = m.annotations
                && let Some(sys_str) = ann.get("org.nixos.nixcache.system")
            {
                return sys_str == system.as_str();
            }
            false
        })
    }
}

/// 强类型 OCI Image Manifest 规范结构体
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OciImageManifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub config: OciDescriptor,
    pub layers: Vec<OciDescriptor>,
    pub annotations: Option<HashMap<String, String>>,
}

impl OciImageManifest {
    /// 序列化为 JSON 字符串
    pub fn to_json_string(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 提取第一层 Layer 的 Digest
    pub fn first_layer_digest(&self) -> Option<&str> {
        self.layers.first().map(|l| l.digest.as_str())
    }
}

/// 强类型 OCI 产物清单枚举 (完整支持 Image Index 与 Image Manifest)
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum OciArtifactManifest {
    Index(OciImageIndex),
    Manifest(OciImageManifest),
}

impl OciArtifactManifest {
    /// 序列化为 JSON 字符串
    pub fn to_json_string(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 获取底层 Schema Version
    pub fn schema_version(&self) -> u32 {
        match self {
            Self::Index(idx) => idx.schema_version,
            Self::Manifest(m) => m.schema_version,
        }
    }

    /// 获取底层 MediaType
    pub fn media_type(&self) -> &str {
        match self {
            Self::Index(idx) => &idx.media_type,
            Self::Manifest(m) => &m.media_type,
        }
    }

    /// 是否为 Image Index
    pub fn is_index(&self) -> bool {
        matches!(self, Self::Index(_))
    }

    /// 是否为 Image Manifest
    pub fn is_manifest(&self) -> bool {
        matches!(self, Self::Manifest(_))
    }

    /// 提取为 Image Index 引用
    pub fn as_index(&self) -> Option<&OciImageIndex> {
        match self {
            Self::Index(idx) => Some(idx),
            Self::Manifest(_) => None,
        }
    }

    /// 提取为 Image Manifest 引用
    pub fn as_manifest(&self) -> Option<&OciImageManifest> {
        match self {
            Self::Index(_) => None,
            Self::Manifest(m) => Some(m),
        }
    }
}

/// 构造强类型的单架构 Session Image Manifest
pub fn build_arch_session_manifest(
    session_blob_digest: &str,
    session_blob_size: u64,
    uncompressed_size: u64,
    config_digest: &str,
    config_size: u64,
    run_id: u64,
    system: &SystemArch,
) -> OciImageManifest {
    let mut layer_annotations = HashMap::new();
    layer_annotations.insert("org.nixos.nixcache.run_id".to_string(), run_id.to_string());
    layer_annotations.insert("org.nixos.nixcache.system".to_string(), system.to_string());
    layer_annotations.insert(
        "org.nixos.nixcache.uncompressed_size".to_string(),
        uncompressed_size.to_string(),
    );
    layer_annotations.insert(
        "org.nixos.nixcache.compression".to_string(),
        "zstd".to_string(),
    );
    layer_annotations.insert("org.nixos.nixcache.schema".to_string(), "5".to_string());

    let mut manifest_annotations = HashMap::new();
    manifest_annotations.insert(
        "org.opencontainers.image.created".to_string(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    manifest_annotations.insert(
        "org.opencontainers.image.description".to_string(),
        format!(
            "NixCache Workflow Run Session Sub-Manifest ({})",
            system.as_str()
        ),
    );
    manifest_annotations.insert("org.nixos.nixcache.system".to_string(), system.to_string());

    OciImageManifest {
        schema_version: 2,
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        config: OciDescriptor {
            media_type: OCI_IMAGE_CONFIG_MEDIA_TYPE.to_string(),
            digest: config_digest.to_string(),
            size: config_size,
            platform: None,
            annotations: None,
        },
        layers: vec![OciDescriptor {
            media_type: CacheLayerMediaType::SESSION_V3_ZSTD.to_string(),
            digest: session_blob_digest.to_string(),
            size: session_blob_size,
            platform: Some(OciPlatform::from_system(system)),
            annotations: Some(layer_annotations),
        }],
        annotations: Some(manifest_annotations),
    }
}

/// 构造强类型的单架构 Baseline Index Image Manifest (强制 V3+Zstd)
pub fn build_arch_index_manifest(
    index_blob_digest: &str,
    index_blob_size: u64,
    uncompressed_size: u64,
    config_digest: &str,
    config_size: u64,
    system: &SystemArch,
) -> OciImageManifest {
    let mut layer_annotations = HashMap::new();
    layer_annotations.insert("org.nixos.nixcache.system".to_string(), system.to_string());
    layer_annotations.insert(
        "org.nixos.nixcache.uncompressed_size".to_string(),
        uncompressed_size.to_string(),
    );
    layer_annotations.insert(
        "org.nixos.nixcache.compression".to_string(),
        "zstd".to_string(),
    );
    layer_annotations.insert("org.nixos.nixcache.schema".to_string(), "5".to_string());

    let mut manifest_annotations = HashMap::new();
    manifest_annotations.insert(
        "org.opencontainers.image.created".to_string(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    manifest_annotations.insert(
        "org.opencontainers.image.description".to_string(),
        format!(
            "NixCache Production Baseline Sub-Manifest ({})",
            system.as_str()
        ),
    );
    manifest_annotations.insert("org.nixos.nixcache.system".to_string(), system.to_string());

    OciImageManifest {
        schema_version: 2,
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        config: OciDescriptor {
            media_type: OCI_IMAGE_CONFIG_MEDIA_TYPE.to_string(),
            digest: config_digest.to_string(),
            size: config_size,
            platform: None,
            annotations: None,
        },
        layers: vec![OciDescriptor {
            media_type: CacheLayerMediaType::INDEX_V3_ZSTD.to_string(),
            digest: index_blob_digest.to_string(),
            size: index_blob_size,
            platform: Some(OciPlatform::from_system(system)),
            annotations: Some(layer_annotations),
        }],
        annotations: Some(manifest_annotations),
    }
}

/// 构造全局聚合 Image Index
pub fn build_image_index(
    manifest_descriptors: Vec<OciDescriptor>,
    description: &str,
) -> OciImageIndex {
    let mut annotations = HashMap::new();
    annotations.insert(
        "org.opencontainers.image.created".to_string(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    annotations.insert(
        "org.opencontainers.image.description".to_string(),
        description.to_string(),
    );

    OciImageIndex {
        schema_version: 2,
        media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
        manifests: manifest_descriptors,
        annotations: Some(annotations),
    }
}
