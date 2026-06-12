use clap::Parser;
use std::{path::PathBuf, process};

mod nix;
mod pipeline;

use nix::BuildMode;
use pipeline::{run_gc, run_pipeline};

#[derive(Parser, Debug)]
#[command(
    name = "nixcache-builder",
    version,
    about = "OCI-backed Nix Cache Builder"
)]
struct Args {
    #[arg(long, short, aliases = ["garbage-collect"], help = "Run garbage collection")]
    gc: bool,

    #[arg(
        long,
        default_value_t = 30,
        help = "Retention days for garbage collection"
    )]
    retention_days: u64,

    #[arg(long, help = "Dry run mode for garbage collection")]
    dry_run: bool,

    #[arg(
        long,
        env = "NIXCACHE_REPO",
        default_value = "shaogme/nixcache-oci",
        help = "OCI repository"
    )]
    repo: String,

    #[arg(
        long,
        env = "NIXCACHE_REGISTRY",
        default_value = "ghcr.io",
        help = "OCI registry"
    )]
    registry: String,

    #[arg(
        long,
        env = "NIXCACHE_SIGNING_KEY_FILE",
        help = "Path to signing key file"
    )]
    signing_key_file: Option<PathBuf>,

    #[arg(
        long,
        env = "NIXCACHE_MODE",
        default_value = "flake",
        help = "Build mode"
    )]
    mode: BuildMode,

    #[arg(long, env = "NIXCACHE_FLAKE_PATH", aliases = ["config-dir"], help = "Path to the flake or config directory")]
    flake_path: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_CONFIG_DIR",
        help = "Fallback path for configuration directory"
    )]
    config_dir: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_FILE",
        default_value = "default.nix",
        help = "Target file for build"
    )]
    file: String,

    #[arg(
        long,
        env = "NIXCACHE_ATTRIBUTES",
        help = "Attributes to build (comma or space separated)"
    )]
    attributes: Option<String>,

    #[arg(long, env = "GITHUB_TOKEN", help = "GitHub token for authentication")]
    github_token: Option<String>,

    #[arg(long, env = "GH_TOKEN", help = "GitHub token fallback")]
    gh_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let flake_path = args
        .flake_path
        .or(args.config_dir)
        .unwrap_or_else(|| ".".to_string());

    let attributes_str = args.attributes.unwrap_or_default();
    let attributes = attributes_str
        .split(|c: char| c == ' ' || c == ',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    let build_config = nix::BuildConfig {
        mode: args.mode,
        flake_path,
        file: args.file,
        attributes,
    };

    let signing_key = args
        .signing_key_file
        .map(|p| p.to_string_lossy().to_string());

    let mut active_token = args.github_token.or(args.gh_token).unwrap_or_default();

    if active_token.is_empty() {
        // Attempt to run `gh auth token` to get the token, like the bash script did
        if let Ok(output) = tokio::process::Command::new("gh")
            .args(["auth", "token"])
            .output()
            .await
            && output.status.success()
        {
            let token_from_gh = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token_from_gh.is_empty() {
                active_token = token_from_gh;
            }
        }
    }

    if args.gc {
        if let Err(e) = run_gc(
            args.retention_days,
            args.dry_run,
            &build_config,
            &args.repo,
            &args.registry,
            &active_token,
        )
        .await
        {
            eprintln!("Garbage collection failed: {}", e);
            process::exit(1);
        }
    } else {
        if let Err(e) = run_pipeline(
            &build_config,
            &args.repo,
            &args.registry,
            signing_key.as_deref(),
            &active_token,
        )
        .await
        {
            eprintln!("Pipeline failed: {}", e);
            process::exit(1);
        }
    }

    Ok(())
}
