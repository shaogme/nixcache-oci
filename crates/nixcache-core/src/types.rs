use crate::{
    error::TypeError,
    sharding::{
        EMPTY_SHARD_MERKLE_HASH, calculate_shard_id, compute_merkle_root,
        compute_shard_merkle_hash, partition_entries_by_shard, shard_id_to_prefix,
    },
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    borrow::Borrow, collections::HashMap, convert::Infallible, env, fmt, ops::Deref, path::Path,
    str::FromStr,
};
use strum::{EnumIter, IntoEnumIterator, VariantArray};

pub const SCHEMA_VERSION: u32 = 5;
pub const SCHEMA_VERSION_V5: u32 = 5;
pub const CACHE_INDEX_VERSION: u32 = 5;
pub const RUN_SESSION_VERSION: u32 = 5;
pub const RECEIPT_VERSION: u32 = 5;
pub const NUM_SHARDS: usize = 1024;

/// Nix 32 字符 Base32 散列值 (例如: `s66mzxpvicwk07gjbjfw9izjfa797vsw`)
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoreHash(String);

impl StoreHash {
    pub fn parse(s: &str) -> Result<Self, TypeError> {
        let trimmed = s.trim();
        if trimmed.len() != 32 {
            return Err(TypeError::StoreHashInvalidLength {
                actual: trimmed.len(),
            });
        }
        for (index, c) in trimmed.chars().enumerate() {
            if !matches!(c, '0'..='9' | 'a'..='d' | 'f'..='n' | 'p'..='s' | 'v'..='z') {
                return Err(TypeError::StoreHashInvalidChar { char: c, index });
            }
        }
        Ok(Self(trimmed.to_string()))
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

    pub fn shard_id(&self) -> u16 {
        calculate_shard_id(self)
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
        let Some((algo, hex)) = trimmed.split_once(':') else {
            return Err(TypeError::NarDigestMissingPrefix { raw: s.to_string() });
        };
        if hex.len() != 64 {
            return Err(TypeError::NarDigestInvalidHexLength { actual: hex.len() });
        }
        for (index, c) in hex.chars().enumerate() {
            if !c.is_ascii_hexdigit() {
                return Err(TypeError::NarDigestInvalidHexChar { char: c, index });
            }
        }
        let _ = algo;
        Ok(Self(trimmed.to_string()))
    }

    pub fn new_sha256(hex: &str) -> Result<Self, TypeError> {
        let trimmed = hex.trim();
        if trimmed.len() != 64 {
            return Err(TypeError::NarDigestInvalidHexLength {
                actual: trimmed.len(),
            });
        }
        for (index, c) in trimmed.chars().enumerate() {
            if !c.is_ascii_hexdigit() {
                return Err(TypeError::NarDigestInvalidHexChar { char: c, index });
            }
        }
        Ok(Self(format!("sha256:{}", trimmed)))
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
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default, EnumIter, VariantArray,
)]
pub enum SystemArch {
    #[default]
    X86_64Linux,
    Aarch64Linux,
    X86_64Darwin,
    Aarch64Darwin,
    I686Linux,
    Armv7lLinux,
    Armv6lLinux,
    Riscv64Linux,
    Aarch64Freebsd,
    X86_64Freebsd,
    I686Freebsd,
    X86_64Netbsd,
    X86_64Openbsd,
    Mips64elLinux,
    Powerpc64leLinux,
    S390xLinux,
    Wasm32Wasi,
    Unknown,
}

impl SystemArch {
    /// 所有支持的系统架构静态变体列表
    pub const VARIANTS: &'static [Self] = <Self as VariantArray>::VARIANTS;

    /// 返回所有标准系统架构迭代器 (排除 Unknown)
    pub fn all() -> impl Iterator<Item = Self> {
        <Self as IntoEnumIterator>::iter().filter(|s| *s != Self::Unknown)
    }

    /// 是否为已知支持的架构
    pub const fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown)
    }

    /// 获取 Nix 标准架构字符串
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::X86_64Linux => "x86_64-linux",
            Self::Aarch64Linux => "aarch64-linux",
            Self::X86_64Darwin => "x86_64-darwin",
            Self::Aarch64Darwin => "aarch64-darwin",
            Self::I686Linux => "i686-linux",
            Self::Armv7lLinux => "armv7l-linux",
            Self::Armv6lLinux => "armv6l-linux",
            Self::Riscv64Linux => "riscv64-linux",
            Self::Aarch64Freebsd => "aarch64-freebsd",
            Self::X86_64Freebsd => "x86_64-freebsd",
            Self::I686Freebsd => "i686-freebsd",
            Self::X86_64Netbsd => "x86_64-netbsd",
            Self::X86_64Openbsd => "x86_64-openbsd",
            Self::Mips64elLinux => "mips64el-linux",
            Self::Powerpc64leLinux => "powerpc64le-linux",
            Self::S390xLinux => "s390x-linux",
            Self::Wasm32Wasi => "wasm32-wasi",
            Self::Unknown => "unknown",
        }
    }

    /// 转换为 OCI Platform 标准元组 (os, architecture, optional variant)
    pub const fn to_oci_platform_tuple(
        &self,
    ) -> (&'static str, &'static str, Option<&'static str>) {
        match self {
            Self::X86_64Linux => ("linux", "amd64", None),
            Self::Aarch64Linux => ("linux", "arm64", None),
            Self::X86_64Darwin => ("darwin", "amd64", None),
            Self::Aarch64Darwin => ("darwin", "arm64", None),
            Self::I686Linux => ("linux", "386", None),
            Self::Armv7lLinux => ("linux", "arm", Some("v7")),
            Self::Armv6lLinux => ("linux", "arm", Some("v6")),
            Self::Riscv64Linux => ("linux", "riscv64", None),
            Self::Aarch64Freebsd => ("freebsd", "arm64", None),
            Self::X86_64Freebsd => ("freebsd", "amd64", None),
            Self::I686Freebsd => ("freebsd", "386", None),
            Self::X86_64Netbsd => ("netbsd", "amd64", None),
            Self::X86_64Openbsd => ("openbsd", "amd64", None),
            Self::Mips64elLinux => ("linux", "mips64le", None),
            Self::Powerpc64leLinux => ("linux", "ppc64le", None),
            Self::S390xLinux => ("linux", "s390x", None),
            Self::Wasm32Wasi => ("wasip1", "wasm", None),
            Self::Unknown => ("unknown", "unknown", None),
        }
    }

    /// 从 OCI Platform 属性构建 SystemArch
    pub fn from_oci(os: &str, architecture: &str, variant: Option<&str>) -> Self {
        let os = os.trim().to_ascii_lowercase();
        let arch = architecture.trim().to_ascii_lowercase();
        let variant = variant.map(|v| v.trim().to_ascii_lowercase());

        match (os.as_str(), arch.as_str(), variant.as_deref()) {
            ("linux", "amd64" | "x86_64", _) => Self::X86_64Linux,
            ("linux", "arm64" | "aarch64", _) => Self::Aarch64Linux,
            ("darwin", "amd64" | "x86_64", _) => Self::X86_64Darwin,
            ("darwin", "arm64" | "aarch64", _) => Self::Aarch64Darwin,
            ("linux", "386" | "i686" | "i386", _) => Self::I686Linux,
            ("linux", "arm", Some("v7") | Some("7")) | ("linux", "armv7l", _) => Self::Armv7lLinux,
            ("linux", "arm", Some("v6") | Some("6")) | ("linux", "armv6l", _) => Self::Armv6lLinux,
            ("linux", "riscv64", _) => Self::Riscv64Linux,
            ("freebsd", "arm64" | "aarch64", _) => Self::Aarch64Freebsd,
            ("freebsd", "amd64" | "x86_64", _) => Self::X86_64Freebsd,
            ("freebsd", "386" | "i686" | "i386", _) => Self::I686Freebsd,
            ("netbsd", "amd64" | "x86_64", _) => Self::X86_64Netbsd,
            ("openbsd", "amd64" | "x86_64", _) => Self::X86_64Openbsd,
            ("linux", "mips64le" | "mips64el", _) => Self::Mips64elLinux,
            ("linux", "ppc64le" | "powerpc64le", _) => Self::Powerpc64leLinux,
            ("linux", "s390x", _) => Self::S390xLinux,
            ("wasi" | "wasip1", "wasm" | "wasm32", _) => Self::Wasm32Wasi,
            _ => Self::Unknown,
        }
    }

    /// 探测当前运行环境的系统架构 (基于运行时 OS/ARCH，零子进程开销)
    pub fn detect_current() -> Self {
        let os = env::consts::OS;
        let arch = env::consts::ARCH;
        let detected = Self::from_oci(os, arch, None);
        if detected.is_known() {
            detected
        } else {
            Self::Unknown
        }
    }

    /// 严格解析系统架构字符串，若未知则返回 TypeError::UnknownSystemArch
    pub fn parse_strict(s: &str) -> Result<Self, TypeError> {
        let arch = Self::from(s);
        if arch.is_known() {
            Ok(arch)
        } else {
            Err(TypeError::UnknownSystemArch { raw: s.to_string() })
        }
    }
}

impl fmt::Display for SystemArch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for SystemArch {
    type Err = Infallible;

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
            "armv6l-linux" => Self::Armv6lLinux,
            "riscv64-linux" => Self::Riscv64Linux,
            "aarch64-freebsd" => Self::Aarch64Freebsd,
            "x86_64-freebsd" => Self::X86_64Freebsd,
            "i686-freebsd" => Self::I686Freebsd,
            "x86_64-netbsd" => Self::X86_64Netbsd,
            "x86_64-openbsd" => Self::X86_64Openbsd,
            "mips64el-linux" => Self::Mips64elLinux,
            "powerpc64le-linux" => Self::Powerpc64leLinux,
            "s390x-linux" => Self::S390xLinux,
            "wasm32-wasi" => Self::Wasm32Wasi,
            _ => Self::Unknown,
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
    pub compression: Option<String>,
    pub file_hash: Option<String>,
    pub file_size: Option<u64>,
    pub nar_hash: String,
    pub references: Vec<String>,
    pub deriver: Option<String>,
    pub signatures: Vec<String>,
    pub ca: Option<String>,
}

impl NarInfoMeta {
    /// 从 store_path 中提取 32 字符 Nix 散列值
    pub fn store_hash(&self) -> Option<StoreHash> {
        let name = Path::new(&self.store_path)
            .file_name()
            .and_then(|n| n.to_str())?;
        if name.len() >= 32 {
            StoreHash::parse(&name[..32]).ok()
        } else {
            None
        }
    }

    /// 提取引用中的有效 StoreHash 迭代器
    pub fn reference_hashes(&self) -> impl Iterator<Item = StoreHash> + '_ {
        self.references.iter().filter_map(|r| {
            let candidate = if let Some(pos) = r.rfind('/') {
                &r[pos + 1..]
            } else {
                r.as_str()
            };
            if candidate.len() >= 32 {
                Some(
                    StoreHash::parse(&candidate[..32])
                        .unwrap_or_else(|_| StoreHash::new_unchecked(&candidate[..32])),
                )
            } else {
                None
            }
        })
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
            lines.push(format!("References: {}", self.references.join(" ")));
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
    pub system: Option<SystemArch>,
    /// 强类型结构化 NarInfo 元数据
    pub narinfo_meta: NarInfoMeta,
    pub nar_digest: NarDigest,
    pub nar_size: u64,
    pub added: String,
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

/// 单个分片描述符 (Merkle Tree 叶子节点)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ShardDescriptor {
    /// 分片前缀编号 (0..1023)
    pub shard_id: u16,
    /// 对应的 2 字符 Nix Base32 前缀 (如 "0a", "s6")
    pub prefix: String,
    /// 该分片的数据 Blob OCI 内容寻址散列
    pub blob_digest: String,
    /// 压缩后大小 (Bytes)
    pub compressed_size: u64,
    /// 解压后大小 (Bytes)
    pub uncompressed_size: u64,
    /// 该分片包含的条目总数
    pub entry_count: usize,
    /// 该分片条目的 Merkle 散列校验值
    pub merkle_hash: String,
}

impl ShardDescriptor {
    /// 创建一个空的初始分片描述符
    pub fn empty(shard_id: u16) -> Self {
        Self {
            shard_id,
            prefix: shard_id_to_prefix(shard_id),
            blob_digest: String::new(),
            compressed_size: 0,
            uncompressed_size: 0,
            entry_count: 0,
            merkle_hash: EMPTY_SHARD_MERKLE_HASH.to_string(),
        }
    }

    pub fn new(
        shard_id: u16,
        blob_digest: impl Into<String>,
        compressed_size: u64,
        uncompressed_size: u64,
        entry_count: usize,
        merkle_hash: impl Into<String>,
    ) -> Self {
        Self {
            shard_id,
            prefix: shard_id_to_prefix(shard_id),
            blob_digest: blob_digest.into(),
            compressed_size,
            uncompressed_size,
            entry_count,
            merkle_hash: merkle_hash.into(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }
}

/// 全局紧凑布隆过滤器元数据容器
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BloomFilterManifest {
    pub num_entries: usize,
    pub num_bits: u64,
    pub num_hashes: u8,
    pub blob_digest: String,
    pub compressed_size: u64,
}

impl BloomFilterManifest {
    pub fn empty() -> Self {
        Self {
            num_entries: 0,
            num_bits: 512,
            num_hashes: 7,
            blob_digest: String::new(),
            compressed_size: 0,
        }
    }

    pub fn new(
        num_entries: usize,
        num_bits: u64,
        num_hashes: u8,
        blob_digest: impl Into<String>,
        compressed_size: u64,
    ) -> Self {
        Self {
            num_entries,
            num_bits,
            num_hashes,
            blob_digest: blob_digest.into(),
            compressed_size,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.num_entries == 0
    }
}

/// 单架构全局分片索引根目录 (Schema v5 Root)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ShardedArchCacheIndexData {
    pub version: u32,
    pub system: SystemArch,
    pub repo: String,
    pub registry: String,
    pub generated: String,
    pub public_key: String,
    /// 1024 个分片描述符列表
    pub shards: Vec<ShardDescriptor>,
    /// 全局 Merkle Root 签名
    pub merkle_root: String,
    /// 布隆过滤器描述符
    pub bloom_filter: BloomFilterManifest,
    /// 跨分片聚合的活跃 GC Roots 列表
    pub gc_roots: Vec<StoreHash>,
    pub last_promoted_run: Option<u64>,
}

impl ShardedArchCacheIndexData {
    /// 创建一个全新的 Schema v5 单架构分片索引根目录 (包含 1024 个空分片描述符)
    pub fn new(system: SystemArch, repo: impl Into<String>, registry: impl Into<String>) -> Self {
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        for id in 0..NUM_SHARDS {
            shards.push(ShardDescriptor::empty(id as u16));
        }
        let merkle_root = compute_merkle_root(&shards);

        Self {
            version: SCHEMA_VERSION_V5,
            system,
            repo: repo.into(),
            registry: registry.into(),
            generated: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            public_key: String::new(),
            shards,
            merkle_root,
            bloom_filter: BloomFilterManifest::empty(),
            gc_roots: Vec::new(),
            last_promoted_run: None,
        }
    }

    /// 根据 StoreHash 快速定位其所属分片的描述符
    pub fn find_shard(&self, hash: &StoreHash) -> Option<&ShardDescriptor> {
        let shard_id = calculate_shard_id(hash);
        self.shards.get(shard_id as usize)
    }

    /// 根据分片 ID (0..1023) 获取描述符
    pub fn find_shard_by_id(&self, shard_id: u16) -> Option<&ShardDescriptor> {
        self.shards.get(shard_id as usize)
    }

    /// 根据 StoreHash 快速定位其所属分片的可变描述符
    pub fn find_shard_mut(&mut self, hash: &StoreHash) -> Option<&mut ShardDescriptor> {
        let shard_id = calculate_shard_id(hash);
        self.shards.get_mut(shard_id as usize)
    }

    /// 根据分片 ID (0..1023) 获取可变描述符
    pub fn find_shard_by_id_mut(&mut self, shard_id: u16) -> Option<&mut ShardDescriptor> {
        self.shards.get_mut(shard_id as usize)
    }

    /// 获取所有分片的条目总数
    pub fn total_entries(&self) -> usize {
        self.shards.iter().map(|s| s.entry_count).sum()
    }

    /// 重新计算并更新全局 Merkle Root Hash
    pub fn recalculate_merkle_root(&mut self) {
        self.merkle_root = compute_merkle_root(&self.shards);
    }
}

/// 单个分片内部的实际数据 Payload (独立 Zstd 压缩存储)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ShardDataPayload {
    pub version: u32,
    pub shard_id: u16,
    pub prefix: String,
    pub entries: HashMap<StoreHash, IndexEntry>,
}

impl ShardDataPayload {
    pub fn new(shard_id: u16) -> Self {
        Self {
            version: SCHEMA_VERSION_V5,
            shard_id,
            prefix: shard_id_to_prefix(shard_id),
            entries: HashMap::new(),
        }
    }

    pub fn with_entries(shard_id: u16, entries: HashMap<StoreHash, IndexEntry>) -> Self {
        Self {
            version: SCHEMA_VERSION_V5,
            shard_id,
            prefix: shard_id_to_prefix(shard_id),
            entries,
        }
    }

    pub fn compute_merkle_hash(&self) -> String {
        compute_shard_merkle_hash(&self.entries)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// 增量 Patch 数据结构 (CI 构建节点产物，零写放大)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DeltaPatchData {
    pub version: u32,
    pub run_id: u64,
    pub job_id: String,
    pub system: SystemArch,
    pub timestamp: String,
    pub new_entries: HashMap<StoreHash, IndexEntry>,
    pub active_gc_roots: Vec<StoreHash>,
}

impl DeltaPatchData {
    pub fn new(run_id: u64, job_id: impl Into<String>, system: SystemArch) -> Self {
        Self {
            version: SCHEMA_VERSION_V5,
            run_id,
            job_id: job_id.into(),
            system,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            new_entries: HashMap::new(),
            active_gc_roots: Vec::new(),
        }
    }

    pub fn with_entries_and_roots(
        run_id: u64,
        job_id: impl Into<String>,
        system: SystemArch,
        new_entries: HashMap<StoreHash, IndexEntry>,
        active_gc_roots: Vec<StoreHash>,
    ) -> Self {
        Self {
            version: SCHEMA_VERSION_V5,
            run_id,
            job_id: job_id.into(),
            system,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            new_entries,
            active_gc_roots,
        }
    }

    /// 将新增条目按 1024 个分片进行分组分桶
    pub fn partition_by_shard(&self) -> HashMap<u16, HashMap<StoreHash, IndexEntry>> {
        partition_entries_by_shard(self.new_entries.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.new_entries.is_empty() && self.active_gc_roots.is_empty()
    }
}

/// 单个构建节点的统计数据
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub discovered_outputs: usize,
    pub built_paths: usize,
    pub substituted_paths: usize,
    pub uploaded_blobs: usize,
    pub total_bytes_uploaded: u64,
}

/// 节点构建回执 (BuildReceipt)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BuildReceipt {
    pub version: u32,
    pub system: SystemArch,
    pub repo: String,
    pub run_id: Option<u64>,
    pub job_id: Option<String>,
    pub timestamp: String,
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
