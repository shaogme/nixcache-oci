use chrono::Utc;
use nixcache_core::SystemArch;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_IMAGE_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
pub const NIX_CACHE_INDEX_MEDIA_TYPE: &str = "application/vnd.nix.cache.index.v1+json";
pub const NIX_CACHE_SESSION_MEDIA_TYPE: &str = "application/vnd.nix.cache.session.v1+json";

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

/// 构造强类型的单架构 Session Image Manifest
pub fn build_arch_session_manifest(
    session_blob_digest: &str,
    session_blob_size: u64,
    config_digest: &str,
    config_size: u64,
    run_id: u64,
    system: &SystemArch,
) -> OciImageManifest {
    let mut layer_annotations = HashMap::new();
    layer_annotations.insert("org.nixos.nixcache.run_id".to_string(), run_id.to_string());
    layer_annotations.insert("org.nixos.nixcache.system".to_string(), system.to_string());
    layer_annotations.insert("org.nixos.nixcache.schema".to_string(), "4".to_string());

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
            media_type: NIX_CACHE_SESSION_MEDIA_TYPE.to_string(),
            digest: session_blob_digest.to_string(),
            size: session_blob_size,
            platform: Some(OciPlatform::from_system(system)),
            annotations: Some(layer_annotations),
        }],
        annotations: Some(manifest_annotations),
    }
}

/// 构造强类型的单架构 Baseline Index Image Manifest
pub fn build_arch_index_manifest(
    index_blob_digest: &str,
    index_blob_size: u64,
    config_digest: &str,
    config_size: u64,
    system: &SystemArch,
) -> OciImageManifest {
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
            media_type: NIX_CACHE_INDEX_MEDIA_TYPE.to_string(),
            digest: index_blob_digest.to_string(),
            size: index_blob_size,
            platform: Some(OciPlatform::from_system(system)),
            annotations: None,
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

/// 兼容构建函数：构造全局 Session Manifest
pub fn build_session_manifest(
    session_blob_digest: &str,
    session_blob_size: u64,
    config_digest: &str,
    config_size: u64,
    run_id: u64,
) -> OciImageManifest {
    let mut layer_annotations = HashMap::new();
    layer_annotations.insert("org.nixos.nixcache.run_id".to_string(), run_id.to_string());
    layer_annotations.insert("org.nixos.nixcache.schema".to_string(), "4".to_string());

    let mut manifest_annotations = HashMap::new();
    manifest_annotations.insert(
        "org.opencontainers.image.created".to_string(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    manifest_annotations.insert(
        "org.opencontainers.image.description".to_string(),
        "NixCache Workflow Run Session Manifest".to_string(),
    );

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
            media_type: NIX_CACHE_SESSION_MEDIA_TYPE.to_string(),
            digest: session_blob_digest.to_string(),
            size: session_blob_size,
            platform: None,
            annotations: Some(layer_annotations),
        }],
        annotations: Some(manifest_annotations),
    }
}

/// 兼容构建函数：构造全局 Baseline Index Manifest
pub fn build_index_manifest(
    index_blob_digest: &str,
    index_blob_size: u64,
    config_digest: &str,
    config_size: u64,
) -> OciImageManifest {
    let mut manifest_annotations = HashMap::new();
    manifest_annotations.insert(
        "org.opencontainers.image.created".to_string(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    manifest_annotations.insert(
        "org.opencontainers.image.description".to_string(),
        "NixCache Production Global Index Manifest".to_string(),
    );

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
            media_type: NIX_CACHE_INDEX_MEDIA_TYPE.to_string(),
            digest: index_blob_digest.to_string(),
            size: index_blob_size,
            platform: None,
            annotations: None,
        }],
        annotations: Some(manifest_annotations),
    }
}

/// 便捷兼容函数：生成 Session Manifest JSON 字符串
pub fn build_session_oci_manifest(
    session_blob_digest: &str,
    session_blob_size: u64,
    config_digest: &str,
    config_size: u64,
    run_id: u64,
) -> String {
    let manifest = build_session_manifest(
        session_blob_digest,
        session_blob_size,
        config_digest,
        config_size,
        run_id,
    );
    manifest.to_json_string().unwrap_or_default()
}

/// 便捷兼容函数：生成 Index Manifest JSON 字符串
pub fn build_index_oci_manifest(
    index_blob_digest: &str,
    index_blob_size: u64,
    config_digest: &str,
    config_size: u64,
) -> String {
    let manifest = build_index_manifest(
        index_blob_digest,
        index_blob_size,
        config_digest,
        config_size,
    );
    manifest.to_json_string().unwrap_or_default()
}
