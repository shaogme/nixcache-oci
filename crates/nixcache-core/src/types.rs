use crate::{
    error::{CoreError, TypeError},
    sharding::{
        calculate_shard_id, compute_merkle_root, compute_shard_merkle_hash, shard_id_to_prefix,
    },
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    borrow::Borrow, collections::HashMap, convert::Infallible, env, fmt, ops::Deref, str::FromStr,
};
use strum::{EnumIter, IntoEnumIterator, VariantArray};

pub const SCHEMA_VERSION_V7: u32 = 7;
pub const CACHE_INDEX_VERSION: u32 = SCHEMA_VERSION_V7;
pub const RUN_SESSION_VERSION: u32 = 6;
pub const RECEIPT_VERSION: u32 = 6;
pub const NUM_SHARDS: usize = 1024;

pub trait IndexValidationLimits {
    fn max_shard_entries(&self) -> u64;
    fn max_gc_roots(&self) -> u64;
    fn max_uncompressed_bytes(&self) -> u64;
    fn max_nar_size(&self) -> u64;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultIndexValidationLimits {
    pub shard_entries: u64,
    pub gc_roots: u64,
    pub uncompressed_bytes: u64,
    pub nar_size: u64,
}

impl Default for DefaultIndexValidationLimits {
    fn default() -> Self {
        Self {
            shard_entries: 500_000,
            gc_roots: 500_000,
            uncompressed_bytes: 64 * 1024 * 1024,
            nar_size: 16 * 1024 * 1024 * 1024,
        }
    }
}

impl IndexValidationLimits for DefaultIndexValidationLimits {
    fn max_shard_entries(&self) -> u64 {
        self.shard_entries
    }

    fn max_gc_roots(&self) -> u64 {
        self.gc_roots
    }

    fn max_uncompressed_bytes(&self) -> u64 {
        self.uncompressed_bytes
    }

    fn max_nar_size(&self) -> u64 {
        self.nar_size
    }
}

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
        if algo != "sha256" {
            return Err(TypeError::NarDigestInvalidAlgorithm {
                algorithm: algo.to_string(),
            });
        }
        if hex.len() != 64 {
            return Err(TypeError::NarDigestInvalidHexLength { actual: hex.len() });
        }
        for (index, c) in hex.chars().enumerate() {
            if !c.is_ascii_hexdigit() {
                return Err(TypeError::NarDigestInvalidHexChar { char: c, index });
            }
        }
        Ok(Self(format!("sha256:{}", hex.to_ascii_lowercase())))
    }

    pub fn new_sha256(hex: &str) -> Result<Self, TypeError> {
        Self::parse(&format!("sha256:{}", hex.trim()))
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

pub(crate) fn validate_store_path(value: &str) -> Result<StoreHash, TypeError> {
    let Some(basename) = value.strip_prefix("/nix/store/") else {
        return Err(TypeError::InvalidStorePathFormat {
            raw: value.to_string(),
        });
    };
    if basename.is_empty() || basename.contains('/') || basename.contains('\\') {
        return Err(TypeError::InvalidStorePathFormat {
            raw: value.to_string(),
        });
    }
    validate_store_path_basename(basename)
}

pub(crate) fn validate_store_path_basename(value: &str) -> Result<StoreHash, TypeError> {
    let Some((hash, name)) = value.split_once('-') else {
        return Err(TypeError::InvalidStorePathFormat {
            raw: value.to_string(),
        });
    };
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(TypeError::InvalidStorePathFormat {
            raw: value.to_string(),
        });
    }
    StoreHash::parse(hash)
}

pub(crate) fn normalize_store_reference(value: &str) -> Result<String, TypeError> {
    let basename = if value.starts_with("/nix/store/") {
        validate_store_path(value)?;
        value
            .strip_prefix("/nix/store/")
            .expect("validated StorePath prefix")
    } else {
        if value.contains('/') || value.contains('\\') {
            return Err(TypeError::InvalidStorePathFormat {
                raw: value.to_string(),
            });
        }
        validate_store_path_basename(value)?;
        value
    };
    Ok(basename.to_string())
}

pub(crate) fn validate_nar_basename(value: &str) -> Result<(), TypeError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains("..")
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(TypeError::InvalidNarBasename {
            raw: value.to_string(),
        });
    }
    Ok(())
}

pub(crate) fn validate_nix_hash(value: &str) -> Result<(), TypeError> {
    let Some((algorithm, separator, encoded)) = value
        .split_once(':')
        .map(|(algorithm, encoded)| (algorithm, ':', encoded))
        .or_else(|| {
            value
                .split_once('-')
                .map(|(algorithm, encoded)| (algorithm, '-', encoded))
        })
    else {
        return Err(TypeError::NixHashInvalidAlgorithm {
            algorithm: String::new(),
        });
    };
    if algorithm != "sha256" {
        return Err(TypeError::NixHashInvalidAlgorithm {
            algorithm: algorithm.to_string(),
        });
    }
    if separator == '-'
        && encoded.len() == 44
        && encoded.as_bytes()[43] == b'='
        && encoded[..43]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
    {
        return Ok(());
    }
    if encoded.len() == 52 {
        for (index, byte) in encoded.bytes().enumerate() {
            if !matches!(
                byte,
                b'0'..=b'9'
                    | b'a'..=b'd'
                    | b'f'..=b'n'
                    | b'p'..=b's'
                    | b'v'..=b'z'
            ) {
                return Err(TypeError::NixHashInvalidChar {
                    char: byte as char,
                    index,
                });
            }
        }
        return Ok(());
    }
    if encoded.len() == 64
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(TypeError::NixHashInvalidLength {
        expected: "52 Nix base32, 64 lowercase hexadecimal, or 44 base64 characters",
        actual: encoded.len(),
    })
}

/// 强类型结构化 NarInfo 元数据 (去除冗余文本与重复解析)
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
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
    pub fn validate_structure(&self) -> Result<(), CoreError> {
        validate_store_path(&self.store_path).map_err(|error| CoreError::InvalidEntry {
            details: format!("StorePath: {error}"),
        })?;
        validate_nar_basename(&self.nar_basename).map_err(|error| CoreError::InvalidEntry {
            details: format!("NAR basename: {error}"),
        })?;
        if let Some(file_hash) = &self.file_hash {
            if file_hash.is_empty() {
                return Err(CoreError::InvalidEntry {
                    details: "FileHash must not be empty".to_string(),
                });
            }
            validate_nix_hash(file_hash).map_err(|error| CoreError::InvalidEntry {
                details: format!("FileHash: {error}"),
            })?;
        }
        if self.compression.as_deref().is_some_and(str::is_empty) {
            return Err(CoreError::InvalidEntry {
                details: "Compression must not be empty".to_string(),
            });
        }
        if self.ca.as_deref().is_some_and(str::is_empty) {
            return Err(CoreError::InvalidEntry {
                details: "CA must not be empty".to_string(),
            });
        }
        if self.file_size == Some(0) {
            return Err(CoreError::InvalidEntry {
                details: "FileSize must be greater than zero".to_string(),
            });
        }
        validate_nix_hash(&self.nar_hash).map_err(|error| CoreError::InvalidEntry {
            details: format!("NarHash: {error}"),
        })?;
        for reference in &self.references {
            normalize_store_reference(reference).map_err(|error| CoreError::InvalidEntry {
                details: format!("reference '{reference}': {error}"),
            })?;
        }
        if let Some(deriver) = &self.deriver {
            let normalized =
                normalize_store_reference(deriver).map_err(|error| CoreError::InvalidEntry {
                    details: format!("Deriver '{deriver}': {error}"),
                })?;
            if !normalized.ends_with(".drv") {
                return Err(CoreError::InvalidEntry {
                    details: format!("Deriver '{deriver}' must end with .drv"),
                });
            }
        }
        Ok(())
    }

    /// 从 store_path 中提取 32 字符 Nix 散列值
    pub fn store_hash(&self) -> Option<StoreHash> {
        validate_store_path(&self.store_path).ok()
    }

    /// 提取引用中的有效 StoreHash；任何损坏引用都会传播为错误。
    pub fn reference_hashes(&self) -> Result<Vec<StoreHash>, TypeError> {
        self.references
            .iter()
            .map(|reference| {
                let normalized = normalize_store_reference(reference)?;
                let (hash, _) = normalized.split_once('-').ok_or_else(|| {
                    TypeError::InvalidStorePathFormat {
                        raw: reference.clone(),
                    }
                })?;
                StoreHash::parse(hash)
            })
            .collect()
    }

    pub fn try_reference_hashes(&self) -> Result<Vec<StoreHash>, TypeError> {
        self.reference_hashes()
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

#[derive(Deserialize)]
struct NarInfoMetaWire {
    store_path: String,
    nar_basename: String,
    compression: Option<String>,
    file_hash: Option<String>,
    file_size: Option<u64>,
    nar_hash: String,
    references: Vec<String>,
    deriver: Option<String>,
    signatures: Vec<String>,
    ca: Option<String>,
}

impl<'de> Deserialize<'de> for NarInfoMeta {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = NarInfoMetaWire::deserialize(deserializer)?;
        let value = Self {
            store_path: wire.store_path,
            nar_basename: wire.nar_basename,
            compression: wire.compression,
            file_hash: wire.file_hash,
            file_size: wire.file_size,
            nar_hash: wire.nar_hash,
            references: wire.references,
            deriver: wire.deriver,
            signatures: wire.signatures,
            ca: wire.ca,
        };
        value
            .validate_structure()
            .map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

/// 强类型 IndexEntry，定义单个 Nix Store 产物及其 NAR 存储元数据
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
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
    pub fn validate_structure(&self) -> Result<(), CoreError> {
        if self.name.is_empty() {
            return Err(CoreError::InvalidEntry {
                details: "entry name must not be empty".to_string(),
            });
        }
        if let Some(system) = self.system
            && !system.is_known()
        {
            return Err(CoreError::InvalidEntry {
                details: "entry system must be known".to_string(),
            });
        }
        self.narinfo_meta.validate_structure()?;
        NarDigest::parse(self.nar_digest.as_str()).map_err(|error| CoreError::InvalidEntry {
            details: format!("NAR digest: {error}"),
        })?;
        if self.nar_size == 0 {
            return Err(CoreError::InvalidEntry {
                details: "NAR size must be greater than zero".to_string(),
            });
        }
        if self.added.trim().is_empty()
            || chrono::DateTime::parse_from_rfc3339(&self.added).is_err()
        {
            return Err(CoreError::InvalidEntry {
                details: "added must be a valid RFC3339 timestamp".to_string(),
            });
        }
        Ok(())
    }

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

#[derive(Deserialize)]
struct IndexEntryWire {
    name: String,
    system: Option<SystemArch>,
    narinfo_meta: NarInfoMeta,
    nar_digest: NarDigest,
    nar_size: u64,
    added: String,
    origin_job: Option<String>,
}

impl<'de> Deserialize<'de> for IndexEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = IndexEntryWire::deserialize(deserializer)?;
        let value = Self {
            name: wire.name,
            system: wire.system,
            narinfo_meta: wire.narinfo_meta,
            nar_digest: wire.nar_digest,
            nar_size: wire.nar_size,
            added: wire.added,
            origin_job: wire.origin_job,
        };
        value
            .validate_structure()
            .map_err(serde::de::Error::custom)?;
        Ok(value)
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

/// 单个分片描述符 (Schema v7 Merkle Tree 叶子节点)
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
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
    pub fn validate_structure(&self) -> Result<(), CoreError> {
        if self.shard_id >= NUM_SHARDS as u16 {
            return Err(CoreError::InvalidIndex {
                details: format!("shard id {} is out of range", self.shard_id),
            });
        }
        let expected_prefix = shard_id_to_prefix(self.shard_id);
        if self.prefix != expected_prefix {
            return Err(CoreError::InvalidIndex {
                details: format!("shard {} has an invalid prefix", self.shard_id),
            });
        }
        if self.is_empty() {
            let expected_merkle_hash = compute_shard_merkle_hash(self.shard_id, &HashMap::new())?;
            if !self.blob_digest.is_empty()
                || self.compressed_size != 0
                || self.uncompressed_size != 0
                || self.merkle_hash != expected_merkle_hash
            {
                return Err(CoreError::InvalidIndex {
                    details: format!("empty shard {} has non-empty metadata", self.shard_id),
                });
            }
        } else {
            validate_sha256(&self.blob_digest).map_err(|details| CoreError::InvalidIndex {
                details: format!("shard {} digest: {details}", self.shard_id),
            })?;
            if self.compressed_size == 0 || self.uncompressed_size == 0 {
                return Err(CoreError::InvalidIndex {
                    details: format!("non-empty shard {} has zero size", self.shard_id),
                });
            }
            if self.entry_count == 0 {
                return Err(CoreError::InvalidIndex {
                    details: format!("non-empty shard {} has zero entries", self.shard_id),
                });
            }
            validate_sha256(&self.merkle_hash).map_err(|details| CoreError::InvalidIndex {
                details: format!("shard {} Merkle hash: {details}", self.shard_id),
            })?;
        }
        Ok(())
    }

    /// 创建一个空的初始分片描述符
    pub fn empty(shard_id: u16) -> Result<Self, CoreError> {
        let merkle_hash = compute_shard_merkle_hash(shard_id, &HashMap::new())?;
        Ok(Self {
            shard_id,
            prefix: shard_id_to_prefix(shard_id),
            blob_digest: String::new(),
            compressed_size: 0,
            uncompressed_size: 0,
            entry_count: 0,
            merkle_hash,
        })
    }

    pub fn new(
        shard_id: u16,
        blob_digest: impl Into<String>,
        compressed_size: u64,
        uncompressed_size: u64,
        entry_count: usize,
        merkle_hash: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let value = Self {
            shard_id,
            prefix: shard_id_to_prefix(shard_id),
            blob_digest: blob_digest.into(),
            compressed_size,
            uncompressed_size,
            entry_count,
            merkle_hash: merkle_hash.into(),
        };
        value.validate_structure()?;
        Ok(value)
    }

    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }
}

#[derive(Deserialize)]
struct ShardDescriptorWire {
    shard_id: u16,
    prefix: String,
    blob_digest: String,
    compressed_size: u64,
    uncompressed_size: u64,
    entry_count: usize,
    merkle_hash: String,
}

impl<'de> Deserialize<'de> for ShardDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ShardDescriptorWire::deserialize(deserializer)?;
        let value = Self {
            shard_id: wire.shard_id,
            prefix: wire.prefix,
            blob_digest: wire.blob_digest,
            compressed_size: wire.compressed_size,
            uncompressed_size: wire.uncompressed_size,
            entry_count: wire.entry_count,
            merkle_hash: wire.merkle_hash,
        };
        value
            .validate_structure()
            .map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

/// 单架构全局分片索引根目录 (Schema v7 Root)
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
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
    /// 跨分片聚合的活跃 GC Roots 列表
    pub gc_roots: Vec<StoreHash>,
    pub last_promoted_run: Option<u64>,
}

impl ShardedArchCacheIndexData {
    /// 创建一个全新的 Schema v7 单架构分片索引根目录 (包含 1024 个空分片描述符)
    pub fn new(system: SystemArch, repo: impl Into<String>, registry: impl Into<String>) -> Self {
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        for id in 0..NUM_SHARDS {
            shards.push(
                ShardDescriptor::empty(id as u16)
                    .expect("the fixed v7 shard range must always be valid"),
            );
        }
        let merkle_root = compute_merkle_root(&shards)
            .expect("a freshly created complete shard set must have a valid Merkle root");

        Self {
            version: SCHEMA_VERSION_V7,
            system,
            repo: repo.into(),
            registry: registry.into(),
            generated: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            public_key: String::new(),
            shards,
            merkle_root,
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
    pub fn recalculate_merkle_root(&mut self) -> Result<(), CoreError> {
        self.merkle_root = compute_merkle_root(&self.shards)?;
        Ok(())
    }

    /// 直接判断 StoreHash 所在的分片是否为空 (O(1) 确定性硬件级零误杀硬过滤)
    pub fn is_shard_empty(&self, hash: &StoreHash) -> bool {
        match self.find_shard(hash) {
            Some(shard) => shard.is_empty(),
            None => true,
        }
    }

    pub fn validate_structure(&self) -> Result<(), CoreError> {
        if self.version != SCHEMA_VERSION_V7 {
            return Err(CoreError::InvalidIndex {
                details: format!("version must be {SCHEMA_VERSION_V7}"),
            });
        }
        if !self.system.is_known() {
            return Err(CoreError::InvalidIndex {
                details: "root system must be known".to_string(),
            });
        }
        if self.repo.trim().is_empty() || self.registry.trim().is_empty() {
            return Err(CoreError::InvalidIndex {
                details: "root repository and registry must not be empty".to_string(),
            });
        }
        if self.generated.trim().is_empty()
            || chrono::DateTime::parse_from_rfc3339(&self.generated).is_err()
        {
            return Err(CoreError::InvalidIndex {
                details: "root generated must be a valid RFC3339 timestamp".to_string(),
            });
        }
        if self.shards.len() != NUM_SHARDS {
            return Err(CoreError::InvalidIndex {
                details: format!("expected exactly {NUM_SHARDS} shard descriptors"),
            });
        }
        for (index, shard) in self.shards.iter().enumerate() {
            let expected_id = index as u16;
            if shard.shard_id != expected_id {
                return Err(CoreError::InvalidIndex {
                    details: format!("shard descriptor at index {index} has wrong id"),
                });
            }
            shard.validate_structure()?;
        }
        validate_sha256(&self.merkle_root).map_err(|details| CoreError::InvalidIndex {
            details: format!("root Merkle hash: {details}"),
        })?;
        if compute_merkle_root(&self.shards)? != self.merkle_root {
            return Err(CoreError::InvalidIndex {
                details: "root Merkle hash does not match shard descriptors".to_string(),
            });
        }
        Ok(())
    }

    pub fn validate_for<L: IndexValidationLimits>(
        &self,
        system: &SystemArch,
        repo: &str,
        registry: &str,
        limits: &L,
    ) -> Result<(), CoreError> {
        self.validate_structure()?;
        if self.system != *system {
            return Err(CoreError::InvalidIndex {
                details: "root system does not match the requested system".to_string(),
            });
        }
        if self.repo != repo || self.registry != registry {
            return Err(CoreError::InvalidIndex {
                details: "root repository or registry does not match the client target".to_string(),
            });
        }
        for shard in &self.shards {
            if !shard.is_empty() {
                if shard.uncompressed_size > limits.max_uncompressed_bytes() {
                    return Err(CoreError::LimitExceeded {
                        target: "shard uncompressed bytes",
                        limit: limits.max_uncompressed_bytes(),
                        actual: shard.uncompressed_size,
                    });
                }
                if shard.entry_count as u64 > limits.max_shard_entries() {
                    return Err(CoreError::LimitExceeded {
                        target: "shard entries",
                        limit: limits.max_shard_entries(),
                        actual: shard.entry_count as u64,
                    });
                }
            }
        }
        if self.gc_roots.len() as u64 > limits.max_gc_roots() {
            return Err(CoreError::LimitExceeded {
                target: "GC roots",
                limit: limits.max_gc_roots(),
                actual: self.gc_roots.len() as u64,
            });
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct ShardedArchCacheIndexDataWire {
    version: u32,
    system: SystemArch,
    repo: String,
    registry: String,
    generated: String,
    public_key: String,
    shards: Vec<ShardDescriptor>,
    merkle_root: String,
    gc_roots: Vec<StoreHash>,
    last_promoted_run: Option<u64>,
}

impl<'de> Deserialize<'de> for ShardedArchCacheIndexData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ShardedArchCacheIndexDataWire::deserialize(deserializer)?;
        let value = Self {
            version: wire.version,
            system: wire.system,
            repo: wire.repo,
            registry: wire.registry,
            generated: wire.generated,
            public_key: wire.public_key,
            shards: wire.shards,
            merkle_root: wire.merkle_root,
            gc_roots: wire.gc_roots,
            last_promoted_run: wire.last_promoted_run,
        };
        value
            .validate_structure()
            .map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

/// 单个分片内部的实际数据 Payload (独立 Zstd 压缩存储)
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ShardDataPayload {
    pub version: u32,
    pub shard_id: u16,
    pub prefix: String,
    pub entries: HashMap<StoreHash, IndexEntry>,
}

impl ShardDataPayload {
    pub fn new(shard_id: u16) -> Result<Self, CoreError> {
        if shard_id >= NUM_SHARDS as u16 {
            return Err(CoreError::InvalidShard {
                details: format!("shard id {shard_id} is out of range"),
            });
        }
        Ok(Self {
            version: SCHEMA_VERSION_V7,
            shard_id,
            prefix: shard_id_to_prefix(shard_id),
            entries: HashMap::new(),
        })
    }

    pub fn with_entries(
        shard_id: u16,
        entries: HashMap<StoreHash, IndexEntry>,
    ) -> Result<Self, CoreError> {
        let mut payload = Self::new(shard_id)?;
        payload.entries = entries;
        Ok(payload)
    }

    pub fn compute_merkle_hash(&self) -> Result<String, CoreError> {
        compute_shard_merkle_hash(self.shard_id, &self.entries)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn validate_structure(&self) -> Result<(), CoreError> {
        if self.version != SCHEMA_VERSION_V7 {
            return Err(CoreError::InvalidShard {
                details: format!("version must be {SCHEMA_VERSION_V7}"),
            });
        }
        if self.shard_id >= NUM_SHARDS as u16 {
            return Err(CoreError::InvalidShard {
                details: format!("shard id {} is out of range", self.shard_id),
            });
        }
        if self.prefix != shard_id_to_prefix(self.shard_id) {
            return Err(CoreError::InvalidShard {
                details: "payload prefix does not match shard id".to_string(),
            });
        }
        for (hash, entry) in &self.entries {
            entry
                .validate_structure()
                .map_err(|error| CoreError::InvalidShard {
                    details: format!("entry {hash}: {error}"),
                })?;
            if hash.shard_id() != self.shard_id {
                return Err(CoreError::InvalidShard {
                    details: format!("entry {hash} belongs to another shard"),
                });
            }
            if entry.system.is_none_or(|system| !system.is_known()) {
                return Err(CoreError::InvalidShard {
                    details: format!("entry {hash} has an unknown or missing system"),
                });
            }
            if entry.store_hash().as_ref() != Some(hash) {
                return Err(CoreError::InvalidShard {
                    details: format!("entry {hash} StorePath does not match map key"),
                });
            }
        }
        Ok(())
    }

    pub fn validate_for<L: IndexValidationLimits>(
        &self,
        shard_id: u16,
        system: &SystemArch,
        limits: &L,
    ) -> Result<(), CoreError> {
        self.validate_structure()?;
        if shard_id >= NUM_SHARDS as u16 || self.shard_id != shard_id {
            return Err(CoreError::InvalidShard {
                details: "payload shard id is invalid or does not match the descriptor".to_string(),
            });
        }
        if self.prefix != shard_id_to_prefix(shard_id) {
            return Err(CoreError::InvalidShard {
                details: "payload prefix does not match shard id".to_string(),
            });
        }
        if self.entries.len() as u64 > limits.max_shard_entries() {
            return Err(CoreError::LimitExceeded {
                target: "shard entries",
                limit: limits.max_shard_entries(),
                actual: self.entries.len() as u64,
            });
        }
        for (hash, entry) in &self.entries {
            if entry.system != Some(*system) {
                return Err(CoreError::InvalidShard {
                    details: format!("entry {hash} has a mismatched or missing system"),
                });
            }
            if entry.nar_size > limits.max_nar_size() {
                return Err(CoreError::LimitExceeded {
                    target: "NAR size",
                    limit: limits.max_nar_size(),
                    actual: entry.nar_size,
                });
            }
        }
        Ok(())
    }

    pub fn validate_merkle_hash(&self, expected: &str) -> Result<(), CoreError> {
        if self.compute_merkle_hash()? != expected {
            return Err(CoreError::InvalidShard {
                details: "payload Merkle hash does not match root descriptor".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct ShardDataPayloadWire {
    version: u32,
    shard_id: u16,
    prefix: String,
    entries: HashMap<StoreHash, IndexEntry>,
}

impl<'de> Deserialize<'de> for ShardDataPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ShardDataPayloadWire::deserialize(deserializer)?;
        let value = Self {
            version: wire.version,
            shard_id: wire.shard_id,
            prefix: wire.prefix,
            entries: wire.entries,
        };
        value
            .validate_structure()
            .map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

fn validate_sha256(value: &str) -> Result<(), String> {
    if is_sha256(value) {
        Ok(())
    } else {
        Err(
            "expected canonical sha256: followed by 64 lowercase hexadecimal characters"
                .to_string(),
        )
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

    /// 执行两个相同架构回执的无损合并
    pub fn merge_with(&mut self, other: BuildReceipt) {
        debug_assert_eq!(
            self.system, other.system,
            "Cannot merge receipts of different architectures"
        );
        self.new_entries.extend(other.new_entries);
        self.active_gc_roots.extend(other.active_gc_roots);
        self.active_gc_roots.sort_unstable();
        self.active_gc_roots.dedup();
        self.stats.discovered_outputs += other.stats.discovered_outputs;
        self.stats.built_paths += other.stats.built_paths;
        self.stats.uploaded_blobs += other.stats.uploaded_blobs;
        self.stats.total_bytes_uploaded += other.stats.total_bytes_uploaded;
        self.stats.substituted_paths += other.stats.substituted_paths;
        if self.public_key.is_none() {
            self.public_key = other.public_key;
        }
    }
}
