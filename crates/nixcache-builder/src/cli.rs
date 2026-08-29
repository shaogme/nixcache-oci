use crate::nix::BuildMode;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    #[arg(
        long,
        env = "NIXCACHE_REPO",
        default_value = "shaogme/nixcache-oci",
        help = "OCI repository (owner/repo)"
    )]
    pub repo: String,

    #[arg(
        long,
        env = "NIXCACHE_REGISTRY",
        default_value = "ghcr.io",
        help = "OCI registry"
    )]
    pub registry: String,

    #[arg(
        long,
        env = "NIXCACHE_RUN_ID",
        help = "GitHub Actions Workflow Run ID (Tier 1 Session)"
    )]
    pub run_id: Option<u64>,

    #[arg(
        long,
        env = "NIXCACHE_BRANCH",
        help = "Branch name or PR number (Tier 2 Session)"
    )]
    pub branch: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_PORT",
        default_value_t = 37515,
        help = "Port for proxy daemon"
    )]
    pub port: u16,

    #[arg(
        long,
        env = "NIXCACHE_LISTEN",
        default_value = "127.0.0.1",
        help = "Address for proxy daemon"
    )]
    pub listen: String,

    #[arg(
        long,
        env = "NIXCACHE_UPSTREAM",
        default_value = "https://cache.nixos.org",
        help = "Upstream cache URLs"
    )]
    pub upstream: String,

    #[arg(
        long,
        env = "NIXCACHE_SESSION_TTL",
        default_value_t = 10,
        help = "Session index TTL in seconds"
    )]
    pub session_ttl: u64,

    #[arg(
        long,
        env = "NIXCACHE_BASELINE_TTL",
        default_value_t = 300,
        help = "Baseline index TTL in seconds"
    )]
    pub baseline_ttl: u64,

    #[arg(
        long,
        env = "NIXCACHE_BASELINE_TAG",
        default_value = "cache-index",
        help = "Baseline production tag"
    )]
    pub baseline_tag: String,

    #[arg(
        long,
        env = "NIXCACHE_SIGNING_KEY_FILE",
        help = "Path to signing key file"
    )]
    pub signing_key_file: Option<PathBuf>,

    #[arg(
        long,
        env = "NIXCACHE_SNAPSHOT_PATH",
        default_value = "/tmp/nixcache-snapshot-before.txt",
        help = "Path to record baseline store paths snapshot"
    )]
    pub snapshot_path: PathBuf,

    #[arg(long, env = "GITHUB_TOKEN", help = "GitHub token for authentication")]
    pub github_token: Option<String>,

    #[arg(long, env = "GH_TOKEN", help = "GitHub token fallback")]
    pub gh_token: Option<String>,
}

#[derive(Parser, Debug, Clone)]
pub struct SessionCaptureArgs {
    #[arg(
        long,
        env = "NIXCACHE_REPO",
        default_value = "shaogme/nixcache-oci",
        help = "OCI repository (owner/repo)"
    )]
    pub repo: String,

    #[arg(
        long,
        env = "NIXCACHE_REGISTRY",
        default_value = "ghcr.io",
        help = "OCI registry"
    )]
    pub registry: String,

    #[arg(long, env = "NIXCACHE_RUN_ID", help = "GitHub Actions Workflow Run ID")]
    pub run_id: Option<u64>,

    #[arg(long, env = "NIXCACHE_JOB_ID", help = "GitHub Actions Job Identifier")]
    pub job_id: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_SYSTEM",
        help = "Target platform system architecture"
    )]
    pub system: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_SIGNING_KEY_FILE",
        help = "Path to signing key file"
    )]
    pub signing_key_file: Option<PathBuf>,

    #[arg(
        long,
        env = "NIXCACHE_OUTPUT_RECEIPT",
        help = "Output path for the BuildReceipt JSON file"
    )]
    pub output_receipt: Option<PathBuf>,

    #[arg(
        long,
        env = "NIXCACHE_PROXY_URL",
        default_value = "http://127.0.0.1:37515",
        help = "Local proxy URL for hot-registration"
    )]
    pub proxy_url: String,

    #[arg(
        long,
        env = "NIXCACHE_SNAPSHOT_PATH",
        default_value = "/tmp/nixcache-snapshot-before.txt",
        help = "Path to baseline store paths snapshot"
    )]
    pub snapshot_before: PathBuf,

    #[arg(value_name = "PATHS", help = "Explicit store paths to capture")]
    pub paths: Vec<String>,

    #[arg(long, env = "GITHUB_TOKEN", help = "GitHub token for authentication")]
    pub github_token: Option<String>,

    #[arg(long, env = "GH_TOKEN", help = "GitHub token fallback")]
    pub gh_token: Option<String>,
}

#[derive(Parser, Debug, Clone)]
pub struct SessionCleanArgs {
    #[arg(
        long,
        env = "NIXCACHE_SNAPSHOT_PATH",
        default_value = "/tmp/nixcache-snapshot-before.txt",
        help = "Path to store paths snapshot to remove"
    )]
    pub snapshot_path: PathBuf,
}

#[derive(Parser, Debug, Clone)]
pub struct BuildArgs {
    #[arg(
        long,
        env = "NIXCACHE_SYSTEM",
        help = "Target platform system architecture (e.g., x86_64-linux)"
    )]
    pub system: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_MODE",
        default_value = "flake",
        help = "Build mode (flake / non-flake)"
    )]
    pub mode: BuildMode,

    #[arg(
        long,
        env = "NIXCACHE_FLAKE_PATH",
        help = "Path to the flake directory"
    )]
    pub flake_path: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_CONFIG_DIR",
        help = "Fallback path for configuration directory"
    )]
    pub config_dir: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_FILE",
        default_value = "default.nix",
        help = "Target file for non-flake build"
    )]
    pub file: String,

    #[arg(
        long,
        env = "NIXCACHE_ATTRIBUTES",
        help = "Attributes to build (comma or space separated)"
    )]
    pub attributes: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_REPO",
        default_value = "shaogme/nixcache-oci",
        help = "OCI repository (owner/repo)"
    )]
    pub repo: String,

    #[arg(
        long,
        env = "NIXCACHE_REGISTRY",
        default_value = "ghcr.io",
        help = "OCI registry"
    )]
    pub registry: String,

    #[arg(
        long,
        env = "NIXCACHE_SIGNING_KEY_FILE",
        help = "Path to signing key file"
    )]
    pub signing_key_file: Option<PathBuf>,

    #[arg(
        long,
        env = "NIXCACHE_OUTPUT_RECEIPT",
        help = "Output path for the BuildReceipt JSON file"
    )]
    pub output_receipt: Option<PathBuf>,

    #[arg(
        long,
        env = "NIXCACHE_FAIL_FAST",
        default_value = "true",
        default_missing_value = "true",
        num_args = 0..=1,
        help = "Fail fast if self-substituter proxy fails to start (default: true)"
    )]
    pub fail_fast: bool,

    #[arg(
        long,
        help = "Allow self-substituter proxy failure (disables fail-fast)"
    )]
    pub no_fail_fast: bool,

    #[arg(long, env = "GITHUB_TOKEN", help = "GitHub token for authentication")]
    pub github_token: Option<String>,

    #[arg(long, env = "GH_TOKEN", help = "GitHub token fallback")]
    pub gh_token: Option<String>,
}

#[derive(Parser, Debug, Clone)]
pub struct PromoteArgs {
    #[arg(
        long,
        env = "NIXCACHE_RUN_ID",
        help = "GitHub Actions Workflow Run ID to promote"
    )]
    pub run_id: Option<u64>,

    #[arg(
        long,
        env = "NIXCACHE_RECEIPTS_DIR",
        help = "Directory containing BuildReceipt JSON files"
    )]
    pub receipts_dir: Option<PathBuf>,

    #[arg(
        long = "receipt",
        help = "Individual BuildReceipt JSON file(s) to merge"
    )]
    pub receipts: Vec<PathBuf>,

    #[arg(value_name = "PATHS", help = "Receipt files or directories to merge")]
    pub positional_paths: Vec<PathBuf>,

    #[arg(
        long,
        env = "NIXCACHE_REPO",
        default_value = "shaogme/nixcache-oci",
        help = "OCI repository (owner/repo)"
    )]
    pub repo: String,

    #[arg(
        long,
        env = "NIXCACHE_REGISTRY",
        default_value = "ghcr.io",
        help = "OCI registry"
    )]
    pub registry: String,

    #[arg(
        long,
        env = "NIXCACHE_TARGET_TAG",
        default_value = "cache-index",
        help = "Target OCI tag for production baseline"
    )]
    pub target_tag: String,

    #[arg(
        long,
        default_value_t = true,
        help = "Clean up workflow run session tag after promotion"
    )]
    pub cleanup_session: bool,

    #[arg(long, help = "Disable cleaning up workflow run session tag")]
    pub no_cleanup_session: bool,

    #[arg(long, env = "GITHUB_TOKEN", help = "GitHub token for authentication")]
    pub github_token: Option<String>,

    #[arg(long, env = "GH_TOKEN", help = "GitHub token fallback")]
    pub gh_token: Option<String>,
}

#[derive(Parser, Debug, Clone)]
pub struct GcArgs {
    #[arg(
        long,
        default_value_t = 30,
        env = "NIXCACHE_RETENTION_DAYS",
        help = "Retention days for garbage collection"
    )]
    pub retention_days: u64,

    #[arg(long, help = "Dry run mode for garbage collection")]
    pub dry_run: bool,

    #[arg(
        long,
        env = "NIXCACHE_REPO",
        default_value = "shaogme/nixcache-oci",
        help = "OCI repository"
    )]
    pub repo: String,

    #[arg(
        long,
        env = "NIXCACHE_REGISTRY",
        default_value = "ghcr.io",
        help = "OCI registry"
    )]
    pub registry: String,

    #[arg(long, env = "GITHUB_TOKEN", help = "GitHub token for authentication")]
    pub github_token: Option<String>,

    #[arg(long, env = "GH_TOKEN", help = "GitHub token fallback")]
    pub gh_token: Option<String>,
}

pub async fn resolve_github_token(
    github_token: Option<String>,
    gh_token: Option<String>,
) -> String {
    let mut token = github_token.or(gh_token).unwrap_or_default();
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
