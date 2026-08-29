use crate::nix::BuildMode;
use clap::{Parser, Subcommand};
use nixcache_cli::{
    AuthTokenArgs, CachePolicyArgs, OciTargetArgs, ServerBindArgs, SessionContextArgs,
    SigningKeyArgs,
};
use nixcache_utils::Env;
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

#[derive(Parser, Debug, Clone, Default)]
pub struct SessionInitArgs {
    #[command(flatten)]
    pub oci: OciTargetArgs,

    #[command(flatten)]
    pub bind: ServerBindArgs,

    #[command(flatten)]
    pub session: SessionContextArgs,

    #[command(flatten)]
    pub auth: AuthTokenArgs,

    #[command(flatten)]
    pub signing: SigningKeyArgs,

    #[command(flatten)]
    pub cache: CachePolicyArgs,
}

#[derive(Parser, Debug, Clone, Default)]
pub struct SessionCaptureArgs {
    #[command(flatten)]
    pub oci: OciTargetArgs,

    #[command(flatten)]
    pub session: SessionContextArgs,

    #[command(flatten)]
    pub auth: AuthTokenArgs,

    #[command(flatten)]
    pub signing: SigningKeyArgs,

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
}

impl SessionCaptureArgs {
    pub fn resolve_proxy_url(&self) -> String {
        self.proxy_url
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_PROXY_URL"))
            .unwrap_or_else(|| "http://127.0.0.1:37515".to_string())
    }

    pub fn resolve_output_receipt(&self) -> Option<PathBuf> {
        self.output_receipt
            .as_deref()
            .and_then(Env::non_empty_path)
            .map(PathBuf::from)
            .or_else(|| Env::get_path("NIXCACHE_OUTPUT_RECEIPT"))
    }

    pub fn resolve_snapshot_before(&self) -> PathBuf {
        self.snapshot_before
            .as_deref()
            .and_then(Env::non_empty_path)
            .map(PathBuf::from)
            .or_else(|| Env::get_path("NIXCACHE_SNAPSHOT_PATH"))
            .unwrap_or_else(|| PathBuf::from("/tmp/nixcache-snapshot-before.txt"))
    }
}

#[derive(Parser, Debug, Clone, Default)]
pub struct SessionCleanArgs {
    #[arg(
        long,
        help = "Path to store paths snapshot to remove [env: NIXCACHE_SNAPSHOT_PATH]"
    )]
    pub snapshot_path: Option<PathBuf>,
}

impl SessionCleanArgs {
    pub fn resolve_snapshot_path(&self) -> PathBuf {
        self.snapshot_path
            .as_deref()
            .and_then(Env::non_empty_path)
            .map(PathBuf::from)
            .or_else(|| Env::get_path("NIXCACHE_SNAPSHOT_PATH"))
            .unwrap_or_else(|| PathBuf::from("/tmp/nixcache-snapshot-before.txt"))
    }
}

#[derive(Parser, Debug, Clone, Default)]
pub struct BuildArgs {
    #[command(flatten)]
    pub oci: OciTargetArgs,

    #[command(flatten)]
    pub auth: AuthTokenArgs,

    #[command(flatten)]
    pub signing: SigningKeyArgs,

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
}

impl BuildArgs {
    pub fn resolve_system(&self) -> Option<String> {
        self.system
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_SYSTEM"))
    }

    pub fn resolve_mode(&self) -> BuildMode {
        self.mode
            .or_else(|| Env::parse("NIXCACHE_MODE"))
            .unwrap_or(BuildMode::Flake)
    }

    pub fn resolve_flake_path(&self) -> String {
        self.flake_path
            .as_deref()
            .and_then(Env::non_empty_str)
            .or_else(|| self.config_dir.as_deref().and_then(Env::non_empty_str))
            .map(|s| s.to_string())
            .or_else(|| Env::get_first(&["NIXCACHE_FLAKE_PATH", "NIXCACHE_CONFIG_DIR"]))
            .unwrap_or_else(|| ".".to_string())
    }

    pub fn resolve_file(&self) -> String {
        self.file
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_FILE"))
            .unwrap_or_else(|| "default.nix".to_string())
    }

    pub fn resolve_attributes(&self) -> Vec<String> {
        let attr_str = self
            .attributes
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_ATTRIBUTES"))
            .unwrap_or_default();

        attr_str
            .split([' ', ','])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    pub fn resolve_output_receipt(&self, system_name: Option<&str>) -> PathBuf {
        self.output_receipt
            .as_deref()
            .and_then(Env::non_empty_path)
            .map(PathBuf::from)
            .or_else(|| Env::get_path("NIXCACHE_OUTPUT_RECEIPT"))
            .unwrap_or_else(|| {
                let default_name = format!("receipt-{}.json", system_name.unwrap_or("output"));
                PathBuf::from(default_name)
            })
    }

    pub fn resolve_fail_fast(&self) -> bool {
        if self.no_fail_fast {
            return false;
        }
        if let Some(ff) = self.fail_fast {
            return ff;
        }
        Env::get_bool("NIXCACHE_FAIL_FAST").unwrap_or(true)
    }
}

#[derive(Parser, Debug, Clone, Default)]
pub struct PromoteArgs {
    #[command(flatten)]
    pub oci: OciTargetArgs,

    #[command(flatten)]
    pub auth: AuthTokenArgs,

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
}

impl PromoteArgs {
    pub fn resolve_run_id(&self) -> Option<u64> {
        self.run_id
            .or_else(|| Env::parse_first(&["NIXCACHE_RUN_ID", "GITHUB_RUN_ID"]))
    }

    pub fn resolve_target_tag(&self) -> String {
        self.target_tag
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_TARGET_TAG"))
            .unwrap_or_else(|| "cache-index".to_string())
    }

    pub fn resolve_receipt_paths(&self) -> Vec<PathBuf> {
        let receipts_dir = self
            .receipts_dir
            .as_deref()
            .and_then(Env::non_empty_path)
            .map(PathBuf::from)
            .or_else(|| Env::get_path("NIXCACHE_RECEIPTS_DIR"));

        let mut paths = self.receipts.clone();
        if let Some(dir) = receipts_dir {
            paths.push(dir);
        }
        paths.extend(self.positional_paths.clone());
        paths
    }

    pub fn resolve_cleanup_session(&self) -> bool {
        if self.no_cleanup_session {
            return false;
        }
        self.cleanup_session.unwrap_or(true)
    }
}

#[derive(Parser, Debug, Clone, Default)]
pub struct GcArgs {
    #[command(flatten)]
    pub oci: OciTargetArgs,

    #[command(flatten)]
    pub auth: AuthTokenArgs,

    #[arg(
        long,
        help = "Retention days for garbage collection [env: NIXCACHE_RETENTION_DAYS]"
    )]
    pub retention_days: Option<u64>,

    #[arg(long, help = "Dry run mode for garbage collection")]
    pub dry_run: bool,
}

impl GcArgs {
    pub fn resolve_retention_days(&self) -> u64 {
        self.retention_days
            .or_else(|| Env::parse("NIXCACHE_RETENTION_DAYS"))
            .unwrap_or(30)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_clap_debug_assert() {
        Cli::command().debug_assert();
    }
}
