use std::{env, process};
use tracing_subscriber;

mod nix;
mod pipeline;

use pipeline::{run_gc, run_pipeline};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = env::args().collect();
    let mut is_gc = false;
    let mut retention_days = 30u64;
    let mut dry_run = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--gc" | "--garbage-collect" => {
                is_gc = true;
            }
            "--retention-days" => {
                if i + 1 < args.len() {
                    retention_days = args[i + 1].parse().unwrap_or(30);
                    i += 1;
                }
            }
            "--dry-run" => {
                dry_run = true;
            }
            _ => {}
        }
        i += 1;
    }

    let repo = env::var("NIXCACHE_REPO").unwrap_or_else(|_| "cmspam/nixcache-oci".to_string());
    let registry = env::var("NIXCACHE_REGISTRY").unwrap_or_else(|_| "ghcr.io".to_string());
    let signing_key = env::var("NIXCACHE_SIGNING_KEY_FILE").ok();
    let config_dir = env::var("NIXCACHE_CONFIG_DIR").unwrap_or_else(|_| "config".to_string());

    let mut active_token = env::var("GITHUB_TOKEN")
        .or_else(|_| env::var("GH_TOKEN"))
        .unwrap_or_default();

    if active_token.is_empty() {
        // Attempt to run `gh auth token` to get the token, like the bash script did
        if let Ok(output) = tokio::process::Command::new("gh").args(["auth", "token"]).output().await {
            if output.status.success() {
                let token_from_gh = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !token_from_gh.is_empty() {
                    active_token = token_from_gh;
                }
            }
        }
    }

    if is_gc {
        if let Err(e) = run_gc(
            retention_days,
            dry_run,
            &config_dir,
            &repo,
            &registry,
            &active_token,
        ).await {
            eprintln!("Garbage collection failed: {}", e);
            process::exit(1);
        }
    } else {
        if let Err(e) = run_pipeline(
            &config_dir,
            &repo,
            &registry,
            signing_key.as_deref(),
            &active_token,
        ).await {
            eprintln!("Pipeline failed: {}", e);
            process::exit(1);
        }
    }

    Ok(())
}
