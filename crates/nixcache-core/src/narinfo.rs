use crate::{
    error::NarInfoParseError,
    lookup::extract_nar_basename,
    types::{
        NarInfoMeta, StoreHash, normalize_store_reference, validate_nar_basename,
        validate_nix_hash, validate_store_path,
    },
};
use std::collections::HashSet;
use std::ops::{Deref, DerefMut};

/// 强类型 NARInfo 描述结构体
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct NarInfo {
    pub meta: NarInfoMeta,
    pub nar_size: u64,
}

impl Deref for NarInfo {
    type Target = NarInfoMeta;

    fn deref(&self) -> &Self::Target {
        &self.meta
    }
}

impl DerefMut for NarInfo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.meta
    }
}

impl NarInfo {
    /// 解析标准 Nix .narinfo 文本格式
    pub fn parse(content: &str) -> Result<Self, NarInfoParseError> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(NarInfoParseError::EmptyContent);
        }

        let mut store_path = None;
        let mut nar_basename = None;
        let mut compression = None;
        let mut file_hash = None;
        let mut file_size = None;
        let mut nar_hash = None;
        let mut nar_size = None;
        let mut references = Vec::new();
        let mut deriver = None;
        let mut signatures = Vec::new();
        let mut ca = None;
        let mut seen_fields = HashSet::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((key, value)) = line.split_once(':') else {
                return Err(NarInfoParseError::MalformedLine(line.to_string()));
            };
            let key = key.trim();
            let value = value.trim();

            let single_field = match key {
                "StorePath" => Some("StorePath"),
                "URL" => Some("URL"),
                "Compression" => Some("Compression"),
                "FileHash" => Some("FileHash"),
                "FileSize" => Some("FileSize"),
                "NarHash" => Some("NarHash"),
                "NarSize" => Some("NarSize"),
                "References" => Some("References"),
                "Deriver" => Some("Deriver"),
                "CA" => Some("CA"),
                _ => None,
            };
            if let Some(field) = single_field
                && !seen_fields.insert(field)
            {
                return Err(NarInfoParseError::DuplicateField(field));
            }

            match key {
                "StorePath" => {
                    if value.is_empty() {
                        return Err(NarInfoParseError::EmptyField("StorePath"));
                    }
                    validate_store_path(value)
                        .map_err(|_| NarInfoParseError::InvalidStorePath(value.to_string()))?;
                    store_path = Some(value.to_string());
                }
                "URL" => {
                    nar_basename = Some(parse_nar_url(value)?);
                }
                "Compression" => {
                    if value.is_empty() {
                        return Err(NarInfoParseError::EmptyField("Compression"));
                    }
                    if value != "none" {
                        compression = Some(value.to_string());
                    }
                }
                "FileHash" => {
                    if value.is_empty() {
                        return Err(NarInfoParseError::EmptyField("FileHash"));
                    }
                    validate_nix_hash(value).map_err(|source| NarInfoParseError::InvalidHash {
                        field: "FileHash",
                        source,
                    })?;
                    file_hash = Some(value.to_string());
                }
                "FileSize" => {
                    if value.is_empty() {
                        return Err(NarInfoParseError::EmptyField("FileSize"));
                    }
                    let parsed = value.parse::<u64>().map_err(|source| {
                        NarInfoParseError::InvalidNumber {
                            field: "FileSize",
                            source,
                        }
                    })?;
                    if parsed == 0 {
                        return Err(NarInfoParseError::NonPositiveSize("FileSize"));
                    }
                    file_size = Some(parsed);
                }
                "NarHash" => {
                    if value.is_empty() {
                        return Err(NarInfoParseError::EmptyField("NarHash"));
                    }
                    validate_nix_hash(value).map_err(|source| NarInfoParseError::InvalidHash {
                        field: "NarHash",
                        source,
                    })?;
                    nar_hash = Some(value.to_string());
                }
                "NarSize" => {
                    if value.is_empty() {
                        return Err(NarInfoParseError::EmptyField("NarSize"));
                    }
                    let parsed = value.parse::<u64>().map_err(|source| {
                        NarInfoParseError::InvalidNumber {
                            field: "NarSize",
                            source,
                        }
                    })?;
                    if parsed == 0 {
                        return Err(NarInfoParseError::NonPositiveSize("NarSize"));
                    }
                    nar_size = Some(parsed);
                }
                "References" => {
                    for token in value.split_whitespace() {
                        let normalized = normalize_store_reference(token)
                            .map_err(|_| NarInfoParseError::InvalidReference(token.to_string()))?;
                        references.push(normalized);
                    }
                }
                "Deriver" => {
                    if value.is_empty() {
                        return Err(NarInfoParseError::EmptyField("Deriver"));
                    }
                    let normalized = normalize_store_reference(value).map_err(|_| {
                        NarInfoParseError::InvalidField {
                            field: "Deriver",
                            details: "must be a valid StorePath basename or full path".to_string(),
                        }
                    })?;
                    if !normalized.ends_with(".drv") {
                        return Err(NarInfoParseError::InvalidField {
                            field: "Deriver",
                            details: "must end with .drv".to_string(),
                        });
                    }
                    deriver = Some(normalized);
                }
                "Sig" if !value.is_empty() => {
                    signatures.push(value.to_string());
                }
                "CA" => {
                    if value.is_empty() {
                        return Err(NarInfoParseError::EmptyField("CA"));
                    }
                    ca = Some(value.to_string());
                }
                _ => {}
            }
        }

        let store_path = store_path.ok_or(NarInfoParseError::MissingRequiredField("StorePath"))?;
        let nar_basename = nar_basename.ok_or(NarInfoParseError::MissingRequiredField("URL"))?;
        let nar_hash = nar_hash.ok_or(NarInfoParseError::MissingRequiredField("NarHash"))?;
        let nar_size = nar_size.ok_or(NarInfoParseError::MissingRequiredField("NarSize"))?;

        let meta = NarInfoMeta {
            store_path,
            nar_basename,
            compression,
            file_hash,
            file_size,
            nar_hash,
            references,
            deriver,
            signatures,
            ca,
        };

        meta.validate_structure()
            .map_err(|error| NarInfoParseError::InvalidField {
                field: "NarInfo",
                details: error.to_string(),
            })?;

        Ok(Self { meta, nar_size })
    }

    /// 序列化为标准 Nix .narinfo 文本表示
    pub fn to_narinfo_string(&self) -> String {
        self.meta.render(self.nar_size)
    }

    /// 提取 NAR 文件的 URL 路径
    pub fn url(&self) -> String {
        format!("nar/{}", self.meta.nar_basename)
    }

    /// 提取 NAR 文件的基本名称 (如 "12345.nar.xz")
    pub fn nar_basename(&self) -> &str {
        &self.meta.nar_basename
    }

    /// 提取 Store Path 中的 32 字符 Nix 散列值
    pub fn store_hash(&self) -> Option<StoreHash> {
        self.meta.store_hash()
    }

    /// 拆分为元数据与 NAR 大小
    pub fn into_meta(self) -> (NarInfoMeta, u64) {
        (self.meta, self.nar_size)
    }
}

fn parse_nar_url(value: &str) -> Result<String, NarInfoParseError> {
    if value.is_empty() {
        return Err(NarInfoParseError::EmptyField("URL"));
    }
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
        || value.contains('\\')
    {
        return Err(NarInfoParseError::InvalidUrl(value.to_string()));
    }
    if value
        .split('/')
        .any(|component| component == "." || component == "..")
    {
        return Err(NarInfoParseError::InvalidUrl(value.to_string()));
    }
    let basename = extract_nar_basename(value);
    validate_nar_basename(basename)
        .map_err(|_| NarInfoParseError::InvalidUrl(value.to_string()))?;
    Ok(basename.to_string())
}
