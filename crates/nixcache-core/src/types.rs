use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub const SCHEMA_VERSION: u32 = 3;
pub const CACHE_INDEX_VERSION: u32 = 3;
pub const RUN_SESSION_VERSION: u32 = 3;
pub const RECEIPT_VERSION: u32 = 3;

/// 强类型 IndexEntry，定义单个 Nix Store 产物及其 NAR 存储元数据
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub narinfo: String,
    pub nar_digest: String,
    pub nar_size: u64,
    pub added: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_job: Option<String>,
}

/// 构建任务执行摘要元数据
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct JobSummaryMetadata {
    pub job_id: String,
    pub system: String,
    pub uploaded_blobs: usize,
    pub uploaded_bytes: u64,
    pub timestamp: String,
}

/// 生产基线全局索引数据 (Tier 3)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CacheIndexData {
    pub version: u32,
    pub repo: String,
    pub registry: String,
    pub image: String,
    pub generated: String,
    #[serde(default)]
    pub public_key: String,
    pub entries: HashMap<String, IndexEntry>,
    #[serde(default, deserialize_with = "deserialize_gc_roots")]
    pub gc_roots: HashMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_promoted_run: Option<u64>,
}

impl Default for CacheIndexData {
    fn default() -> Self {
        Self {
            version: CACHE_INDEX_VERSION,
            repo: String::new(),
            registry: String::new(),
            image: String::new(),
            generated: String::new(),
            public_key: String::new(),
            entries: HashMap::new(),
            gc_roots: HashMap::new(),
            last_promoted_run: None,
        }
    }
}

/// 工作流会话清单 (Tier 1 / Tier 2)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunSessionManifest {
    pub version: u32,
    pub run_id: u64,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub ref_name: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    pub entries: HashMap<String, IndexEntry>,
    #[serde(default, deserialize_with = "deserialize_gc_roots")]
    pub gc_roots: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub completed_jobs: Vec<JobSummaryMetadata>,
}

impl Default for RunSessionManifest {
    fn default() -> Self {
        Self {
            version: RUN_SESSION_VERSION,
            run_id: 0,
            head_sha: String::new(),
            ref_name: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            public_key: None,
            entries: HashMap::new(),
            gc_roots: HashMap::new(),
            completed_jobs: Vec::new(),
        }
    }
}

/// 单个构建节点的统计数据
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    #[serde(default)]
    pub discovered_outputs: usize,
    #[serde(default)]
    pub built_paths: usize,
    #[serde(default)]
    pub substituted_paths: usize,
    #[serde(default)]
    pub uploaded_blobs: usize,
    #[serde(default)]
    pub total_bytes_uploaded: u64,
}

/// 节点构建回执 (BuildReceipt)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BuildReceipt {
    pub version: u32,
    pub system: String,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    pub new_entries: HashMap<String, IndexEntry>,
    pub active_gc_roots: Vec<String>,
    pub stats: BuildStats,
}

impl BuildReceipt {
    pub fn new(
        system: String,
        repo: String,
        timestamp: String,
        public_key: Option<String>,
        new_entries: HashMap<String, IndexEntry>,
        active_gc_roots: Vec<String>,
        stats: BuildStats,
    ) -> Self {
        Self {
            version: RECEIPT_VERSION,
            system,
            repo,
            run_id: None,
            job_id: None,
            timestamp,
            public_key,
            new_entries,
            active_gc_roots,
            stats,
        }
    }

    pub fn with_run_info(mut self, run_id: Option<u64>, job_id: Option<String>) -> Self {
        self.run_id = run_id;
        self.job_id = job_id;
        self
    }
}

/// 兼容 Schema v1/v2 数组与 Schema v3 多架构字典的 GC Roots 反序列化器
pub fn deserialize_gc_roots<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = Value::deserialize(deserializer)?;
    match val {
        Value::Object(map) => {
            let mut result = HashMap::new();
            for (k, v) in map {
                if let Value::Array(arr) = v {
                    let strings: Vec<String> = arr
                        .into_iter()
                        .filter_map(|item| match item {
                            Value::String(s) => Some(s),
                            _ => None,
                        })
                        .collect();
                    result.insert(k, strings);
                }
            }
            Ok(result)
        }
        Value::Array(arr) => {
            let strings: Vec<String> = arr
                .into_iter()
                .filter_map(|item| match item {
                    Value::String(s) => Some(s),
                    _ => None,
                })
                .collect();
            let mut result = HashMap::new();
            if !strings.is_empty() {
                result.insert("default".to_string(), strings);
            }
            Ok(result)
        }
        _ => Ok(HashMap::new()),
    }
}
