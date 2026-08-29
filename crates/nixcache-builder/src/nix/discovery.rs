use crate::{
    error::BuilderError,
    nix::driver::{BuildConfig, BuildMode, BuildTarget, NixCli},
};
use std::path::Path;
use tokio::{fs, process::Command};
use tracing::info;

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
                    .await
                    .map_err(|e| BuilderError::Config(format!(
                        "Invalid path {}: {}",
                        config.flake_path, e
                    )))?
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
                    return Err(BuilderError::NixCli("nix flake update failed".to_string()));
                }
            }

            let mut targets = Vec::new();

            // 1. Packages
            let expr = format!("{}#packages.{}", flake_ref, system);
            let output = Command::new("nix")
                .args([
                    "eval",
                    &expr,
                    "--apply",
                    "attrs: builtins.concatStringsSep \"\\n\" (builtins.attrNames attrs)",
                    "--raw",
                ])
                .output()
                .await;
            if let Ok(out) = output
                && out.status.success()
            {
                let names = String::from_utf8_lossy(&out.stdout);
                for name in names.lines() {
                    if !name.trim().is_empty() {
                        targets.push(BuildTarget::Flake {
                            flake_ref: flake_ref.clone(),
                            attribute: format!("packages.{}.{}", system, name.trim()),
                        });
                    }
                }
            }

            // 2. NixOS Configurations
            let expr = format!("{}#nixosConfigurations", flake_ref);
            let filter_expr = format!(
                "attrs: builtins.concatStringsSep \"\\n\" (builtins.filter (name: (attrs.${{name}}.config.nixpkgs.system or attrs.${{name}}.pkgs.system or \"{}\") == \"{}\") (builtins.attrNames attrs))",
                system, system
            );
            let output = Command::new("nix")
                .args(["eval", &expr, "--apply", &filter_expr, "--raw"])
                .output()
                .await;
            if let Ok(out) = output
                && out.status.success()
            {
                let names = String::from_utf8_lossy(&out.stdout);
                for name in names.lines() {
                    if !name.trim().is_empty() {
                        targets.push(BuildTarget::Flake {
                            flake_ref: flake_ref.clone(),
                            attribute: format!(
                                "nixosConfigurations.{}.config.system.build.toplevel",
                                name.trim()
                            ),
                        });
                    }
                }
            }

            // 3. DevShells
            let expr = format!("{}#devShells.{}", flake_ref, system);
            let output = Command::new("nix")
                .args([
                    "eval",
                    &expr,
                    "--apply",
                    "attrs: builtins.concatStringsSep \"\\n\" (builtins.attrNames attrs)",
                    "--raw",
                ])
                .output()
                .await;
            if let Ok(out) = output
                && out.status.success()
            {
                let names = String::from_utf8_lossy(&out.stdout);
                for name in names.lines() {
                    if !name.trim().is_empty() {
                        targets.push(BuildTarget::Flake {
                            flake_ref: flake_ref.clone(),
                            attribute: format!("devShells.{}.{}", system, name.trim()),
                        });
                    }
                }
            }

            if targets.is_empty() {
                return Err(BuilderError::NixCli(format!(
                    "No buildable outputs found for {} in {}",
                    system, config.flake_path
                )));
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

#[cfg(test)]
mod tests {
    use super::discover_outputs;
    use crate::nix::driver::{BuildConfig, BuildMode, BuildTarget};

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
