use crate::{
    error::{BuilderError, NixExecError},
    nix::driver::{BuildConfig, BuildMode, BuildTarget, NixCli},
};
use serde::Deserialize;
use std::path::Path;
use tokio::{fs, process::Command};
use tracing::info;

#[derive(Deserialize, Debug, Default)]
struct DiscoveredFlakeOutputs {
    #[serde(default)]
    pkgs: Vec<String>,
    #[serde(rename = "devShells", default)]
    dev_shells: Vec<String>,
    #[serde(default)]
    nixos: Vec<String>,
}

/// 发现 Nix 构建目标
pub async fn discover_outputs(config: &BuildConfig) -> Result<Vec<BuildTarget>, BuilderError> {
    match config.mode {
        BuildMode::Flake => {
            let cli = NixCli;
            let system = match &config.system {
                Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                _ => cli.current_system().await?,
            };
            info!(
                "Discovering flake outputs for {} in {}",
                system, config.flake_path
            );

            let flake_ref = format!(
                "path:{}",
                fs::canonicalize(&config.flake_path)
                    .await?
                    .to_string_lossy()
            );

            let lock_file = Path::new(&config.flake_path).join("flake.lock");
            if !lock_file.exists() {
                info!("Generating flake.lock for {}", config.flake_path);
                let status = Command::new("nix")
                    .args(["flake", "update", "--flake", &flake_ref])
                    .status()
                    .await?;
                if !status.success() {
                    return Err(NixExecError::ExitFailure {
                        command: format!("nix flake update --flake {}", flake_ref),
                        status,
                        stderr: String::new(),
                    }
                    .into());
                }
            }

            let mut targets = Vec::new();

            // 一次性求值 Packages, NixOS Configurations 与 DevShells
            let expr = format!(
                r#"let
  flake = builtins.getFlake "{}";
  sys = "{}";
  pkgs = builtins.attrNames (flake.packages.${{sys}} or {{}});
  devShells = builtins.attrNames (flake.devShells.${{sys}} or {{}});
  allNixos = flake.nixosConfigurations or {{}};
  nixos = builtins.filter (name:
    let cfg = allNixos.${{name}};
    in (cfg.config.nixpkgs.system or cfg.pkgs.system or sys) == sys
  ) (builtins.attrNames allNixos);
in {{ inherit pkgs devShells nixos; }}"#,
                flake_ref, system
            );

            let output = Command::new("nix")
                .args(["eval", "--json", "--impure", "--expr", &expr])
                .output()
                .await?;

            if !output.status.success() {
                let err_msg = String::from_utf8_lossy(&output.stderr).to_string();
                return Err(NixExecError::ExitFailure {
                    command: "nix eval --json --impure --expr".to_string(),
                    status: output.status,
                    stderr: err_msg,
                }
                .into());
            }

            let json_str = String::from_utf8_lossy(&output.stdout);
            let discovered: DiscoveredFlakeOutputs = serde_json::from_str(&json_str)?;

            for name in discovered.pkgs {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    targets.push(BuildTarget::Flake {
                        flake_ref: flake_ref.clone(),
                        attribute: format!("packages.{}.{}", system, trimmed),
                    });
                }
            }

            for name in discovered.nixos {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    targets.push(BuildTarget::Flake {
                        flake_ref: flake_ref.clone(),
                        attribute: format!(
                            "nixosConfigurations.{}.config.system.build.toplevel",
                            trimmed
                        ),
                    });
                }
            }

            for name in discovered.dev_shells {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    targets.push(BuildTarget::Flake {
                        flake_ref: flake_ref.clone(),
                        attribute: format!("devShells.{}.{}", system, trimmed),
                    });
                }
            }

            if targets.is_empty() {
                return Err(NixExecError::Execution(format!(
                    "No buildable outputs found for {} in {}",
                    system, config.flake_path
                ))
                .into());
            }

            Ok(targets)
        }
        BuildMode::NonFlake => {
            if !config.attributes.is_empty() {
                let targets = config
                    .attributes
                    .iter()
                    .map(|attr| BuildTarget::NonFlake {
                        file: config.file.clone(),
                        attribute: Some(attr.clone()),
                    })
                    .collect();
                Ok(targets)
            } else {
                Ok(vec![BuildTarget::NonFlake {
                    file: config.file.clone(),
                    attribute: None,
                }])
            }
        }
    }
}

/// 解析 Flake 及其属性对应的输出 StoreHash 列表
pub async fn resolve_flake_output_hashes(
    flake_path: &str,
    attributes: &[String],
) -> Result<Vec<nixcache_core::StoreHash>, BuilderError> {
    use nixcache_core::extract_store_hash;
    use tracing::warn;

    let mut hashes = Vec::new();
    let abs_flake_path = match fs::canonicalize(flake_path).await {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(e) => {
            warn!("Failed to canonicalize flake path {}: {}", flake_path, e);
            flake_path.to_string()
        }
    };

    if !attributes.is_empty() {
        for attr in attributes {
            let target = format!("path:{}#{}", abs_flake_path, attr);
            let output = Command::new("nix")
                .args(["path-info", "--accept-flake-config", "--json", &target])
                .output()
                .await;

            if let Ok(out) = output
                && out.status.success()
            {
                let stdout_str = String::from_utf8_lossy(&out.stdout);
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&stdout_str) {
                    if let Some(obj) = val.as_object() {
                        for k in obj.keys() {
                            if let Some(h) = extract_store_hash(k) {
                                hashes.push(h);
                            }
                        }
                    } else if let Some(arr) = val.as_array() {
                        for item in arr {
                            if let Some(path_str) = item.get("path").and_then(|p| p.as_str())
                                && let Some(h) = extract_store_hash(path_str)
                            {
                                hashes.push(h);
                            }
                        }
                    }
                }
            } else {
                let eval_out = Command::new("nix")
                    .args([
                        "eval",
                        "--accept-flake-config",
                        "--raw",
                        &format!("{}.outPath", target),
                    ])
                    .output()
                    .await;

                if let Ok(e_out) = eval_out
                    && e_out.status.success()
                {
                    let p = String::from_utf8_lossy(&e_out.stdout).trim().to_string();
                    if let Some(h) = extract_store_hash(&p) {
                        hashes.push(h);
                    }
                }
            }
        }
    } else {
        let build_config = BuildConfig {
            system: None,
            mode: BuildMode::Flake,
            flake_path: abs_flake_path.clone(),
            file: "default.nix".to_string(),
            attributes: Vec::new(),
        };
        if let Ok(targets) = discover_outputs(&build_config).await {
            for target in targets {
                if let BuildTarget::Flake {
                    flake_ref,
                    attribute,
                } = target
                {
                    let full_target = format!("{}#{}", flake_ref, attribute);
                    let eval_out = Command::new("nix")
                        .args([
                            "eval",
                            "--accept-flake-config",
                            "--raw",
                            &format!("{}.outPath", full_target),
                        ])
                        .output()
                        .await;

                    if let Ok(e_out) = eval_out
                        && e_out.status.success()
                    {
                        let p = String::from_utf8_lossy(&e_out.stdout).trim().to_string();
                        if let Some(h) = extract_store_hash(&p) {
                            hashes.push(h);
                        }
                    }
                }
            }
        }
    }

    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use super::{DiscoveredFlakeOutputs, discover_outputs};
    use crate::nix::driver::{BuildConfig, BuildMode, BuildTarget};

    #[test]
    fn test_discovered_flake_outputs_deserialization() {
        let json_str = r#"{
            "pkgs": ["pkgA", "pkgB"],
            "devShells": ["default"],
            "nixos": ["host1"]
        }"#;
        let outputs: DiscoveredFlakeOutputs = serde_json::from_str(json_str).unwrap();
        assert_eq!(outputs.pkgs, vec!["pkgA", "pkgB"]);
        assert_eq!(outputs.dev_shells, vec!["default"]);
        assert_eq!(outputs.nixos, vec!["host1"]);
    }

    #[tokio::test]
    async fn test_non_flake_discovery() {
        let config = BuildConfig {
            system: Some("x86_64-linux".to_string()),
            mode: BuildMode::NonFlake,
            flake_path: ".".to_string(),
            file: "default.nix".to_string(),
            attributes: vec!["pkgA".to_string(), "pkgB".to_string()],
        };

        let targets = discover_outputs(&config).await.unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets[0],
            BuildTarget::NonFlake {
                file: "default.nix".to_string(),
                attribute: Some("pkgA".to_string()),
            }
        );
        assert_eq!(
            targets[1],
            BuildTarget::NonFlake {
                file: "default.nix".to_string(),
                attribute: Some("pkgB".to_string()),
            }
        );
    }
}
