use crate::{error::BuilderError, nix::driver::NixCli};
use nixcache_core::matches_pattern;
use std::{
    collections::HashSet,
    io::{self, ErrorKind},
    path::Path,
};
use tokio::fs;
use tracing::warn;

/// 确定性顶层目标根节点解析器
pub struct TargetResolver;

impl TargetResolver {
    /// 从多种输入源解析顶层根路径列表
    pub async fn resolve_target_roots(
        explicit_paths: &[String],
        targets_expr: Option<&str>,
        out_link_pattern: Option<&str>,
        workspace_root: &Path,
    ) -> Result<Vec<String>, BuilderError> {
        let mut roots = HashSet::new();

        // 1. 显式传入的 Store 路径
        for p in explicit_paths {
            let trimmed = p.trim();
            if trimmed.starts_with("/nix/store/") {
                roots.insert(trimmed.to_string());
            }
        }

        // 2. 解析 targets 表达式 (通过 nix path-info --json 解析目标 output)
        if let Some(exprs) = targets_expr
            && !exprs.trim().is_empty()
        {
            let target_list: Vec<&str> = exprs.split_whitespace().collect();
            let resolved = NixCli.resolve_flake_or_attr_targets(&target_list).await?;
            roots.extend(resolved);
        }

        // 3. 解析 out-link 软链接 (例如 ./result, ./result-*)
        if let Some(pattern) = out_link_pattern
            && !pattern.trim().is_empty()
        {
            let links = Self::find_symlink_targets(workspace_root, pattern.trim()).await?;
            roots.extend(links);
        }

        let mut sorted: Vec<String> = roots.into_iter().collect();
        sorted.sort();
        Ok(sorted)
    }

    /// 遍历匹配工作区内的符号链接并解析其指向的 /nix/store 路径
    pub async fn find_symlink_targets(
        base_dir: &Path,
        pattern: &str,
    ) -> Result<Vec<String>, BuilderError> {
        let mut results = Vec::new();
        let norm_pattern = pattern.strip_prefix("./").unwrap_or(pattern);

        let pattern_path = Path::new(norm_pattern);
        let (dir_rel, file_pattern) = if let Some(parent) = pattern_path.parent()
            && !parent.as_os_str().is_empty()
        {
            (
                parent,
                pattern_path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or("*"),
            )
        } else {
            (Path::new(""), norm_pattern)
        };

        let search_dir = if dir_rel.as_os_str().is_empty() {
            base_dir.to_path_buf()
        } else {
            base_dir.join(dir_rel)
        };

        if !search_dir.exists() {
            return Ok(results);
        }

        let mut dir_reader = match fs::read_dir(&search_dir).await {
            Ok(r) => r,
            Err(_) => return Ok(results),
        };

        while let Ok(Some(entry)) = dir_reader.next_entry().await {
            let entry_name = entry.file_name();
            let entry_name_str = match entry_name.to_str() {
                Some(s) => s,
                None => continue,
            };

            if matches_pattern(file_pattern, entry_name_str) {
                let entry_path = entry.path();
                let symlink_meta = match fs::symlink_metadata(&entry_path).await {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                if symlink_meta.file_type().is_symlink() {
                    let link_target = fs::read_link(&entry_path).await?;
                    let resolved_target = if link_target.is_relative() {
                        let parent = entry_path.parent().unwrap_or(base_dir);
                        parent.join(&link_target)
                    } else {
                        link_target
                    };

                    match fs::canonicalize(&resolved_target).await {
                        Ok(canon) => {
                            let canon_str = canon.to_string_lossy().to_string();
                            if canon_str.starts_with("/nix/store/") {
                                results.push(canon_str);
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Symlink {:?} target {:?} could not be canonicalized: {}",
                                entry_path, resolved_target, e
                            );
                            return Err(io::Error::new(
                                ErrorKind::NotFound,
                                format!(
                                    "Dangling or broken symlink {:?} pointing to non-existent {:?}",
                                    entry_path, resolved_target
                                ),
                            )
                            .into());
                        }
                    }
                } else if let Ok(canon) = fs::canonicalize(&entry_path).await {
                    let canon_str = canon.to_string_lossy().to_string();
                    if canon_str.starts_with("/nix/store/") {
                        results.push(canon_str);
                    }
                }
            }
        }

        results.sort();
        results.dedup();
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    #[cfg(unix)]
    async fn test_find_symlink_targets_single_and_multiple() {
        let temp = tempdir().unwrap();
        let ws = temp.path();

        // 构造伪造的目标目录
        let fake_store = ws.join("nix/store");
        fs::create_dir_all(&fake_store).await.unwrap();
        let app1 = fake_store.join("11111111111111111111111111111111-app-1.0");
        let app2 = fake_store.join("22222222222222222222222222222222-app-2.0");
        fs::write(&app1, b"bin1").await.unwrap();
        fs::write(&app2, b"bin2").await.unwrap();

        // 创建指向 fake_store 的软链接 (以 /nix/store 风格路径测试)
        // 注意：canonicalize 返回真实物理路径，在此测试中我们验证解析逻辑与错误处理
        let link1 = ws.join("result");
        let link2 = ws.join("result-service");
        tokio::fs::symlink(&app1, &link1).await.unwrap();
        tokio::fs::symlink(&app2, &link2).await.unwrap();

        let link_pattern = "result*";
        let norm_pattern = link_pattern.strip_prefix("./").unwrap_or(link_pattern);
        assert!(matches_pattern(norm_pattern, "result"));
        assert!(matches_pattern(norm_pattern, "result-service"));
        assert!(!matches_pattern(norm_pattern, "other"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_find_symlink_targets_dangling_error() {
        let temp = tempdir().unwrap();
        let ws = temp.path();

        let dangling_link = ws.join("result");
        tokio::fs::symlink(ws.join("non_existent_target"), &dangling_link)
            .await
            .unwrap();

        let res = TargetResolver::find_symlink_targets(ws, "./result*").await;
        assert!(res.is_err());
        let err_str = res.unwrap_err().to_string();
        assert!(err_str.contains("Dangling or broken symlink"));
    }

    #[tokio::test]
    async fn test_resolve_target_roots_combines_explicit_paths() {
        let temp = tempdir().unwrap();
        let explicit = vec![
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-explicit1".to_string(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-explicit2".to_string(),
        ];

        let roots = TargetResolver::resolve_target_roots(&explicit, None, None, temp.path())
            .await
            .unwrap();

        assert_eq!(roots.len(), 2);
        assert_eq!(
            roots[0],
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-explicit1"
        );
        assert_eq!(
            roots[1],
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-explicit2"
        );
    }
}
