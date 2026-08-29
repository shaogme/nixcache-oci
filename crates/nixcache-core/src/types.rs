use crate::error::TypeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{borrow::Borrow, collections::HashMap, fmt, ops::Deref, str::FromStr};

pub const SCHEMA_VERSION: u32 = 3;
pub const CACHE_INDEX_VERSION: u32 = 3;
pub const RUN_SESSION_VERSION: u32 = 3;
pub const RECEIPT_VERSION: u32 = 3;

/// Nix 32 字符 Base32 散列值 (例如: `s66mzxpvicwk07gjbjfw9izjfa797vsw`)
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoreHash(String);

impl StoreHash {
    pub fn parse(s: &str) -> Result<Self, TypeError> {
        let trimmed = s.trim();
        if trimmed.len() == 32
            && trimmed
                .chars()
                .all(|c| matches!(c, '0'..='9' | 'a'..='d' | 'f'..='n' | 'p'..='s' | 'v'..='z'))
        {
            Ok(Self(trimmed.to_string()))
        } else {
            Err(TypeError::InvalidStoreHash(s.to_string()))
        }
    }

    /// 不做合法性校验直接构造 StoreHash (仅限受信任的内部或测试场景)
    pub fn new_unchecked(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl Deref for StoreHash {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for StoreHash {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for StoreHash {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StoreHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for StoreHash {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for StoreHash {
    type Error = TypeError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl TryFrom<String> for StoreHash {
    type Error = TypeError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl Serialize for StoreHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for StoreHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl Default for StoreHash {
    fn default() -> Self {
        Self("00000000000000000000000000000000".to_string())
    }
}

/// OCI 内容寻址散列值 (例如: `sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0`)
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NarDigest(String);

impl Default for NarDigest {
    fn default() -> Self {
        Self("sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string())
    }
}

impl NarDigest {
    pub fn parse(s: &str) -> Result<Self, TypeError> {
        let trimmed = s.trim();
        if let Some(hex) = trimmed.strip_prefix("sha256:") {
            if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(Self(trimmed.to_string()));
            }
        } else if let Some((algo, hex)) = trimmed.split_once(':')
            && !algo.is_empty()
            && !hex.is_empty()
            && algo.chars().all(|c| c.is_ascii_alphanumeric())
            && hex.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Ok(Self(trimmed.to_string()));
        }
        Err(TypeError::InvalidNarDigest(s.to_string()))
    }

    pub fn new_sha256(hex: &str) -> Result<Self, TypeError> {
        let trimmed = hex.trim();
        if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            Ok(Self(format!("sha256:{}", trimmed)))
        } else {
            Err(TypeError::InvalidNarDigest(hex.to_string()))
        }
    }

    /// 不做合法性校验直接构造 NarDigest (仅限受信任的内部或测试场景)
    pub fn new_unchecked(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl Deref for NarDigest {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for NarDigest {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for NarDigest {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NarDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for NarDigest {
    type Err = TypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for NarDigest {
    type Error = TypeError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl TryFrom<String> for NarDigest {
    type Error = TypeError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl Serialize for NarDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NarDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// 系统架构强类型
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum SystemArch {
    #[default]
    X86_64Linux,
    Aarch64Linux,
    X86_64Darwin,
    Aarch64Darwin,
    I686Linux,
    Armv7lLinux,
    Riscv64Linux,
    Other(String),
}

impl SystemArch {
    pub fn as_str(&self) -> &str {
        match self {
            Self::X86_64Linux => "x86_64-linux",
            Self::Aarch64Linux => "aarch64-linux",
            Self::X86_64Darwin => "x86_64-darwin",
            Self::Aarch64Darwin => "aarch64-darwin",
            Self::I686Linux => "i686-linux",
            Self::Armv7lLinux => "armv7l-linux",
            Self::Riscv64Linux => "riscv64-linux",
            Self::Other(s) => s.as_str(),
        }
    }
}

impl fmt::Display for SystemArch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for SystemArch {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(s))
    }
}

impl From<&str> for SystemArch {
    fn from(s: &str) -> Self {
        match s.trim() {
            "x86_64-linux" => Self::X86_64Linux,
            "aarch64-linux" => Self::Aarch64Linux,
            "x86_64-darwin" => Self::X86_64Darwin,
            "aarch64-darwin" => Self::Aarch64Darwin,
            "i686-linux" => Self::I686Linux,
            "armv7l-linux" => Self::Armv7lLinux,
            "riscv64-linux" => Self::Riscv64Linux,
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<String> for SystemArch {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl Serialize for SystemArch {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SystemArch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from(s))
    }
}

/// 强类型结构化 NarInfo 元数据 (去除冗余文本与重复解析)
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct NarInfoMeta {
    pub store_path: String,
    pub nar_basename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    pub nar_hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<StoreHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deriver: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
}

impl NarInfoMeta {
    /// 从 store_path 中提取 32 字符 Nix 散列值
    pub fn store_hash(&self) -> Option<StoreHash> {
        let name = std::path::Path::new(&self.store_path)
            .file_name()
            .and_then(|n| n.to_str())?;
        if name.len() >= 32 {
            StoreHash::parse(&name[..32]).ok()
        } else {
            None
        }
    }

    /// 渲染为标准 Nix .narinfo 文本
    pub fn render(&self, nar_size: u64) -> String {
        let mut lines = Vec::with_capacity(12);
        lines.push(format!("StorePath: {}", self.store_path));
        lines.push(format!("URL: nar/{}", self.nar_basename));

        if let Some(ref comp) = self.compression {
            lines.push(format!("Compression: {}", comp));
        }
        if let Some(ref fh) = self.file_hash {
            lines.push(format!("FileHash: {}", fh));
        }
        if let Some(fs) = self.file_size {
            lines.push(format!("FileSize: {}", fs));
        }

        lines.push(format!("NarHash: {}", self.nar_hash));
        lines.push(format!("NarSize: {}", nar_size));

        if !self.references.is_empty() {
            let refs: Vec<&str> = self.references.iter().map(|r| r.as_str()).collect();
            lines.push(format!("References: {}", refs.join(" ")));
        }
        if let Some(ref drv) = self.deriver {
            lines.push(format!("Deriver: {}", drv));
        }
        for sig in &self.signatures {
            lines.push(format!("Sig: {}", sig));
        }
        if let Some(ref ca) = self.ca {
            lines.push(format!("CA: {}", ca));
        }

        lines.join("\n") + "\n"
    }
}

/// 强类型 IndexEntry，定义单个 Nix Store 产物及其 NAR 存储元数据
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemArch>,
    /// 强类型结构化 NarInfo 元数据
    pub narinfo_meta: NarInfoMeta,
    pub nar_digest: NarDigest,
    pub nar_size: u64,
    pub added: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_job: Option<String>,
}

impl IndexEntry {
    /// 零开销获取 NAR 基础文件名
    pub fn nar_basename(&self) -> &str {
        &self.narinfo_meta.nar_basename
    }

    /// 按需渲染出标准 Nix .narinfo 文本
    pub fn to_narinfo_string(&self) -> String {
        self.narinfo_meta.render(self.nar_size)
    }

    /// 获取关联的 StoreHash
    pub fn store_hash(&self) -> Option<StoreHash> {
        self.narinfo_meta.store_hash()
    }
}

/// 构建任务执行摘要元数据
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct JobSummaryMetadata {
    pub job_id: String,
    pub system: SystemArch,
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
    pub entries: HashMap<StoreHash, IndexEntry>,
    #[serde(default)]
    pub gc_roots: HashMap<SystemArch, Vec<StoreHash>>,
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
    pub entries: HashMap<StoreHash, IndexEntry>,
    #[serde(default)]
    pub gc_roots: HashMap<SystemArch, Vec<StoreHash>>,
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
    pub system: SystemArch,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    pub new_entries: HashMap<StoreHash, IndexEntry>,
    pub active_gc_roots: Vec<StoreHash>,
    pub stats: BuildStats,
}

impl BuildReceipt {
    pub fn new(
        system: SystemArch,
        repo: String,
        timestamp: String,
        public_key: Option<String>,
        new_entries: HashMap<StoreHash, IndexEntry>,
        active_gc_roots: Vec<StoreHash>,
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
