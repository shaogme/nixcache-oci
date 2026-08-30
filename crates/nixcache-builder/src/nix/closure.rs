use crate::{
    error::BuilderError,
    nix::{driver::NixCli, filter::NixPathInfoItem},
    session::init::FastStoreScanner,
};
use clap::ValueEnum;
use nixcache_core::StoreHash;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fmt, path::Path, str::FromStr};
use tracing::warn;

#[derive(Serialize, Deserialize, ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureMode {
    /// 精准仅捕获目标及其运行时闭包 (彻底剔除编译期工具与中间构建产物)
    #[default]
    #[value(name = "runtime-closure")]
    RuntimeClosure,
    /// 捕获完整构建依赖闭包 (包含 nativeBuildInputs 等编译依赖图)
    #[value(name = "build-closure")]
    BuildClosure,
    /// 仅捕获根节点自身
    #[value(name = "roots-only")]
    RootsOnly,
    /// 旧版全量 Inode 盲 Diff 模式 (已弃用)
    #[value(name = "diff-all")]
    DiffAll,
}

impl FromStr for CaptureMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().replace('_', "-").as_str() {
            "runtime-closure" | "runtime" => Ok(CaptureMode::RuntimeClosure),
            "build-closure" | "build" => Ok(CaptureMode::BuildClosure),
            "roots-only" | "roots" => Ok(CaptureMode::RootsOnly),
            "diff-all" | "diff" | "all" => Ok(CaptureMode::DiffAll),
            other => Err(format!("Invalid capture mode: {}", other)),
        }
    }
}

impl fmt::Display for CaptureMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureMode::RuntimeClosure => write!(f, "runtime-closure"),
            CaptureMode::BuildClosure => write!(f, "build-closure"),
            CaptureMode::RootsOnly => write!(f, "roots-only"),
            CaptureMode::DiffAll => write!(f, "diff-all"),
        }
    }
}

/// 强类型闭包计算结果集 (包含完整元数据，下游零二次查询)
#[derive(Debug, Clone, Default)]
pub struct ClosureCandidateResult {
    /// 候选产物的完整强类型元数据 (直接复用，避免二次 path-info)
    pub items: Vec<NixPathInfoItem>,
    /// 提纯后的顶层 Active GC Roots (严格仅包含目标根节点)
    pub active_gc_roots: Vec<StoreHash>,
}

/// 基于 Nix DAG 依赖图论的精准闭包引擎
pub struct ClosureEngine;

impl ClosureEngine {
    /// 提取真正的 Active GC Roots (严格仅来自顶层目标根节点)
    pub fn extract_gc_roots(target_roots: &[String]) -> Vec<StoreHash> {
        let mut roots: Vec<StoreHash> = target_roots
            .iter()
            .filter_map(|p| Path::new(p).file_name().and_then(|n| n.to_str())?.get(..32))
            .filter_map(|h| StoreHash::parse(h).ok())
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// 纯函数：根据构建前快照集合过滤掉已存在的项目
    pub fn filter_snapshot_diff(
        items: Vec<NixPathInfoItem>,
        snapshot_before_set: &HashSet<String>,
    ) -> Vec<NixPathInfoItem> {
        items
            .into_iter()
            .filter(|item| {
                let file_name = Path::new(&item.path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&item.path);
                // 仅保留未在构建前快照中出现的路径
                !snapshot_before_set.contains(file_name)
                    && !snapshot_before_set.contains(&item.path)
            })
            .collect()
    }

    /// 一次性完成依赖闭包递归解析、快照差集过滤与 GC Roots 提纯
    pub async fn compute_candidate_closure(
        target_roots: &[String],
        snapshot_before_set: Option<&HashSet<String>>,
        mode: CaptureMode,
        strict_closure: bool,
    ) -> Result<ClosureCandidateResult, BuilderError> {
        if target_roots.is_empty() {
            if strict_closure && mode != CaptureMode::DiffAll {
                return Err(BuilderError::NixCli(
                    "No valid target outputs or result symlinks found to capture. \
                     Please ensure 'nix build' generated a result symlink, or explicitly specify 'targets' or 'out-link' in action inputs."
                        .to_string(),
                ));
            }

            warn!("No target roots identified; falling back to unconstrained diff-all mode.");
            if let Some(snap_set) = snapshot_before_set {
                let current_names = FastStoreScanner::scan_store_names(Path::new("/nix/store"))
                    .await
                    .unwrap_or_default();
                let diff_paths: Vec<String> = current_names
                    .difference(snap_set)
                    .map(|name| format!("/nix/store/{}", name))
                    .collect();
                let raw_items = NixCli.get_path_infos_typed(&diff_paths).await?;
                let active_gc_roots = Self::extract_gc_roots(&diff_paths);
                return Ok(ClosureCandidateResult {
                    items: raw_items,
                    active_gc_roots,
                });
            } else {
                return Ok(ClosureCandidateResult::default());
            }
        }

        // 1. 提纯 Active GC Roots (严格来自 target_roots 顶级产物)
        let active_gc_roots = Self::extract_gc_roots(target_roots);

        // 2. 单次查询获取强类型完整元数据
        let raw_items = match mode {
            CaptureMode::RootsOnly => {
                // 仅查询顶层根节点自身的 path-info (单批次非递归)
                NixCli.get_path_infos_typed(target_roots).await?
            }
            CaptureMode::RuntimeClosure => {
                // 仅在此处调用 1 次 nix path-info --recursive --json
                NixCli.get_recursive_path_infos(target_roots).await?
            }
            CaptureMode::BuildClosure => {
                let build_paths = NixCli.get_build_closure(target_roots).await?;
                NixCli.get_path_infos_typed(&build_paths).await?
            }
            CaptureMode::DiffAll => {
                if let Some(snap_set) = snapshot_before_set {
                    let current_names = FastStoreScanner::scan_store_names(Path::new("/nix/store"))
                        .await
                        .unwrap_or_default();
                    let diff_paths: Vec<String> = current_names
                        .difference(snap_set)
                        .map(|name| format!("/nix/store/{}", name))
                        .collect();
                    NixCli.get_path_infos_typed(&diff_paths).await?
                } else {
                    NixCli.get_recursive_path_infos(target_roots).await?
                }
            }
        };

        // 3. 基于数学等价定理执行内存差集过滤: C(R) ∖ U_snapshot
        let filtered_items = if let Some(snap_set) = snapshot_before_set {
            Self::filter_snapshot_diff(raw_items, snap_set)
        } else {
            raw_items
        };

        Ok(ClosureCandidateResult {
            items: filtered_items,
            active_gc_roots,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_mode_parsing_and_display() {
        assert_eq!(
            <CaptureMode as FromStr>::from_str("runtime-closure").unwrap(),
            CaptureMode::RuntimeClosure
        );
        assert_eq!(
            <CaptureMode as FromStr>::from_str("runtime_closure").unwrap(),
            CaptureMode::RuntimeClosure
        );
        assert_eq!(
            <CaptureMode as FromStr>::from_str("runtime").unwrap(),
            CaptureMode::RuntimeClosure
        );
        assert_eq!(
            <CaptureMode as FromStr>::from_str("build-closure").unwrap(),
            CaptureMode::BuildClosure
        );
        assert_eq!(
            <CaptureMode as FromStr>::from_str("roots-only").unwrap(),
            CaptureMode::RootsOnly
        );
        assert_eq!(
            <CaptureMode as FromStr>::from_str("diff-all").unwrap(),
            CaptureMode::DiffAll
        );
        assert!(<CaptureMode as FromStr>::from_str("unknown-mode").is_err());

        assert_eq!(
            format!("{}", CaptureMode::RuntimeClosure),
            "runtime-closure"
        );
        assert_eq!(format!("{}", CaptureMode::BuildClosure), "build-closure");
        assert_eq!(format!("{}", CaptureMode::RootsOnly), "roots-only");
        assert_eq!(format!("{}", CaptureMode::DiffAll), "diff-all");
    }

    #[test]
    fn test_extract_gc_roots_strictly_top_level() {
        let targets = vec![
            "/nix/store/11111111111111111111111111111111-my-app-1.0".to_string(),
            "/nix/store/22222222222222222222222222222222-my-service-2.0".to_string(),
        ];
        let roots = ClosureEngine::extract_gc_roots(&targets);
        assert_eq!(roots.len(), 2);
        assert_eq!(
            roots[0],
            StoreHash::new_unchecked("11111111111111111111111111111111")
        );
        assert_eq!(
            roots[1],
            StoreHash::new_unchecked("22222222222222222222222222222222")
        );
    }

    #[test]
    fn test_filter_snapshot_diff() {
        let items = vec![
            NixPathInfoItem {
                path: "/nix/store/11111111111111111111111111111111-app".to_string(),
                nar_hash: "sha256:1111".to_string(),
                ..Default::default()
            },
            NixPathInfoItem {
                path: "/nix/store/22222222222222222222222222222222-runtime-lib".to_string(),
                nar_hash: "sha256:2222".to_string(),
                ..Default::default()
            },
            NixPathInfoItem {
                path: "/nix/store/33333333333333333333333333333333-glibc-upstream".to_string(),
                nar_hash: "sha256:3333".to_string(),
                ..Default::default()
            },
        ];

        // 模拟快照中已经存在 glibc-upstream (测试 basename 形式和全路径形式)
        let mut snapshot_set = HashSet::new();
        snapshot_set.insert("33333333333333333333333333333333-glibc-upstream".to_string());

        let filtered = ClosureEngine::filter_snapshot_diff(items, &snapshot_set);
        assert_eq!(filtered.len(), 2);
        assert_eq!(
            filtered[0].path,
            "/nix/store/11111111111111111111111111111111-app"
        );
        assert_eq!(
            filtered[1].path,
            "/nix/store/22222222222222222222222222222222-runtime-lib"
        );
    }

    #[test]
    fn test_filter_snapshot_diff_high_volume() {
        // 构造包含 10,000 个伪路径的快照文件集合
        let mut snapshot_set = HashSet::with_capacity(10_000);
        for i in 0..10_000 {
            snapshot_set.insert(format!("{:032x}-old-pkg-{}", i, i));
        }

        let mut items = Vec::with_capacity(100);
        for i in 0..50 {
            items.push(NixPathInfoItem {
                path: format!("/nix/store/{:032x}-old-pkg-{}", i, i),
                ..Default::default()
            });
        }
        for i in 10_000..10_050 {
            items.push(NixPathInfoItem {
                path: format!("/nix/store/{:032x}-new-pkg-{}", i, i),
                ..Default::default()
            });
        }

        let start = std::time::Instant::now();
        let filtered = ClosureEngine::filter_snapshot_diff(items, &snapshot_set);
        let elapsed = start.elapsed();

        assert_eq!(filtered.len(), 50);
        assert!(
            elapsed.as_millis() < 50,
            "Snapshot diff should take < 50ms, took {:?}",
            elapsed
        );
        for item in &filtered {
            assert!(item.path.contains("new-pkg"));
        }
    }
}
