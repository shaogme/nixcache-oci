use clap::Args;
use nixcache_utils::Env;
use tokio::process::Command;

/// 认证令牌参数组与解析器
#[derive(Args, Debug, Clone, Default)]
pub struct AuthTokenArgs {
    #[arg(long, help = "GitHub token for authentication [env: GITHUB_TOKEN]")]
    pub github_token: Option<String>,

    #[arg(long, help = "GitHub token fallback [env: GH_TOKEN]")]
    pub gh_token: Option<String>,
}

impl AuthTokenArgs {
    /// 异步解析有效 GitHub Token：CLI 参数优先 -> 环境变量 (GITHUB_TOKEN/GH_TOKEN) -> gh auth token
    pub async fn resolve_token(&self) -> String {
        let mut token = self
            .github_token
            .as_deref()
            .and_then(Env::non_empty_str)
            .or_else(|| self.gh_token.as_deref().and_then(Env::non_empty_str))
            .map(|s| s.to_string())
            .or_else(|| Env::get_first(&["GITHUB_TOKEN", "GH_TOKEN"]))
            .unwrap_or_default();

        if token.is_empty()
            && let Ok(output) = Command::new("gh").args(["auth", "token"]).output().await
            && output.status.success()
        {
            let token_from_gh = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token_from_gh.is_empty() {
                token = token_from_gh;
            }
        }
        token
    }

    /// 同步解析 GitHub Token（仅读取 CLI 参数与环境变量，不探测外部 gh CLI）
    pub fn resolve_token_sync(&self) -> String {
        self.github_token
            .as_deref()
            .and_then(Env::non_empty_str)
            .or_else(|| self.gh_token.as_deref().and_then(Env::non_empty_str))
            .map(|s| s.to_string())
            .or_else(|| Env::get_first(&["GITHUB_TOKEN", "GH_TOKEN"]))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::AuthTokenArgs;
    use std::env;

    #[tokio::test]
    async fn test_auth_token_resolution() {
        let explicit = AuthTokenArgs {
            github_token: Some("ghp_explicit123".to_string()),
            gh_token: None,
        };
        assert_eq!(explicit.resolve_token().await, "ghp_explicit123");

        let fallback_cli = AuthTokenArgs {
            github_token: Some("  ".to_string()),
            gh_token: Some("ghp_fallback456".to_string()),
        };
        assert_eq!(fallback_cli.resolve_token().await, "ghp_fallback456");

        let empty = AuthTokenArgs::default();
        unsafe {
            env::set_var("GITHUB_TOKEN", "ghp_env789");
        }
        assert_eq!(empty.resolve_token().await, "ghp_env789");

        unsafe {
            env::remove_var("GITHUB_TOKEN");
        }
    }
}
