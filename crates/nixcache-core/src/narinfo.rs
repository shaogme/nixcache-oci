use crate::error::NarInfoParseError;
use std::path::Path;

/// 强类型 NARInfo 描述结构体
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct NarInfo {
    pub store_path: String,
    pub url: String,
    pub compression: Option<String>,
    pub file_hash: Option<String>,
    pub file_size: Option<u64>,
    pub nar_hash: String,
    pub nar_size: u64,
    pub references: Vec<String>,
    pub deriver: Option<String>,
    pub signatures: Vec<String>,
    pub ca: Option<String>,
}

impl NarInfo {
    /// 解析标准 Nix .narinfo 文本格式
    pub fn parse(content: &str) -> Result<Self, NarInfoParseError> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(NarInfoParseError::EmptyContent);
        }

        let mut store_path = None;
        let mut url = None;
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
                    "URL" => url = Some(value.to_string()),
                    "Compression" => {
                        if !value.is_empty() && value != "none" {
                            compression = Some(value.to_string());
                        }
                    }
                    "FileHash" => file_hash = Some(value.to_string()),
                    "FileSize" => {
                        let parsed =
                            value
                                .parse::<u64>()
                                .map_err(|_| NarInfoParseError::InvalidNumber {
                                    field: "FileSize",
                                    value: value.to_string(),
                                })?;
                        file_size = Some(parsed);
                    }
                    "NarHash" => nar_hash = Some(value.to_string()),
                    "NarSize" => {
                        let parsed =
                            value
                                .parse::<u64>()
                                .map_err(|_| NarInfoParseError::InvalidNumber {
                                    field: "NarSize",
                                    value: value.to_string(),
                                })?;
                        nar_size = Some(parsed);
                    }
                    "References" => {
                        references = value
                            .split_whitespace()
                            .map(|s| s.to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
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
        let url = url.ok_or(NarInfoParseError::MissingRequiredField("URL"))?;
        let nar_hash = nar_hash.ok_or(NarInfoParseError::MissingRequiredField("NarHash"))?;
        let nar_size = nar_size.ok_or(NarInfoParseError::MissingRequiredField("NarSize"))?;

        Ok(Self {
            store_path,
            url,
            compression,
            file_hash,
            file_size,
            nar_hash,
            nar_size,
            references,
            deriver,
            signatures,
            ca,
        })
    }

    /// 序列化为标准 Nix .narinfo 文本表示
    pub fn to_narinfo_string(&self) -> String {
        let mut lines = Vec::with_capacity(12);
        lines.push(format!("StorePath: {}", self.store_path));
        lines.push(format!("URL: {}", self.url));

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
        lines.push(format!("NarSize: {}", self.nar_size));

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

    /// 提取 NAR 文件的基本名称 (如 "12345.nar.xz")
    pub fn nar_basename(&self) -> &str {
        if let Some(rest) = self.url.strip_prefix("nar/") {
            rest.split_whitespace().next().unwrap_or(rest)
        } else if let Some(pos) = self.url.rfind('/') {
            &self.url[pos + 1..]
        } else {
            &self.url
        }
    }

    /// 提取 Store Path 中的 32 字符 Nix 散列值
    pub fn store_hash(&self) -> Option<&str> {
        let name = Path::new(&self.store_path)
            .file_name()
            .and_then(|n| n.to_str())?;
        if name.len() >= 32 {
            Some(&name[..32])
        } else {
            None
        }
    }
}
