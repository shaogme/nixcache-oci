use clap::Args;
use nixcache_utils::Env;

/// 会话与 CI/CD 运行时上下文参数组
#[derive(Args, Debug, Clone, Default)]
pub struct SessionContextArgs {
    #[arg(long, help = "GitHub Actions Workflow Run ID [env: NIXCACHE_RUN_ID]")]
    pub run_id: Option<u64>,

    #[arg(long, help = "Branch name or PR ref [env: NIXCACHE_BRANCH]")]
    pub branch: Option<String>,

    #[arg(long, help = "GitHub Actions Job Identifier [env: NIXCACHE_JOB_ID]")]
    pub job_id: Option<String>,

    #[arg(
        long,
        help = "Target platform system architecture [env: NIXCACHE_SYSTEM]"
    )]
    pub system: Option<String>,
}

impl SessionContextArgs {
    /// 解析 Workflow Run ID（支持 NIXCACHE_RUN_ID 与 GITHUB_RUN_ID 级联回退）
    pub fn resolve_run_id(&self) -> Option<u64> {
        self.run_id
            .or_else(|| Env::parse_first(&["NIXCACHE_RUN_ID", "GITHUB_RUN_ID"]))
    }

    /// 解析分支或 PR 标识（支持 NIXCACHE_BRANCH -> GITHUB_REF_NAME -> GITHUB_HEAD_REF 级联回退）
    pub fn resolve_branch(&self) -> Option<String> {
        self.branch
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get_first(&["NIXCACHE_BRANCH", "GITHUB_REF_NAME", "GITHUB_HEAD_REF"]))
    }

    /// 解析 Job 标识（默认 default-job）
    pub fn resolve_job_id(&self, default_job: &str) -> String {
        self.job_id
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get_first(&["NIXCACHE_JOB_ID", "GITHUB_JOB"]))
            .unwrap_or_else(|| default_job.to_string())
    }

    /// 解析 System 架构名称
    pub fn resolve_system(&self) -> Option<String> {
        self.system
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_SYSTEM"))
    }
}

#[cfg(test)]
mod tests {
    use super::SessionContextArgs;
    use std::env;

    #[test]
    fn test_session_context_resolution() {
        let empty = SessionContextArgs::default();
        assert_eq!(empty.resolve_run_id(), None);
        assert_eq!(empty.resolve_branch(), None);
        assert_eq!(empty.resolve_job_id("default-job"), "default-job");
        assert_eq!(empty.resolve_system(), None);

        unsafe {
            env::set_var("GITHUB_RUN_ID", "98765");
            env::set_var("GITHUB_REF_NAME", "main");
            env::set_var("GITHUB_JOB", "build-linux");
            env::set_var("NIXCACHE_SYSTEM", "x86_64-linux");
        }

        assert_eq!(empty.resolve_run_id(), Some(98765));
        assert_eq!(empty.resolve_branch(), Some("main".to_string()));
        assert_eq!(empty.resolve_job_id("default-job"), "build-linux");
        assert_eq!(empty.resolve_system(), Some("x86_64-linux".to_string()));

        let explicit = SessionContextArgs {
            run_id: Some(111),
            branch: Some("feature/pr-1".to_string()),
            job_id: Some("custom-job".to_string()),
            system: Some("aarch64-linux".to_string()),
        };
        assert_eq!(explicit.resolve_run_id(), Some(111));
        assert_eq!(explicit.resolve_branch(), Some("feature/pr-1".to_string()));
        assert_eq!(explicit.resolve_job_id("default-job"), "custom-job");
        assert_eq!(explicit.resolve_system(), Some("aarch64-linux".to_string()));

        unsafe {
            env::remove_var("GITHUB_RUN_ID");
            env::remove_var("GITHUB_REF_NAME");
            env::remove_var("GITHUB_JOB");
            env::remove_var("NIXCACHE_SYSTEM");
        }
    }
}
