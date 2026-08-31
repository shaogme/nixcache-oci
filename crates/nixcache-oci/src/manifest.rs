use chrono::Utc;
use nixcache_core::SystemArch;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_IMAGE_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";

/// OCI 空配置 Blob Sha256 散列常量 ("{}" 的 sha256 结果)
pub const EMPTY_CONFIG_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
/// OCI 空配置 Blob 大小 (2 字节)
pub const EMPTY_CONFIG_SIZE: u64 = 2;

/// Schema v5 媒体类型静态常量
pub struct CacheLayerMediaTypeV5;

impl CacheLayerMediaTypeV5 {
    /// 单架构分片索引根目录元数据清单
    pub const ROOT_INDEX_V5_ZSTD: &'static str = "application/vnd.nix.cache.root.v5+zstd";
    /// 全局布隆过滤器数据层
    pub const BLOOM_FILTER_V5_ZSTD: &'static str = "application/vnd.nix.cache.bloom.v5+zstd";
    /// 单个分片数据内容层
    pub const SHARD_DATA_V5_ZSTD: &'static str = "application/vnd.nix.cache.shard.v5+zstd";
    /// 增量 Delta Patch 补丁层
    pub const DELTA_PATCH_V5_ZSTD: &'static str = "application/vnd.nix.cache.delta.v5+zstd";
}

/// 强类型 OCI NixCache Layer 媒体类型 (Schema v5)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheLayerMediaType {
    RootIndexV5Zstd,
    BloomFilterV5Zstd,
    ShardDataV5Zstd,
    DeltaPatchV5Zstd,
}

impl CacheLayerMediaType {
    pub const ROOT_INDEX_V5_ZSTD: &'static str = CacheLayerMediaTypeV5::ROOT_INDEX_V5_ZSTD;
    pub const BLOOM_FILTER_V5_ZSTD: &'static str = CacheLayerMediaTypeV5::BLOOM_FILTER_V5_ZSTD;
    pub const SHARD_DATA_V5_ZSTD: &'static str = CacheLayerMediaTypeV5::SHARD_DATA_V5_ZSTD;
    pub const DELTA_PATCH_V5_ZSTD: &'static str = CacheLayerMediaTypeV5::DELTA_PATCH_V5_ZSTD;

    /// 从媒体类型字符串严格解析
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            Self::ROOT_INDEX_V5_ZSTD => Some(Self::RootIndexV5Zstd),
            Self::BLOOM_FILTER_V5_ZSTD => Some(Self::BloomFilterV5Zstd),
            Self::SHARD_DATA_V5_ZSTD => Some(Self::ShardDataV5Zstd),
            Self::DELTA_PATCH_V5_ZSTD => Some(Self::DeltaPatchV5Zstd),
            _ => None,
        }
    }

    /// 转换为静态媒体类型字符串
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::RootIndexV5Zstd => Self::ROOT_INDEX_V5_ZSTD,
            Self::BloomFilterV5Zstd => Self::BLOOM_FILTER_V5_ZSTD,
            Self::ShardDataV5Zstd => Self::SHARD_DATA_V5_ZSTD,
            Self::DeltaPatchV5Zstd => Self::DELTA_PATCH_V5_ZSTD,
        }
    }

    /// 是否为分片根目录索引类型
    pub const fn is_root_index(&self) -> bool {
        matches!(self, Self::RootIndexV5Zstd)
    }

    /// 是否为布隆过滤器类型
    pub const fn is_bloom_filter(&self) -> bool {
        matches!(self, Self::BloomFilterV5Zstd)
    }

    /// 是否为分片数据类型
    pub const fn is_shard_data(&self) -> bool {
        matches!(self, Self::ShardDataV5Zstd)
    }

    /// 是否为增量补丁类型
    pub const fn is_delta_patch(&self) -> bool {
        matches!(self, Self::DeltaPatchV5Zstd)
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

/// 单架构 Schema v5 Baseline Root Index Image Manifest 构建参数
#[derive(Debug, Clone)]
pub struct ShardedArchIndexManifestParams<'a> {
    pub root_blob_digest: &'a str,
    pub root_blob_size: u64,
    pub bloom_blob_digest: &'a str,
    pub bloom_blob_size: u64,
    pub config_digest: &'a str,
    pub config_size: u64,
    pub system: &'a SystemArch,
    pub merkle_root: &'a str,
}

/// 构造强类型的单架构 Schema v5 Baseline Root Index Image Manifest (Root Directory + Bloom Filter)
pub fn build_sharded_arch_index_manifest(
    params: ShardedArchIndexManifestParams<'_>,
) -> OciImageManifest {
    let mut root_layer_annotations = HashMap::new();
    root_layer_annotations.insert(
        "org.nixos.nixcache.system".to_string(),
        params.system.to_string(),
    );
    root_layer_annotations.insert(
        "org.nixos.nixcache.merkle_root".to_string(),
        params.merkle_root.to_string(),
    );
    root_layer_annotations.insert("org.nixos.nixcache.schema".to_string(), "5".to_string());

    let mut bloom_layer_annotations = HashMap::new();
    bloom_layer_annotations.insert(
        "org.nixos.nixcache.type".to_string(),
        "bloom_filter".to_string(),
    );
    bloom_layer_annotations.insert("org.nixos.nixcache.schema".to_string(), "5".to_string());

    let mut manifest_annotations = HashMap::new();
    manifest_annotations.insert(
        "org.opencontainers.image.created".to_string(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    manifest_annotations.insert(
        "org.opencontainers.image.description".to_string(),
        format!(
            "NixCache Sharded Merkle Baseline ({})",
            params.system.as_str()
        ),
    );
    manifest_annotations.insert(
        "org.nixos.nixcache.system".to_string(),
        params.system.to_string(),
    );
    manifest_annotations.insert(
        "org.nixos.nixcache.merkle_root".to_string(),
        params.merkle_root.to_string(),
    );
    manifest_annotations.insert("org.nixos.nixcache.schema".to_string(), "5".to_string());

    let layers = vec![
        OciDescriptor {
            media_type: CacheLayerMediaTypeV5::ROOT_INDEX_V5_ZSTD.to_string(),
            digest: params.root_blob_digest.to_string(),
            size: params.root_blob_size,
            platform: Some(OciPlatform::from_system(params.system)),
            annotations: Some(root_layer_annotations),
        },
        OciDescriptor {
            media_type: CacheLayerMediaTypeV5::BLOOM_FILTER_V5_ZSTD.to_string(),
            digest: params.bloom_blob_digest.to_string(),
            size: params.bloom_blob_size,
            platform: Some(OciPlatform::from_system(params.system)),
            annotations: Some(bloom_layer_annotations),
        },
    ];

    OciImageManifest {
        schema_version: 2,
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        config: OciDescriptor {
            media_type: OCI_IMAGE_CONFIG_MEDIA_TYPE.to_string(),
            digest: params.config_digest.to_string(),
            size: params.config_size,
            platform: None,
            annotations: None,
        },
        layers,
        annotations: Some(manifest_annotations),
    }
}

/// 构造强类型的单架构 Delta Patch Image Manifest
pub fn build_delta_patch_manifest(
    delta_blob_digest: &str,
    delta_blob_size: u64,
    config_digest: &str,
    config_size: u64,
    run_id: u64,
    job_id: &str,
    system: &SystemArch,
) -> OciImageManifest {
    let mut layer_annotations = HashMap::new();
    layer_annotations.insert("org.nixos.nixcache.run_id".to_string(), run_id.to_string());
    layer_annotations.insert("org.nixos.nixcache.job_id".to_string(), job_id.to_string());
    layer_annotations.insert("org.nixos.nixcache.system".to_string(), system.to_string());
    layer_annotations.insert("org.nixos.nixcache.schema".to_string(), "5".to_string());

    let mut manifest_annotations = HashMap::new();
    manifest_annotations.insert(
        "org.opencontainers.image.created".to_string(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    manifest_annotations.insert(
        "org.opencontainers.image.description".to_string(),
        format!("NixCache Delta Patch ({})", system.as_str()),
    );
    manifest_annotations.insert("org.nixos.nixcache.system".to_string(), system.to_string());
    manifest_annotations.insert("org.nixos.nixcache.run_id".to_string(), run_id.to_string());
    manifest_annotations.insert("org.nixos.nixcache.schema".to_string(), "5".to_string());

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
            media_type: CacheLayerMediaTypeV5::DELTA_PATCH_V5_ZSTD.to_string(),
            digest: delta_blob_digest.to_string(),
            size: delta_blob_size,
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
