use crate::{error::BuilderError, nix::driver::NixCli};
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

    /// 纯函数：计算闭包集合与差集路径的交集 (若差集为空则保留全量闭包)
    pub fn compute_intersection(
        closure_set: &HashSet<String>,
        diff_paths: &[String],
    ) -> Vec<String> {
        let mut result: Vec<String> = if diff_paths.is_empty() {
            closure_set.iter().cloned().collect()
        } else {
            let diff_set: HashSet<&str> = diff_paths.iter().map(|s| s.as_str()).collect();
            closure_set
                .iter()
                .filter(|p| diff_set.contains(p.as_str()))
                .cloned()
                .collect()
        };
        result.sort();
        result
    }

    /// 计算精准候选路径集与根节点集合
    pub async fn compute_candidate_paths(
        target_roots: &[String],
        diff_paths: &[String],
        mode: CaptureMode,
        strict_closure: bool,
    ) -> Result<(Vec<String>, Vec<StoreHash>), BuilderError> {
        if target_roots.is_empty() {
            if strict_closure && mode != CaptureMode::DiffAll {
                return Err(BuilderError::NixCli(
                    "No valid target outputs or result symlinks found to capture. \
                     Please ensure 'nix build' generated a result symlink, or explicitly specify 'targets' or 'out-link' in action inputs."
                        .to_string(),
                ));
            }

            warn!("No target roots identified; falling back to unconstrained diff-all mode.");
            let roots = Self::extract_gc_roots(diff_paths);
            return Ok((diff_paths.to_vec(), roots));
        }

        // 1. 提取真正的 Active GC Roots (严格仅来自 target_roots)
        let active_gc_roots = Self::extract_gc_roots(target_roots);

        // 2. 根据捕获模式计算候选路径集合
        let candidate_paths = match mode {
            CaptureMode::RootsOnly => target_roots.to_vec(),
            CaptureMode::RuntimeClosure => {
                let closure_items = NixCli.get_recursive_path_infos(target_roots).await?;
                let closure_set: HashSet<String> =
                    closure_items.into_iter().map(|i| i.path).collect();
                Self::compute_intersection(&closure_set, diff_paths)
            }
            CaptureMode::BuildClosure => {
                let build_closure = NixCli.get_build_closure(target_roots).await?;
                let build_set: HashSet<String> = build_closure.into_iter().collect();
                Self::compute_intersection(&build_set, diff_paths)
            }
            CaptureMode::DiffAll => diff_paths.to_vec(),
        };

        Ok((candidate_paths, active_gc_roots))
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
    fn test_compute_intersection_with_diff() {
        let mut closure = HashSet::new();
        closure.insert("/nix/store/11111111111111111111111111111111-app".to_string());
        closure.insert("/nix/store/22222222222222222222222222222222-runtime-lib".to_string());
        closure.insert("/nix/store/33333333333333333333333333333333-glibc-upstream".to_string());

        // 模拟 Diff：仅本地编译了 app、runtime-lib 以及中间编译期工具 custom-compiler
        let diff = vec![
            "/nix/store/11111111111111111111111111111111-app".to_string(),
            "/nix/store/22222222222222222222222222222222-runtime-lib".to_string(),
            "/nix/store/44444444444444444444444444444444-custom-compiler".to_string(),
        ];

        let result = ClosureEngine::compute_intersection(&closure, &diff);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"/nix/store/11111111111111111111111111111111-app".to_string()));
        assert!(
            result.contains(&"/nix/store/22222222222222222222222222222222-runtime-lib".to_string())
        );
        // 关键断言：编译期工具 custom-compiler 被 100% 过滤剔除
        assert!(
            !result.contains(
                &"/nix/store/44444444444444444444444444444444-custom-compiler".to_string()
            )
        );
    }

    #[test]
    fn test_compute_intersection_empty_diff_returns_full_closure() {
        let mut closure = HashSet::new();
        closure.insert("/nix/store/11111111111111111111111111111111-app".to_string());
        closure.insert("/nix/store/22222222222222222222222222222222-runtime-lib".to_string());

        let result = ClosureEngine::compute_intersection(&closure, &[]);
        assert_eq!(result.len(), 2);
    }
}
