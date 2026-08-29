use clap::Parser;
use std::{path::PathBuf, process};

mod cli;
mod env_injector;
mod error;
mod gc;
mod nix;
mod promote;
mod session;
mod summary;
mod worker;

use cli::{Cli, Commands, SessionCommands, resolve_github_token};
use gc::run_gc;
use nix::BuildConfig;
use promote::run_promote;
use session::{
    SessionCaptureOptions, SessionInitOptions, run_session_capture, run_session_clean,
    run_session_init,
};
use worker::run_build_worker;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Session(session_args) => match session_args.command {
            SessionCommands::Init(args) => {
                let active_token = resolve_github_token(args.github_token, args.gh_token).await;
                let run_id = args.run_id.or_else(|| {
                    std::env::var("GITHUB_RUN_ID")
                        .ok()
                        .and_then(|v| v.parse::<u64>().ok())
                });
                let branch = args
                    .branch
                    .or_else(|| std::env::var("GITHUB_REF_NAME").ok())
                    .or_else(|| std::env::var("GITHUB_HEAD_REF").ok());

                let signing_key = args
                    .signing_key_file
                    .map(|p| p.to_string_lossy().to_string());

                let init_opts = SessionInitOptions {
                    repo: &args.repo,
                    registry: &args.registry,
                    run_id,
                    branch,
                    port: args.port,
                    listen: &args.listen,
                    upstream: &args.upstream,
                    session_ttl: args.session_ttl,
                    baseline_ttl: args.baseline_ttl,
                    baseline_tag: &args.baseline_tag,
                    github_token: &active_token,
                    signing_key_file: signing_key.as_deref(),
                    snapshot_path: Some(&args.snapshot_path),
                };

                if let Err(e) = run_session_init(&init_opts).await {
                    eprintln!("Session init failed: {}", e);
                    process::exit(1);
                }
            }
            SessionCommands::Capture(args) => {
                let active_token = resolve_github_token(args.github_token, args.gh_token).await;
                let run_id = args
                    .run_id
                    .or_else(|| {
                        std::env::var("GITHUB_RUN_ID")
                            .ok()
                            .and_then(|v| v.parse::<u64>().ok())
                    })
                    .unwrap_or(0);

                let job_id = args
                    .job_id
                    .or_else(|| std::env::var("GITHUB_JOB").ok())
                    .unwrap_or_else(|| "default-job".to_string());

                let signing_key = args
                    .signing_key_file
                    .map(|p| p.to_string_lossy().to_string());

                let capture_opts = SessionCaptureOptions {
                    repo: &args.repo,
                    registry: &args.registry,
                    run_id,
                    job_id: &job_id,
                    system_opt: args.system.as_deref(),
                    signing_key_file: signing_key.as_deref(),
                    github_token: &active_token,
                    output_receipt_path: args.output_receipt.as_deref(),
                    proxy_url: Some(&args.proxy_url),
                    snapshot_before: Some(&args.snapshot_before),
                    explicit_paths: &args.paths,
                };

                if let Err(e) = run_session_capture(&capture_opts).await {
                    eprintln!("Session capture failed: {}", e);
                    process::exit(1);
                }
            }
            SessionCommands::Clean(args) => {
                if let Err(e) = run_session_clean(Some(&args.snapshot_path)).await {
                    eprintln!("Session clean failed: {}", e);
                    process::exit(1);
                }
            }
        },

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

        Commands::Promote(args) => {
            let active_token = resolve_github_token(args.github_token, args.gh_token).await;
            let run_id = args.run_id.or_else(|| {
                std::env::var("GITHUB_RUN_ID")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
            });

            let mut paths = args.receipts;
            if let Some(dir) = args.receipts_dir {
                paths.push(dir);
            }
            paths.extend(args.positional_paths);

            let cleanup_session = if args.no_cleanup_session {
                false
            } else {
                args.cleanup_session
            };

            if let Err(e) = run_promote(
                run_id,
                &paths,
                &args.repo,
                &args.registry,
                &args.target_tag,
                cleanup_session,
                &active_token,
            )
            .await
            {
                eprintln!("Promote failed: {}", e);
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
    }

    Ok(())
}
