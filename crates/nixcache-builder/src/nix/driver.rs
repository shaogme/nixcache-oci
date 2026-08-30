use crate::{
    env_injector::NixEnvInjector,
    error::BuilderError,
    nix::filter::{NixArtifactFilter, NixArtifactFilterContext, NixPathInfoItem},
};
use clap::ValueEnum;
use nixcache_core::StoreHash;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    env, fmt,
    path::Path,
    process::Stdio,
    str::FromStr,
};
use tokio::{fs, io::AsyncWriteExt, process::Command};
use tracing::{error, info};

#[derive(Serialize, Deserialize, ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildMode {
    #[value(name = "flake")]
    Flake,
    #[value(name = "non-flake")]
    NonFlake,
}

impl FromStr for BuildMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "flake" => Ok(BuildMode::Flake),
            "non-flake" | "nonflake" => Ok(BuildMode::NonFlake),
            other => Err(format!("Invalid build mode: {}", other)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub system: Option<String>,
    pub mode: BuildMode,
    pub flake_path: String,
    pub file: String,
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildTarget {
    Flake {
        flake_ref: String,
        attribute: String,
    },
    NonFlake {
        file: String,
        attribute: Option<String>,
    },
}

impl fmt::Display for BuildTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildTarget::Flake {
                flake_ref,
                attribute,
            } => {
                write!(f, "{}#{}", flake_ref, attribute)
            }
            BuildTarget::NonFlake { file, attribute } => {
                if let Some(attr) = attribute {
                    write!(f, "{} -A {}", file, attr)
                } else {
                    write!(f, "{}", file)
                }
            }
        }
    }
}

/// 强类型 Nix CLI 驱动器
#[derive(Clone, Debug, Default)]
pub struct NixCli;

impl NixCli {
    pub async fn current_system(&self) -> Result<String, BuilderError> {
        let output = Command::new("nix")
            .args([
                "eval",
                "--raw",
                "--impure",
                "--expr",
                "builtins.currentSystem",
            ])
            .output()
            .await?;

        if !output.status.success() {
            return Err(BuilderError::NixCli(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub async fn build_outputs(
        &self,
        targets: &[BuildTarget],
    ) -> Result<Vec<String>, BuilderError> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_paths = Vec::new();
        let nix_config = env::var("NIX_CONFIG").ok();

        // 1. 处理 Flake 目标 (统一合并为单次 nix build 调用)
        let flake_targets: Vec<&BuildTarget> = targets
            .iter()
            .filter(|t| matches!(t, BuildTarget::Flake { .. }))
            .collect();

        if !flake_targets.is_empty() {
            info!("Batch building {} flake target(s)", flake_targets.len());
            let mut cmd = Command::new("nix");
            cmd.arg("build");
            if let Some(ref config) = nix_config {
                NixEnvInjector::apply_to_command(&mut cmd, config);
            }

            for target in &flake_targets {
                if let BuildTarget::Flake {
                    flake_ref,
                    attribute,
                } = target
                {
                    cmd.arg(format!("{}#{}", flake_ref, attribute));
                }
            }
            cmd.args(["--no-link", "--accept-flake-config", "--json"]);

            let output = cmd.output().await?;
            if output.status.success() {
                let json_str = String::from_utf8_lossy(&output.stdout);
                Self::extract_outputs_from_json(&json_str, &mut all_paths)?;
            } else {
                error!(
                    "Batch nix build for flake targets failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                // Fallback to nix path-info
                let mut path_info_cmd = Command::new("nix");
                path_info_cmd.arg("path-info");
                for target in &flake_targets {
                    if let BuildTarget::Flake {
                        flake_ref,
                        attribute,
                    } = target
                    {
                        path_info_cmd.arg(format!("{}#{}", flake_ref, attribute));
                    }
                }

                let path_info_out = path_info_cmd.output().await?;
                if path_info_out.status.success() {
                    let paths = String::from_utf8_lossy(&path_info_out.stdout);
                    for p in paths.lines() {
                        let trimmed = p.trim();
                        if !trimmed.is_empty() {
                            all_paths.push(trimmed.to_string());
                        }
                    }
                } else {
                    return Err(BuilderError::NixCli(
                        "Failed to build flake targets and fallback path-info failed".to_string(),
                    ));
                }
            }
        }

        // 2. 处理 NonFlake 目标 (按 file 分组批量构建)
        let mut non_flake_groups: HashMap<&str, Vec<Option<&str>>> = HashMap::new();
        for target in targets {
            if let BuildTarget::NonFlake { file, attribute } = target {
                non_flake_groups
                    .entry(file.as_str())
                    .or_default()
                    .push(attribute.as_deref());
            }
        }

        for (file, attrs) in non_flake_groups {
            info!("Batch building non-flake targets for file: {}", file);
            let mut cmd = Command::new("nix");
            cmd.arg("build");
            if let Some(ref config) = nix_config {
                NixEnvInjector::apply_to_command(&mut cmd, config);
            }
            cmd.args(["--file", file]);
            for a in attrs.iter().flatten() {
                cmd.arg(*a);
            }
            cmd.args(["--no-link", "--json"]);

            let output = cmd.output().await?;
            if output.status.success() {
                let json_str = String::from_utf8_lossy(&output.stdout);
                Self::extract_outputs_from_json(&json_str, &mut all_paths)?;
            } else {
                error!(
                    "Batch nix build for non-flake targets failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                // Fallback to nix path-info
                let mut path_info_cmd = Command::new("nix");
                path_info_cmd.args(["path-info", "--file", file]);
                for a in attrs.iter().flatten() {
                    path_info_cmd.arg(*a);
                }

                let path_info_out = path_info_cmd.output().await?;
                if path_info_out.status.success() {
                    let paths = String::from_utf8_lossy(&path_info_out.stdout);
                    for p in paths.lines() {
                        let trimmed = p.trim();
                        if !trimmed.is_empty() {
                            all_paths.push(trimmed.to_string());
                        }
                    }
                } else {
                    return Err(BuilderError::NixCli(format!(
                        "Failed to build non-flake targets for {}",
                        file
                    )));
                }
            }
        }

        all_paths.sort();
        all_paths.dedup();
        Ok(all_paths)
    }

    fn extract_outputs_from_json(
        json_str: &str,
        all_paths: &mut Vec<String>,
    ) -> Result<(), BuilderError> {
        let val: Value = serde_json::from_str(json_str)?;
        if let Some(arr) = val.as_array() {
            for item in arr {
                if let Some(outputs) = item.get("outputs").and_then(|o| o.as_object()) {
                    for v in outputs.values() {
                        if let Some(p) = v.as_str() {
                            all_paths.push(p.to_string());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn find_locally_built_paths(
        &self,
        paths: &[String],
        own_hashes: &[String],
    ) -> Result<Vec<String>, BuilderError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        let mut cmd = Command::new("nix");
        cmd.args(["path-info", "--json", "--recursive"]);
        for p in paths {
            cmd.arg(p);
        }

        let output = cmd.output().await?;

        if !output.status.success() {
            return Err(BuilderError::NixCli(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        let items = parse_path_info_items_typed(&json_str)?;

        let own_hashes_set: HashSet<StoreHash> = own_hashes
            .iter()
            .map(|s| StoreHash::parse(s).unwrap_or_else(|_| StoreHash::new_unchecked(s)))
            .collect();

        let filter_ctx = NixArtifactFilterContext {
            own_public_key: None,
            remote_cached_hashes: &own_hashes_set,
            trusted_upstream_prefixes: &[],
        };

        let report = NixArtifactFilter::classify_and_filter(items, &filter_ctx);
        let mut result: Vec<String> = report.to_export.into_iter().map(|i| i.path).collect();
        result.sort();
        result.dedup();
        Ok(result)
    }

    pub async fn get_own_public_key(&self, signing_key_file: Option<&str>) -> Option<String> {
        let key_file = signing_key_file?;
        let pub_file = format!("{}.pub", key_file);
        if Path::new(&pub_file).exists() {
            fs::read_to_string(&pub_file)
                .await
                .ok()
                .map(|s| s.trim().to_string())
        } else {
            let mut output = Command::new("nix")
                .args(["key", "convert-secret-to-public"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .ok()?;

            let secret = fs::read_to_string(key_file).await.ok()?;
            let mut stdin = output.stdin.take()?;
            let _ = stdin.write_all(secret.as_bytes()).await;
            let _ = stdin.flush().await;
            drop(stdin);

            let res = output.wait_with_output().await.ok()?;
            if res.status.success() {
                Some(String::from_utf8_lossy(&res.stdout).trim().to_string())
            } else {
                None
            }
        }
    }

    /// 递归获取指定 Store 路径的完整强类型运行时闭包元数据
    pub async fn get_recursive_path_infos(
        &self,
        paths: &[String],
    ) -> Result<Vec<NixPathInfoItem>, BuilderError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut all_items = Vec::new();
        const BATCH_SIZE: usize = 128;

        for chunk in paths.chunks(BATCH_SIZE) {
            let mut cmd = Command::new("nix");
            cmd.args(["path-info", "--recursive", "--json"]);
            for p in chunk {
                cmd.arg(p);
            }

            let output = cmd.output().await?;
            if !output.status.success() {
                let err_msg = String::from_utf8_lossy(&output.stderr).to_string();
                return Err(BuilderError::NixCli(format!(
                    "nix path-info --recursive failed: {}",
                    err_msg
                )));
            }

            let json_str = String::from_utf8_lossy(&output.stdout);
            let items = parse_path_info_items_typed(&json_str)?;
            all_items.extend(items);
        }

        all_items.sort_by(|a, b| a.path.cmp(&b.path));
        all_items.dedup_by(|a, b| a.path == b.path);
        Ok(all_items)
    }

    /// 解析 Flake 表达式或属性目标为确定的 Store 路径
    pub async fn resolve_flake_or_attr_targets(
        &self,
        targets: &[&str],
    ) -> Result<Vec<String>, BuilderError> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        let mut resolved = Vec::new();
        let mut to_query = Vec::new();

        for t in targets {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("/nix/store/") {
                resolved.push(trimmed.to_string());
            } else {
                to_query.push(trimmed);
            }
        }

        if !to_query.is_empty() {
            let mut cmd = Command::new("nix");
            cmd.args([
                "--extra-experimental-features",
                "nix-command flakes",
                "path-info",
                "--json",
                "--accept-flake-config",
            ]);
            for target in &to_query {
                cmd.arg(target);
            }

            let output = cmd.output().await?;
            if output.status.success() {
                let json_str = String::from_utf8_lossy(&output.stdout);
                if let Ok(items) = parse_path_info_items_typed(&json_str) {
                    for item in items {
                        if !item.path.is_empty() {
                            resolved.push(item.path);
                        }
                    }
                }
            } else {
                for target in &to_query {
                    let mut eval_cmd = Command::new("nix");
                    eval_cmd.args([
                        "--extra-experimental-features",
                        "nix-command flakes",
                        "eval",
                        "--accept-flake-config",
                        "--raw",
                        &format!("{}.outPath", target),
                    ]);
                    if let Ok(eval_out) = eval_cmd.output().await
                        && eval_out.status.success()
                    {
                        let path = String::from_utf8_lossy(&eval_out.stdout).trim().to_string();
                        if path.starts_with("/nix/store/") {
                            resolved.push(path);
                            continue;
                        }
                    }

                    let mut single_cmd = Command::new("nix");
                    single_cmd.args([
                        "--extra-experimental-features",
                        "nix-command flakes",
                        "path-info",
                        "--accept-flake-config",
                        target,
                    ]);
                    if let Ok(single_out) = single_cmd.output().await
                        && single_out.status.success()
                    {
                        for line in String::from_utf8_lossy(&single_out.stdout).lines() {
                            let p = line.trim();
                            if p.starts_with("/nix/store/") {
                                resolved.push(p.to_string());
                            }
                        }
                    } else {
                        return Err(BuilderError::NixCli(format!(
                            "Failed to resolve target expression: {}",
                            target
                        )));
                    }
                }
            }
        }

        resolved.sort();
        resolved.dedup();
        Ok(resolved)
    }

    /// 递归获取目标产物的完整构建闭包 (包含 Derivation 及 buildInputs 等编译期依赖图)
    pub async fn get_build_closure(&self, paths: &[String]) -> Result<Vec<String>, BuilderError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        let mut build_paths = HashSet::new();

        // 1. 获取运行时闭包与 deriver
        let items = self.get_recursive_path_infos(paths).await?;
        let mut derivers = Vec::new();

        for item in &items {
            build_paths.insert(item.path.clone());
            if let Some(ref drv) = item.deriver
                && !drv.is_empty()
            {
                derivers.push(drv.clone());
            }
        }

        // 2. 递归查询 Deriver 的前向构建依赖
        if !derivers.is_empty() {
            derivers.sort();
            derivers.dedup();

            for chunk in derivers.chunks(128) {
                let mut cmd = Command::new("nix-store");
                cmd.args(["--query", "--requisites"]);
                for drv in chunk {
                    cmd.arg(drv);
                }

                if let Ok(output) = cmd.output().await
                    && output.status.success()
                {
                    for line in String::from_utf8_lossy(&output.stdout).lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with("/nix/store/") {
                            build_paths.insert(trimmed.to_string());
                        }
                    }
                } else {
                    let mut path_info_cmd = Command::new("nix");
                    path_info_cmd.args(["path-info", "--recursive"]);
                    for drv in chunk {
                        path_info_cmd.arg(drv);
                    }
                    if let Ok(pi_out) = path_info_cmd.output().await
                        && pi_out.status.success()
                    {
                        for line in String::from_utf8_lossy(&pi_out.stdout).lines() {
                            let trimmed = line.trim();
                            if trimmed.starts_with("/nix/store/") {
                                build_paths.insert(trimmed.to_string());
                            }
                        }
                    }
                }
            }
        }

        let mut result: Vec<String> = build_paths.into_iter().collect();
        result.sort();
        Ok(result)
    }
}

pub fn parse_path_info_items_typed(json_str: &str) -> Result<Vec<NixPathInfoItem>, BuilderError> {
    let parsed: Value = serde_json::from_str(json_str)?;
    parse_path_info_value_typed(&parsed)
}

pub fn parse_path_info_value_typed(parsed: &Value) -> Result<Vec<NixPathInfoItem>, BuilderError> {
    if let Some(arr) = parsed.as_array() {
        let mut list = Vec::with_capacity(arr.len());
        for v in arr {
            let item: NixPathInfoItem = serde_json::from_value(v.clone())?;
            list.push(item);
        }
        Ok(list)
    } else if let Some(obj) = parsed.as_object() {
        let mut list = Vec::with_capacity(obj.len());
        for (path, v) in obj {
            let mut item: NixPathInfoItem = if v.is_null() {
                NixPathInfoItem {
                    path: path.clone(),
                    ..Default::default()
                }
            } else {
                serde_json::from_value(v.clone())?
            };
            if item.path.is_empty() {
                item.path = path.clone();
            }
            list.push(item);
        }
        Ok(list)
    } else {
        Err(BuilderError::NixCli(
            "Unexpected path-info JSON format".to_string(),
        ))
    }
}

pub async fn get_own_public_key(signing_key_file: Option<&str>) -> Option<String> {
    NixCli.get_own_public_key(signing_key_file).await
}

#[cfg(test)]
mod tests {
    use super::{BuildMode, BuildTarget, NixCli, parse_path_info_items_typed};
    use std::str::FromStr;

    #[test]
    fn test_build_mode_parsing() {
        assert_eq!(BuildMode::from_str("flake").unwrap(), BuildMode::Flake);
        assert_eq!(BuildMode::from_str("FLAKE ").unwrap(), BuildMode::Flake);
        assert_eq!(
            BuildMode::from_str("non-flake").unwrap(),
            BuildMode::NonFlake
        );
        assert_eq!(
            BuildMode::from_str("nonflake").unwrap(),
            BuildMode::NonFlake
        );
        assert_eq!(
            BuildMode::from_str("Non-Flake").unwrap(),
            BuildMode::NonFlake
        );
        assert!(BuildMode::from_str("invalid").is_err());
    }

    #[test]
    fn test_extract_outputs_from_json() {
        let json_str = r#"[
            {
                "outputs": {
                    "out": "/nix/store/11111111111111111111111111111111-pkg-a",
                    "dev": "/nix/store/22222222222222222222222222222222-pkg-a-dev"
                }
            },
            {
                "outputs": {
                    "bin": "/nix/store/33333333333333333333333333333333-pkg-b"
                }
            },
            {
                "outputs": {}
            }
        ]"#;

        let mut paths = Vec::new();
        NixCli::extract_outputs_from_json(json_str, &mut paths).unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths.contains(&"/nix/store/11111111111111111111111111111111-pkg-a".to_string()));
        assert!(
            paths.contains(&"/nix/store/22222222222222222222222222222222-pkg-a-dev".to_string())
        );
        assert!(paths.contains(&"/nix/store/33333333333333333333333333333333-pkg-b".to_string()));
    }

    #[test]
    fn test_build_target_display() {
        let flake_target = BuildTarget::Flake {
            flake_ref: "path:/root/workspace".to_string(),
            attribute: "packages.x86_64-linux.my-pkg".to_string(),
        };
        assert_eq!(
            format!("{}", flake_target),
            "path:/root/workspace#packages.x86_64-linux.my-pkg"
        );

        let non_flake_target_attr = BuildTarget::NonFlake {
            file: "default.nix".to_string(),
            attribute: Some("cache-proxy".to_string()),
        };
        assert_eq!(
            format!("{}", non_flake_target_attr),
            "default.nix -A cache-proxy"
        );

        let non_flake_target_no_attr = BuildTarget::NonFlake {
            file: "default.nix".to_string(),
            attribute: None,
        };
        assert_eq!(format!("{}", non_flake_target_no_attr), "default.nix");
    }

    #[test]
    fn test_parse_path_info_invalid_format() {
        let json_data = "invalid string";
        assert!(parse_path_info_items_typed(json_data).is_err());
    }

    #[test]
    fn test_parse_path_info_typed_formats() {
        let json_arr = r#"[
            {
                "path": "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-typed-1",
                "narHash": "sha256:1111",
                "narSize": 100,
                "references": ["/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep"],
                "signatures": ["sig1"]
            }
        ]"#;

        let items = parse_path_info_items_typed(json_arr).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].path,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-typed-1"
        );
        assert_eq!(items[0].nar_hash, "sha256:1111");
        assert_eq!(items[0].nar_size, 100);
        assert_eq!(
            items[0].normalized_references(),
            vec!["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep"]
        );

        let json_map = r#"{
            "/nix/store/cccccccccccccccccccccccccccccccc-typed-map": {
                "narHash": "sha256:2222",
                "narSize": 200,
                "references": []
            }
        }"#;

        let map_items = parse_path_info_items_typed(json_map).unwrap();
        assert_eq!(map_items.len(), 1);
        assert_eq!(
            map_items[0].path,
            "/nix/store/cccccccccccccccccccccccccccccccc-typed-map"
        );
        assert_eq!(map_items[0].nar_size, 200);
    }
}
