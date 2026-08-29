use std::{env, io};
use tokio::{fs::OpenOptions, io::AsyncWriteExt, process::Command};
use tracing::info;

/// 零侵入 Nix 环境变量注入器
pub struct NixEnvInjector;

impl NixEnvInjector {
    /// 生成零侵入的 NIX_CONFIG 环境变量内容
    pub fn generate_nix_config(substituters: &[&str], trusted_public_keys: &[&str]) -> String {
        let mut lines = Vec::new();
        if !substituters.is_empty() {
            let joined_subs = substituters.join(" ");
            lines.push(format!("extra-substituters = {}", joined_subs));
            lines.push(format!("extra-trusted-substituters = {}", joined_subs));
        }
        if !trusted_public_keys.is_empty() {
            let joined_keys = trusted_public_keys.join(" ");
            lines.push(format!("extra-trusted-public-keys = {}", joined_keys));
        }
        lines.join("\n")
    }

    /// 在 GitHub Actions 环境下自动导出至 GITHUB_ENV
    pub async fn export_to_github_env(nix_config: &str) -> io::Result<()> {
        Self::export_to_file(nix_config, env::var("GITHUB_ENV").ok().as_deref()).await
    }

    /// 导出 NIX_CONFIG 到指定的文件路径
    pub async fn export_to_file(nix_config: &str, file_path_opt: Option<&str>) -> io::Result<()> {
        if nix_config.trim().is_empty() {
            return Ok(());
        }

        if let Some(github_env_path) = file_path_opt {
            let delimiter = "EOF_NIXCACHE_CONFIG";
            let payload = format!("NIX_CONFIG<<{}\n{}\n{}\n", delimiter, nix_config, delimiter);
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(github_env_path)
                .await?
                .write_all(payload.as_bytes())
                .await?;
            info!(
                "Exported NIX_CONFIG to GITHUB_ENV file at {}",
                github_env_path
            );
        }
        Ok(())
    }

    /// 将 NIX_CONFIG 注入到给定的 Tokio Command 中
    pub fn apply_to_command(cmd: &mut Command, nix_config: &str) {
        if !nix_config.trim().is_empty() {
            let existing = env::var("NIX_CONFIG").unwrap_or_default();
            let merged = if existing.is_empty() {
                nix_config.to_string()
            } else {
                format!("{}\n{}", existing, nix_config)
            };
            cmd.env("NIX_CONFIG", merged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NixEnvInjector;
    use tempfile::NamedTempFile;
    use tokio::fs;


    #[test]
    fn test_generate_nix_config() {
        let subs = ["http://127.0.0.1:37515", "https://cache.nixos.org"];
        let keys = ["cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="];

        let config = NixEnvInjector::generate_nix_config(&subs, &keys);
        assert!(
            config.contains("extra-substituters = http://127.0.0.1:37515 https://cache.nixos.org")
        );
        assert!(config.contains(
            "extra-trusted-substituters = http://127.0.0.1:37515 https://cache.nixos.org"
        ));
        assert!(config.contains(
            "extra-trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
        ));
    }

    #[test]
    fn test_generate_empty_nix_config() {
        let config = NixEnvInjector::generate_nix_config(&[], &[]);
        assert!(config.is_empty());
    }

    #[tokio::test]
    async fn test_export_to_github_env() {
        let temp_file = NamedTempFile::new().unwrap();
        let path_str = temp_file.path().to_string_lossy().to_string();

        let config = "extra-substituters = http://127.0.0.1:37515";
        let res = NixEnvInjector::export_to_file(config, Some(&path_str)).await;
        assert!(res.is_ok());

        let written = fs::read_to_string(&path_str).await.unwrap();
        assert!(written.contains("NIX_CONFIG<<EOF_NIXCACHE_CONFIG"));
        assert!(written.contains("extra-substituters = http://127.0.0.1:37515"));
    }

    #[test]
    fn test_apply_to_command() {
        use std::ffi::OsStr;

        let mut cmd = tokio::process::Command::new("nix");
        NixEnvInjector::apply_to_command(&mut cmd, "extra-substituters = http://127.0.0.1:37515");
        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.as_std().get_envs().collect();
        let nix_cfg_opt = envs
            .iter()
            .find(|(k, _)| *k == "NIX_CONFIG")
            .and_then(|(_, v)| *v);
        assert!(nix_cfg_opt.is_some());
        assert!(
            nix_cfg_opt
                .unwrap()
                .to_str()
                .unwrap()
                .contains("extra-substituters = http://127.0.0.1:37515")
        );
    }
}
