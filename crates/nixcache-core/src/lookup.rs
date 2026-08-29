use crate::types::IndexEntry;
use std::{collections::HashMap, path::Path};

/// 从 store 路径字符串中提取 32 字符 Nix 散列值
pub fn extract_store_hash(store_path: &str) -> Option<&str> {
    let name = Path::new(store_path).file_name().and_then(|n| n.to_str())?;
    if name.len() >= 32 {
        Some(&name[..32])
    } else {
        None
    }
}

/// 从 URL 或 narinfo 行提取 NAR 文件名 (如 "hot.nar.xz")
pub fn extract_nar_basename(url_or_path: &str) -> &str {
    let trimmed = url_or_path.trim();
    if let Some(rest) = trimmed.strip_prefix("URL: nar/") {
        rest.split_whitespace().next().unwrap_or(rest)
    } else if let Some(rest) = trimmed.strip_prefix("URL: ") {
        let path = rest.split_whitespace().next().unwrap_or(rest);
        path.rsplit('/').next().unwrap_or(path)
    } else if let Some(rest) = trimmed.strip_prefix("nar/") {
        rest.split_whitespace().next().unwrap_or(rest)
    } else if let Some(pos) = trimmed.rfind('/') {
        &trimmed[pos + 1..]
    } else {
        trimmed
    }
}

/// 从 IndexEntry 集合中构建高效的 NAR 文件名到 Blob Digest 的反向查找表
pub fn build_nar_lookup_map(entries: &HashMap<String, IndexEntry>) -> HashMap<String, String> {
    let mut map = HashMap::with_capacity(entries.len());
    for entry in entries.values() {
        for line in entry.narinfo.lines() {
            if line.starts_with("URL: ") {
                let nar_name = extract_nar_basename(line);
                if !nar_name.is_empty() {
                    map.insert(nar_name.to_string(), entry.nar_digest.clone());
                }
                break;
            }
        }
    }
    map
}
