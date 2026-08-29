use crate::{env_injector::NixEnvInjector, error::BuilderError};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{env, fmt, path::Path, process::Stdio, str::FromStr};
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
        let mut all_paths = Vec::new();
        for target in targets {
            info!("Building target: {}", target);
            let mut cmd = Command::new("nix");
            cmd.arg("build");
            if let Ok(config) = env::var("NIX_CONFIG") {
                NixEnvInjector::apply_to_command(&mut cmd, &config);
            }

            match target {
                BuildTarget::Flake {
                    flake_ref,
                    attribute,
                } => {
                    cmd.arg(format!("{}#{}", flake_ref, attribute));
                    cmd.args(["--no-link", "--accept-flake-config", "--json"]);
                }
                BuildTarget::NonFlake { file, attribute } => {
                    cmd.args(["--file", file]);
                    if let Some(attr) = attribute {
                        cmd.arg(attr);
                    }
                    cmd.args(["--no-link", "--json"]);
                }
            }

            let output = cmd.output().await?;

            if !output.status.success() {
                error!(
                    "nix build failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                // Fallback to nix path-info
                let mut path_info_cmd = Command::new("nix");
                path_info_cmd.arg("path-info");
                match target {
                    BuildTarget::Flake {
                        flake_ref,
                        attribute,
                    } => {
                        path_info_cmd.arg(format!("{}#{}", flake_ref, attribute));
                    }
                    BuildTarget::NonFlake { file, attribute } => {
                        path_info_cmd.args(["--file", file]);
                        if let Some(attr) = attribute {
                            path_info_cmd.arg(attr);
                        }
                    }
                }

                let path_info_out = path_info_cmd.output().await?;
                if path_info_out.status.success() {
                    let paths = String::from_utf8_lossy(&path_info_out.stdout);
                    for p in paths.lines() {
                        if !p.trim().is_empty() {
                            all_paths.push(p.trim().to_string());
                        }
                    }
                    continue;
                }
                return Err(BuilderError::NixCli(format!(
                    "Failed to build target: {}",
                    target
                )));
            }

            let json_str = String::from_utf8_lossy(&output.stdout);
            if let Ok(val) = serde_json::from_str::<Value>(&json_str) {
                if let Some(arr) = val.as_array() {
                    for item in arr {
                        if let Some(outputs) = item.get("outputs").and_then(|o| o.as_object()) {
                            for val in outputs.values() {
                                if let Some(p) = val.as_str() {
                                    all_paths.push(p.to_string());
                                }
                            }
                        }
                    }
                }
            } else {
                return Err(BuilderError::NixCli(format!(
                    "Failed to parse build JSON output for target: {}",
                    target
                )));
            }
        }

        Ok(all_paths)
    }

    pub async fn find_locally_built_paths(
        &self,
        paths: &[String],
        own_hashes: &[String],
    ) -> Result<Vec<String>, BuilderError> {
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
        let parsed = serde_json::from_str::<Value>(&json_str)?;

        let items = parse_path_info_items(&parsed)?;
        Ok(filter_locally_built_paths(&items, own_hashes))
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
}

pub fn parse_path_info_items(parsed: &Value) -> Result<Vec<Value>, BuilderError> {
    if let Some(arr) = parsed.as_array() {
        Ok(arr.clone())
    } else if let Some(obj) = parsed.as_object() {
        let mut list = Vec::new();
        for (path, val) in obj {
            let mut item = val.clone();
            if let Some(item_obj) = item.as_object_mut() {
                item_obj.insert("path".to_string(), Value::String(path.clone()));
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

pub fn filter_locally_built_paths(items: &[Value], own_hashes: &[String]) -> Vec<String> {
    let mut locally_built = Vec::new();
    for item in items {
        if let Some(path) = item.get("path").and_then(|p| p.as_str()) {
            let sigs = item
                .get("signatures")
                .or_else(|| item.get("sigs"))
                .and_then(|s| s.as_array());

            let has_sig = match sigs {
                None => false,
                Some(arr) => !arr.is_empty(),
            };

            if !has_sig
                && let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str())
                && name.len() >= 32
            {
                let hash = &name[..32];
                if !own_hashes.iter().any(|h| h == hash) {
                    locally_built.push(path.to_string());
                }
            }
        }
    }

    locally_built.sort();
    locally_built.dedup();
    locally_built
}

pub async fn get_own_public_key(signing_key_file: Option<&str>) -> Option<String> {
    NixCli.get_own_public_key(signing_key_file).await
}

#[cfg(test)]
mod tests {
    use super::{BuildTarget, filter_locally_built_paths, parse_path_info_items};
    use serde_json::json;

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
    fn test_parse_path_info_array_format() {
        let json_data = json!([
            {
                "path": "/nix/store/00000000000000000000000000000001-local-unsigned",
                "signatures": [],
                "narHash": "sha256:abcd",
                "narSize": 1000
            },
            {
                "path": "/nix/store/00000000000000000000000000000002-upstream-signed",
                "signatures": ["cache.nixos.org-1:sig123"],
                "narHash": "sha256:efgh",
                "narSize": 2000
            },
            {
                "path": "/nix/store/00000000000000000000000000000003-already-in-cache",
                "signatures": [],
                "narHash": "sha256:ijkl",
                "narSize": 3000
            }
        ]);

        let items = parse_path_info_items(&json_data).unwrap();
        assert_eq!(items.len(), 3);

        let own_hashes = vec!["00000000000000000000000000000003".to_string()];
        let filtered = filter_locally_built_paths(&items, &own_hashes);

        assert_eq!(
            filtered,
            vec!["/nix/store/00000000000000000000000000000001-local-unsigned".to_string()]
        );
    }

    #[test]
    fn test_parse_path_info_object_format() {
        let json_data = json!({
            "/nix/store/11111111111111111111111111111111-map-pkg": {
                "narHash": "sha256:xxxx",
                "narSize": 500
            }
        });

        let items = parse_path_info_items(&json_data).unwrap();
        assert_eq!(items.len(), 1);

        let filtered = filter_locally_built_paths(&items, &[]);
        assert_eq!(
            filtered,
            vec!["/nix/store/11111111111111111111111111111111-map-pkg".to_string()]
        );
    }

    #[test]
    fn test_parse_path_info_invalid_format() {
        let json_data = json!("invalid string");
        assert!(parse_path_info_items(&json_data).is_err());
    }
}
