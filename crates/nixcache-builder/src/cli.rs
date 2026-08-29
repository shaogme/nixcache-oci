use crate::nix::BuildMode;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|s| {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

pub fn env_path_non_empty(key: &str) -> Option<PathBuf> {
    env_non_empty(key).map(PathBuf::from)
}

pub fn env_u64_non_empty(key: &str) -> Option<u64> {
    env_non_empty(key).and_then(|s| s.parse().ok())
}

#[derive(Parser, Debug)]
#[command(
    name = "nixcache-builder",
    version,
    about = "OCI-backed Nix Binary Cache Coordinator & Builder"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage workflow-run session lifecycle (init, capture, clean)
    Session(SessionCli),

    /// Build Nix targets, push NAR blobs, and output a BuildReceipt
    Build(BuildArgs),

    /// Promote workflow run session into the baseline production cache-index
    Promote(PromoteArgs),

    /// Perform cross-architecture garbage collection on cache-index
    Gc(GcArgs),
}

#[derive(Parser, Debug)]
pub struct SessionCli {
    #[command(subcommand)]
    pub command: SessionCommands,
}

#[derive(Subcommand, Debug)]
pub enum SessionCommands {
    /// Initialize local proxy daemon, configure nix substituters, and record store snapshot
    Init(SessionInitArgs),

    /// Capture newly built paths, upload NAR blobs, update run session with CAS, and hot-register
    Capture(SessionCaptureArgs),

    /// Clean up local session snapshot and temporary files
    Clean(SessionCleanArgs),
}

#[derive(Parser, Debug, Clone)]
pub struct SessionInitArgs {
    #[arg(long, help = "OCI repository (owner/repo) [env: NIXCACHE_REPO]")]
    pub repo: Option<String>,

    #[arg(long, help = "OCI registry [env: NIXCACHE_REGISTRY]")]
    pub registry: Option<String>,

    #[arg(
        long,
        help = "GitHub Actions Workflow Run ID (Tier 1 Session) [env: NIXCACHE_RUN_ID]"
    )]
    pub run_id: Option<u64>,

    #[arg(
        long,
        help = "Branch name or PR number (Tier 2 Session) [env: NIXCACHE_BRANCH]"
    )]
    pub branch: Option<String>,

    #[arg(long, help = "Port for proxy daemon [env: NIXCACHE_PORT]")]
    pub port: Option<u16>,

    #[arg(long, help = "Address for proxy daemon [env: NIXCACHE_LISTEN]")]
    pub listen: Option<String>,

    #[arg(long, help = "Upstream cache URLs [env: NIXCACHE_UPSTREAM]")]
    pub upstream: Option<String>,

    #[arg(
        long,
        help = "Session index TTL in seconds [env: NIXCACHE_SESSION_TTL]"
    )]
    pub session_ttl: Option<u64>,

    #[arg(
        long,
        help = "Baseline index TTL in seconds [env: NIXCACHE_BASELINE_TTL]"
    )]
    pub baseline_ttl: Option<u64>,

    #[arg(long, help = "Baseline production tag [env: NIXCACHE_BASELINE_TAG]")]
    pub baseline_tag: Option<String>,

    #[arg(
        long,
        help = "Path to signing key file [env: NIXCACHE_SIGNING_KEY_FILE]"
    )]
    pub signing_key_file: Option<PathBuf>,

    #[arg(
        long,
        help = "Path to record baseline store paths snapshot [env: NIXCACHE_SNAPSHOT_PATH]"
    )]
    pub snapshot_path: Option<PathBuf>,

    #[arg(long, help = "GitHub token for authentication [env: GITHUB_TOKEN]")]
    pub github_token: Option<String>,

    #[arg(long, help = "GitHub token fallback [env: GH_TOKEN]")]
    pub gh_token: Option<String>,
}

impl SessionInitArgs {
    pub fn repo(&self) -> String {
        self.repo
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REPO"))
            .unwrap_or_else(|| "shaogme/nixcache-oci".to_string())
    }

    pub fn registry(&self) -> String {
        self.registry
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REGISTRY"))
            .unwrap_or_else(|| "ghcr.io".to_string())
    }

    pub fn run_id(&self) -> Option<u64> {
        self.run_id
            .or_else(|| env_u64_non_empty("NIXCACHE_RUN_ID"))
            .or_else(|| env_u64_non_empty("GITHUB_RUN_ID"))
    }

    pub fn branch(&self) -> Option<String> {
        self.branch
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_BRANCH"))
            .or_else(|| env_non_empty("GITHUB_REF_NAME"))
            .or_else(|| env_non_empty("GITHUB_HEAD_REF"))
    }

    pub fn port(&self) -> u16 {
        self.port
            .or_else(|| env_non_empty("NIXCACHE_PORT").and_then(|s| s.parse().ok()))
            .unwrap_or(37515)
    }

    pub fn listen(&self) -> String {
        self.listen
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_LISTEN"))
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }

    pub fn upstream(&self) -> String {
        self.upstream
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_UPSTREAM"))
            .unwrap_or_else(|| "https://cache.nixos.org".to_string())
    }

    pub fn session_ttl(&self) -> u64 {
        self.session_ttl
            .or_else(|| env_u64_non_empty("NIXCACHE_SESSION_TTL"))
            .unwrap_or(10)
    }

    pub fn baseline_ttl(&self) -> u64 {
        self.baseline_ttl
            .or_else(|| env_u64_non_empty("NIXCACHE_BASELINE_TTL"))
            .unwrap_or(300)
    }

    pub fn baseline_tag(&self) -> String {
        self.baseline_tag
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_BASELINE_TAG"))
            .unwrap_or_else(|| "cache-index".to_string())
    }

    pub fn signing_key_file(&self) -> Option<PathBuf> {
        self.signing_key_file
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .or_else(|| env_path_non_empty("NIXCACHE_SIGNING_KEY_FILE"))
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.snapshot_path
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .or_else(|| env_path_non_empty("NIXCACHE_SNAPSHOT_PATH"))
            .unwrap_or_else(|| PathBuf::from("/tmp/nixcache-snapshot-before.txt"))
    }
}

#[derive(Parser, Debug, Clone)]
pub struct SessionCaptureArgs {
    #[arg(long, help = "OCI repository (owner/repo) [env: NIXCACHE_REPO]")]
    pub repo: Option<String>,

    #[arg(long, help = "OCI registry [env: NIXCACHE_REGISTRY]")]
    pub registry: Option<String>,

    #[arg(long, help = "GitHub Actions Workflow Run ID [env: NIXCACHE_RUN_ID]")]
    pub run_id: Option<u64>,

    #[arg(long, help = "GitHub Actions Job Identifier [env: NIXCACHE_JOB_ID]")]
    pub job_id: Option<String>,

    #[arg(
        long,
        help = "Target platform system architecture [env: NIXCACHE_SYSTEM]"
    )]
    pub system: Option<String>,

    #[arg(
        long,
        help = "Path to signing key file [env: NIXCACHE_SIGNING_KEY_FILE]"
    )]
    pub signing_key_file: Option<PathBuf>,

    #[arg(
        long,
        help = "Output path for the BuildReceipt JSON file [env: NIXCACHE_OUTPUT_RECEIPT]"
    )]
    pub output_receipt: Option<PathBuf>,

    #[arg(
        long,
        help = "Local proxy URL for hot-registration [env: NIXCACHE_PROXY_URL]"
    )]
    pub proxy_url: Option<String>,

    #[arg(
        long,
        help = "Path to baseline store paths snapshot [env: NIXCACHE_SNAPSHOT_PATH]"
    )]
    pub snapshot_before: Option<PathBuf>,

    #[arg(value_name = "PATHS", help = "Explicit store paths to capture")]
    pub paths: Vec<String>,

    #[arg(long, help = "GitHub token for authentication [env: GITHUB_TOKEN]")]
    pub github_token: Option<String>,

    #[arg(long, help = "GitHub token fallback [env: GH_TOKEN]")]
    pub gh_token: Option<String>,
}

impl SessionCaptureArgs {
    pub fn repo(&self) -> String {
        self.repo
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REPO"))
            .unwrap_or_else(|| "shaogme/nixcache-oci".to_string())
    }

    pub fn registry(&self) -> String {
        self.registry
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REGISTRY"))
            .unwrap_or_else(|| "ghcr.io".to_string())
    }

    pub fn run_id(&self) -> Option<u64> {
        self.run_id
            .or_else(|| env_u64_non_empty("NIXCACHE_RUN_ID"))
            .or_else(|| env_u64_non_empty("GITHUB_RUN_ID"))
    }

    pub fn job_id(&self) -> String {
        self.job_id
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_JOB_ID"))
            .or_else(|| env_non_empty("GITHUB_JOB"))
            .unwrap_or_else(|| "default-job".to_string())
    }

    pub fn system(&self) -> Option<String> {
        self.system
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_SYSTEM"))
    }

    pub fn signing_key_file(&self) -> Option<PathBuf> {
        self.signing_key_file
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .or_else(|| env_path_non_empty("NIXCACHE_SIGNING_KEY_FILE"))
    }

    pub fn output_receipt(&self) -> Option<PathBuf> {
        self.output_receipt
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .or_else(|| env_path_non_empty("NIXCACHE_OUTPUT_RECEIPT"))
    }

    pub fn proxy_url(&self) -> String {
        self.proxy_url
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_PROXY_URL"))
            .unwrap_or_else(|| "http://127.0.0.1:37515".to_string())
    }

    pub fn snapshot_before(&self) -> PathBuf {
        self.snapshot_before
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .or_else(|| env_path_non_empty("NIXCACHE_SNAPSHOT_PATH"))
            .unwrap_or_else(|| PathBuf::from("/tmp/nixcache-snapshot-before.txt"))
    }
}

#[derive(Parser, Debug, Clone)]
pub struct SessionCleanArgs {
    #[arg(
        long,
        help = "Path to store paths snapshot to remove [env: NIXCACHE_SNAPSHOT_PATH]"
    )]
    pub snapshot_path: Option<PathBuf>,
}

impl SessionCleanArgs {
    pub fn snapshot_path(&self) -> PathBuf {
        self.snapshot_path
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .or_else(|| env_path_non_empty("NIXCACHE_SNAPSHOT_PATH"))
            .unwrap_or_else(|| PathBuf::from("/tmp/nixcache-snapshot-before.txt"))
    }
}

#[derive(Parser, Debug, Clone)]
pub struct BuildArgs {
    #[arg(
        long,
        help = "Target platform system architecture (e.g., x86_64-linux) [env: NIXCACHE_SYSTEM]"
    )]
    pub system: Option<String>,

    #[arg(long, help = "Build mode (flake / non-flake) [env: NIXCACHE_MODE]")]
    pub mode: Option<BuildMode>,

    #[arg(long, help = "Path to the flake directory [env: NIXCACHE_FLAKE_PATH]")]
    pub flake_path: Option<String>,

    #[arg(
        long,
        help = "Fallback path for configuration directory [env: NIXCACHE_CONFIG_DIR]"
    )]
    pub config_dir: Option<String>,

    #[arg(long, help = "Target file for non-flake build [env: NIXCACHE_FILE]")]
    pub file: Option<String>,

    #[arg(
        long,
        help = "Attributes to build (comma or space separated) [env: NIXCACHE_ATTRIBUTES]"
    )]
    pub attributes: Option<String>,

    #[arg(long, help = "OCI repository (owner/repo) [env: NIXCACHE_REPO]")]
    pub repo: Option<String>,

    #[arg(long, help = "OCI registry [env: NIXCACHE_REGISTRY]")]
    pub registry: Option<String>,

    #[arg(
        long,
        help = "Path to signing key file [env: NIXCACHE_SIGNING_KEY_FILE]"
    )]
    pub signing_key_file: Option<PathBuf>,

    #[arg(
        long,
        help = "Output path for the BuildReceipt JSON file [env: NIXCACHE_OUTPUT_RECEIPT]"
    )]
    pub output_receipt: Option<PathBuf>,

    #[arg(
        long,
        default_missing_value = "true",
        num_args = 0..=1,
        help = "Fail fast if self-substituter proxy fails to start [env: NIXCACHE_FAIL_FAST]"
    )]
    pub fail_fast: Option<bool>,

    #[arg(
        long,
        help = "Allow self-substituter proxy failure (disables fail-fast)"
    )]
    pub no_fail_fast: bool,

    #[arg(long, help = "GitHub token for authentication [env: GITHUB_TOKEN]")]
    pub github_token: Option<String>,

    #[arg(long, help = "GitHub token fallback [env: GH_TOKEN]")]
    pub gh_token: Option<String>,
}

impl BuildArgs {
    pub fn system(&self) -> Option<String> {
        self.system
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_SYSTEM"))
    }

    pub fn mode(&self) -> BuildMode {
        if let Some(m) = self.mode {
            return m;
        }
        if let Some(env_m) = env_non_empty("NIXCACHE_MODE")
            && let Ok(m) = env_m.parse::<BuildMode>()
        {
            return m;
        }
        BuildMode::Flake

    }

    pub fn flake_path(&self) -> Option<String> {
        self.flake_path
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_FLAKE_PATH"))
    }

    pub fn config_dir(&self) -> Option<String> {
        self.config_dir
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_CONFIG_DIR"))
    }

    pub fn file(&self) -> String {
        self.file
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_FILE"))
            .unwrap_or_else(|| "default.nix".to_string())
    }

    pub fn attributes(&self) -> Option<String> {
        self.attributes
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_ATTRIBUTES"))
    }

    pub fn repo(&self) -> String {
        self.repo
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REPO"))
            .unwrap_or_else(|| "shaogme/nixcache-oci".to_string())
    }

    pub fn registry(&self) -> String {
        self.registry
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REGISTRY"))
            .unwrap_or_else(|| "ghcr.io".to_string())
    }

    pub fn signing_key_file(&self) -> Option<PathBuf> {
        self.signing_key_file
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .or_else(|| env_path_non_empty("NIXCACHE_SIGNING_KEY_FILE"))
    }

    pub fn output_receipt(&self) -> Option<PathBuf> {
        self.output_receipt
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .or_else(|| env_path_non_empty("NIXCACHE_OUTPUT_RECEIPT"))
    }

    pub fn fail_fast(&self) -> bool {
        if self.no_fail_fast {
            return false;
        }
        if let Some(ff) = self.fail_fast {
            return ff;
        }
        if let Some(env_ff) = env_non_empty("NIXCACHE_FAIL_FAST") {
            return env_ff != "0" && env_ff.to_lowercase() != "false";
        }
        true
    }
}

#[derive(Parser, Debug, Clone)]
pub struct PromoteArgs {
    #[arg(
        long,
        help = "GitHub Actions Workflow Run ID to promote [env: NIXCACHE_RUN_ID]"
    )]
    pub run_id: Option<u64>,

    #[arg(
        long,
        help = "Directory containing BuildReceipt JSON files [env: NIXCACHE_RECEIPTS_DIR]"
    )]
    pub receipts_dir: Option<PathBuf>,

    #[arg(
        long = "receipt",
        help = "Individual BuildReceipt JSON file(s) to merge"
    )]
    pub receipts: Vec<PathBuf>,

    #[arg(value_name = "PATHS", help = "Receipt files or directories to merge")]
    pub positional_paths: Vec<PathBuf>,

    #[arg(long, help = "OCI repository (owner/repo) [env: NIXCACHE_REPO]")]
    pub repo: Option<String>,

    #[arg(long, help = "OCI registry [env: NIXCACHE_REGISTRY]")]
    pub registry: Option<String>,

    #[arg(
        long,
        help = "Target OCI tag for production baseline [env: NIXCACHE_TARGET_TAG]"
    )]
    pub target_tag: Option<String>,

    #[arg(
        long,
        default_missing_value = "true",
        num_args = 0..=1,
        help = "Clean up workflow run session tag after promotion"
    )]
    pub cleanup_session: Option<bool>,

    #[arg(long, help = "Disable cleaning up workflow run session tag")]
    pub no_cleanup_session: bool,

    #[arg(long, help = "GitHub token for authentication [env: GITHUB_TOKEN]")]
    pub github_token: Option<String>,

    #[arg(long, help = "GitHub token fallback [env: GH_TOKEN]")]
    pub gh_token: Option<String>,
}

impl PromoteArgs {
    pub fn run_id(&self) -> Option<u64> {
        self.run_id
            .or_else(|| env_u64_non_empty("NIXCACHE_RUN_ID"))
            .or_else(|| env_u64_non_empty("GITHUB_RUN_ID"))
    }

    pub fn receipts_dir(&self) -> Option<PathBuf> {
        self.receipts_dir
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .or_else(|| env_path_non_empty("NIXCACHE_RECEIPTS_DIR"))
    }

    pub fn repo(&self) -> String {
        self.repo
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REPO"))
            .unwrap_or_else(|| "shaogme/nixcache-oci".to_string())
    }

    pub fn registry(&self) -> String {
        self.registry
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REGISTRY"))
            .unwrap_or_else(|| "ghcr.io".to_string())
    }

    pub fn target_tag(&self) -> String {
        self.target_tag
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_TARGET_TAG"))
            .unwrap_or_else(|| "cache-index".to_string())
    }

    pub fn cleanup_session(&self) -> bool {
        if self.no_cleanup_session {
            return false;
        }
        self.cleanup_session.unwrap_or(true)
    }
}

#[derive(Parser, Debug, Clone)]
pub struct GcArgs {
    #[arg(
        long,
        help = "Retention days for garbage collection [env: NIXCACHE_RETENTION_DAYS]"
    )]
    pub retention_days: Option<u64>,

    #[arg(long, help = "Dry run mode for garbage collection")]
    pub dry_run: bool,

    #[arg(long, help = "OCI repository [env: NIXCACHE_REPO]")]
    pub repo: Option<String>,

    #[arg(long, help = "OCI registry [env: NIXCACHE_REGISTRY]")]
    pub registry: Option<String>,

    #[arg(long, help = "GitHub token for authentication [env: GITHUB_TOKEN]")]
    pub github_token: Option<String>,

    #[arg(long, help = "GitHub token fallback [env: GH_TOKEN]")]
    pub gh_token: Option<String>,
}

impl GcArgs {
    pub fn retention_days(&self) -> u64 {
        self.retention_days
            .or_else(|| env_u64_non_empty("NIXCACHE_RETENTION_DAYS"))
            .unwrap_or(30)
    }

    pub fn repo(&self) -> String {
        self.repo
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REPO"))
            .unwrap_or_else(|| "shaogme/nixcache-oci".to_string())
    }

    pub fn registry(&self) -> String {
        self.registry
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| env_non_empty("NIXCACHE_REGISTRY"))
            .unwrap_or_else(|| "ghcr.io".to_string())
    }
}

pub async fn resolve_github_token(
    github_token: Option<&str>,
    gh_token: Option<&str>,
) -> String {
    let mut token = github_token
        .filter(|s| !s.trim().is_empty())
        .or_else(|| gh_token.filter(|s| !s.trim().is_empty()))
        .map(|s| s.to_string())
        .or_else(|| env_non_empty("GITHUB_TOKEN"))
        .or_else(|| env_non_empty("GH_TOKEN"))
        .unwrap_or_default();
    if token.is_empty()
        && let Ok(output) = tokio::process::Command::new("gh")
            .args(["auth", "token"])
            .output()
            .await
        && output.status.success()
    {
        let token_from_gh = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token_from_gh.is_empty() {
            token = token_from_gh;
        }
    }
    token
}

