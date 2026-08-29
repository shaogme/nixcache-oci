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

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn capture_and_clear(keys: &[&'static str]) -> Self {
            let saved = keys
                .iter()
                .map(|&key| {
                    let val = env::var(key).ok();
                    unsafe {
                        env::remove_var(key);
                    }
                    (key, val)
                })
                .collect();
            Self { saved }
        }

        fn set(&self, key: &str, value: &str) {
            unsafe {
                env::set_var(key, value);
            }
        }

        fn remove(&self, key: &str) {
            unsafe {
                env::remove_var(key);
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for &(key, ref val) in &self.saved {
                unsafe {
                    if let Some(v) = val {
                        env::set_var(key, v);
                    } else {
                        env::remove_var(key);
                    }
                }
            }
        }
    }

    #[test]
    fn test_session_context_resolution() {
        let keys = [
            "NIXCACHE_RUN_ID",
            "GITHUB_RUN_ID",
            "NIXCACHE_BRANCH",
            "GITHUB_REF_NAME",
            "GITHUB_HEAD_REF",
            "NIXCACHE_JOB_ID",
            "GITHUB_JOB",
            "NIXCACHE_SYSTEM",
        ];
        let guard = EnvGuard::capture_and_clear(&keys);

        let empty = SessionContextArgs::default();
        assert_eq!(empty.resolve_run_id(), None);
        assert_eq!(empty.resolve_branch(), None);
        assert_eq!(empty.resolve_job_id("default-job"), "default-job");
        assert_eq!(empty.resolve_system(), None);

        // 测试 GitHub Actions 默认环境变量回退
        guard.set("GITHUB_RUN_ID", "98765");
        guard.set("GITHUB_REF_NAME", "main");
        guard.set("GITHUB_JOB", "build-linux");
        guard.set("NIXCACHE_SYSTEM", "x86_64-linux");

        assert_eq!(empty.resolve_run_id(), Some(98765));
        assert_eq!(empty.resolve_branch(), Some("main".to_string()));
        assert_eq!(empty.resolve_job_id("default-job"), "build-linux");
        assert_eq!(empty.resolve_system(), Some("x86_64-linux".to_string()));

        // 测试 NIXCACHE_* 优先于 GITHUB_*
        guard.set("NIXCACHE_RUN_ID", "12345");
        guard.set("NIXCACHE_BRANCH", "dev");
        guard.set("NIXCACHE_JOB_ID", "custom-nixcache-job");
        assert_eq!(empty.resolve_run_id(), Some(12345));
        assert_eq!(empty.resolve_branch(), Some("dev".to_string()));
        assert_eq!(empty.resolve_job_id("default-job"), "custom-nixcache-job");

        // 测试 GITHUB_HEAD_REF 回退
        guard.remove("NIXCACHE_BRANCH");
        guard.remove("GITHUB_REF_NAME");
        guard.set("GITHUB_HEAD_REF", "pr-branch");
        assert_eq!(empty.resolve_branch(), Some("pr-branch".to_string()));

        // 测试显式参数优先级高于环境变量
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

        drop(guard);
    }
}
