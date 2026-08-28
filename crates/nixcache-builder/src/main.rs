use clap::{Parser, Subcommand};
use std::{path::PathBuf, process};

mod nix;
mod pipeline;

use nix::{BuildConfig, BuildMode};
use pipeline::{run_all_in_one, run_build_worker, run_gc, run_merge_coordinator};

#[derive(Parser, Debug)]
#[command(
    name = "nixcache-builder",
    version,
    about = "OCI-backed Nix Binary Cache Builder and Multi-Arch Coordinator"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Build Nix targets, push NAR blobs, and output a BuildReceipt
    Build(BuildArgs),

    /// Merge multiple BuildReceipts and publish the global cache-index
    Merge(MergeArgs),

    /// Perform cross-architecture garbage collection on cache-index
    Gc(GcArgs),

    /// Execute single-node build, blob push, and index publish (all-in-one)
    AllInOne(AllInOneArgs),
}

#[derive(Parser, Debug)]
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

#[derive(Parser, Debug)]
pub struct MergeArgs {
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

    #[arg(long, env = "GITHUB_TOKEN", help = "GitHub token for authentication")]
    pub github_token: Option<String>,

    #[arg(long, env = "GH_TOKEN", help = "GitHub token fallback")]
    pub gh_token: Option<String>,
}

#[derive(Parser, Debug)]
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

#[derive(Parser, Debug)]
pub struct AllInOneArgs {
    #[arg(
        long,
        env = "NIXCACHE_SYSTEM",
        help = "Target platform system architecture"
    )]
    pub system: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_MODE",
        default_value = "flake",
        help = "Build mode"
    )]
    pub mode: BuildMode,

    #[arg(
        long,
        env = "NIXCACHE_FLAKE_PATH",
        help = "Path to the flake or config directory"
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
        help = "Target file for build"
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

    #[arg(
        long,
        env = "NIXCACHE_SIGNING_KEY_FILE",
        help = "Path to signing key file"
    )]
    pub signing_key_file: Option<PathBuf>,

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

async fn resolve_github_token(github_token: Option<String>, gh_token: Option<String>) -> String {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Build(args) => {
            let flake_path = args
                .flake_path
                .or(args.config_dir)
                .unwrap_or_else(|| ".".to_string());

            let attributes_str = args.attributes.unwrap_or_default();
            let attributes = attributes_str
                .split([' ', ','])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();

            let build_config = BuildConfig {
                system: args.system.clone(),
                mode: args.mode,
                flake_path,
                file: args.file,
                attributes,
            };

            let signing_key = args
                .signing_key_file
                .map(|p| p.to_string_lossy().to_string());

            let active_token = resolve_github_token(args.github_token, args.gh_token).await;

            let fail_fast = if args.no_fail_fast {
                false
            } else {
                args.fail_fast
            };

            let default_receipt_name = format!(
                "receipt-{}.json",
                args.system.as_deref().unwrap_or("output")
            );
            let receipt_path = args
                .output_receipt
                .unwrap_or_else(|| PathBuf::from(default_receipt_name));

            if let Err(e) = run_build_worker(
                &build_config,
                &args.repo,
                &args.registry,
                signing_key.as_deref(),
                &active_token,
                &receipt_path,
                fail_fast,
            )
            .await
            {
                eprintln!("Worker build failed: {}", e);
                process::exit(1);
            }
        }
        Commands::Merge(args) => {
            let active_token = resolve_github_token(args.github_token, args.gh_token).await;

            let mut paths = args.receipts;
            if let Some(dir) = args.receipts_dir {
                paths.push(dir);
            }
            paths.extend(args.positional_paths);

            if paths.is_empty() {
                // Default search in ./receipts or current directory
                let default_dir = PathBuf::from("./receipts");
                if default_dir.exists() {
                    paths.push(default_dir);
                } else {
                    paths.push(PathBuf::from("."));
                }
            }

            if let Err(e) =
                run_merge_coordinator(&paths, &args.repo, &args.registry, &active_token).await
            {
                eprintln!("Merge coordinator failed: {}", e);
                process::exit(1);
            }
        }

        Commands::Gc(args) => {
            let active_token = resolve_github_token(args.github_token, args.gh_token).await;

            if let Err(e) = run_gc(
                args.retention_days,
                args.dry_run,
                &args.repo,
                &args.registry,
                &active_token,
            )
            .await
            {
                eprintln!("Garbage collection failed: {}", e);
                process::exit(1);
            }
        }
        Commands::AllInOne(args) => {
            let flake_path = args
                .flake_path
                .or(args.config_dir)
                .unwrap_or_else(|| ".".to_string());

            let attributes_str = args.attributes.unwrap_or_default();
            let attributes = attributes_str
                .split([' ', ','])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();

            let build_config = BuildConfig {
                system: args.system,
                mode: args.mode,
                flake_path,
                file: args.file,
                attributes,
            };

            let signing_key = args
                .signing_key_file
                .map(|p| p.to_string_lossy().to_string());

            let active_token = resolve_github_token(args.github_token, args.gh_token).await;

            let fail_fast = if args.no_fail_fast {
                false
            } else {
                args.fail_fast
            };

            if let Err(e) = run_all_in_one(
                &build_config,
                &args.repo,
                &args.registry,
                signing_key.as_deref(),
                &active_token,
                fail_fast,
            )
            .await
            {
                eprintln!("All-in-one pipeline failed: {}", e);
                process::exit(1);
            }
        }
    }

    Ok(())
}
