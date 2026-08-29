use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_IMAGE_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
pub const NIX_CACHE_INDEX_MEDIA_TYPE: &str = "application/vnd.nix.cache.index.v1+json";
pub const NIX_CACHE_SESSION_MEDIA_TYPE: &str = "application/vnd.nix.cache.session.v1+json";

/// 强类型 OCI 内容描述符 (Content Descriptor)
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OciDescriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// 构造强类型的 Session Image Manifest
pub fn build_session_manifest(
    session_blob_digest: &str,
    session_blob_size: u64,
    config_digest: &str,
    config_size: u64,
    run_id: u64,
) -> OciImageManifest {
    let mut layer_annotations = HashMap::new();
    layer_annotations.insert("org.nixos.nixcache.run_id".to_string(), run_id.to_string());
    layer_annotations.insert("org.nixos.nixcache.schema".to_string(), "3".to_string());

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
            annotations: None,
        },
        layers: vec![OciDescriptor {
            media_type: NIX_CACHE_SESSION_MEDIA_TYPE.to_string(),
            digest: session_blob_digest.to_string(),
            size: session_blob_size,
            annotations: Some(layer_annotations),
        }],
        annotations: Some(manifest_annotations),
    }
}

/// 构造强类型的 Baseline Index Image Manifest
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
            annotations: None,
        },
        layers: vec![OciDescriptor {
            media_type: NIX_CACHE_INDEX_MEDIA_TYPE.to_string(),
            digest: index_blob_digest.to_string(),
            size: index_blob_size,
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
