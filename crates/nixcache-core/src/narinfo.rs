use crate::{
    error::NarInfoParseError,
    lookup::extract_nar_basename,
    types::{NarInfoMeta, StoreHash},
};
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

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim();

                match key {
                    "StorePath" => store_path = Some(value.to_string()),
                    "URL" => {
                        let extracted = extract_nar_basename(value);
                        if !extracted.is_empty() {
                            nar_basename = Some(extracted.to_string());
                        }
                    }
                    "Compression" => {
                        if !value.is_empty() && value != "none" {
                            compression = Some(value.to_string());
                        }
                    }
                    "FileHash" => file_hash = Some(value.to_string()),
                    "FileSize" => {
                        let parsed = value.parse::<u64>().map_err(|source| {
                            NarInfoParseError::InvalidNumber {
                                field: "FileSize",
                                source,
                            }
                        })?;
                        file_size = Some(parsed);
                    }
                    "NarHash" => nar_hash = Some(value.to_string()),
                    "NarSize" => {
                        let parsed = value.parse::<u64>().map_err(|source| {
                            NarInfoParseError::InvalidNumber {
                                field: "NarSize",
                                source,
                            }
                        })?;
                        nar_size = Some(parsed);
                    }
                    "References" => {
                        for token in value.split_whitespace() {
                            let item = token.trim();
                            if item.is_empty() {
                                continue;
                            }
                            let bname = if let Some(pos) = item.rfind('/') {
                                &item[pos + 1..]
                            } else {
                                item
                            };
                            references.push(bname.to_string());
                        }
                    }
                    "Deriver" if !value.is_empty() => {
                        deriver = Some(value.to_string());
                    }
                    "Sig" if !value.is_empty() => {
                        signatures.push(value.to_string());
                    }
                    "CA" if !value.is_empty() => {
                        ca = Some(value.to_string());
                    }
                    _ => {}
                }
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
