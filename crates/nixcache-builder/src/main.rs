use clap::Parser;
use std::{error::Error, path::PathBuf, process};

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
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Session(session_args) => match session_args.command {
            SessionCommands::Init(args) => {
                let active_token =
                    resolve_github_token(args.github_token.as_deref(), args.gh_token.as_deref())
                        .await;
                let repo = args.repo();
                let registry = args.registry();
                let run_id = args.run_id();
                let branch = args.branch();
                let port = args.port();
                let listen = args.listen();
                let upstream = args.upstream();
                let session_ttl = args.session_ttl();
                let baseline_ttl = args.baseline_ttl();
                let baseline_tag = args.baseline_tag();
                let signing_key = args
                    .signing_key_file()
                    .map(|p| p.to_string_lossy().to_string());
                let snapshot_path = args.snapshot_path();

                let init_opts = SessionInitOptions {
                    repo: &repo,
                    registry: &registry,
                    run_id,
                    branch,
                    port,
                    listen: &listen,
                    upstream: &upstream,
                    session_ttl,
                    baseline_ttl,
                    baseline_tag: &baseline_tag,
                    github_token: &active_token,
                    signing_key_file: signing_key.as_deref(),
                    snapshot_path: Some(&snapshot_path),
                };

                if let Err(e) = run_session_init(&init_opts).await {
                    eprintln!("Session init failed: {}", e);
                    process::exit(1);
                }
            }
            SessionCommands::Capture(args) => {
                let active_token =
                    resolve_github_token(args.github_token.as_deref(), args.gh_token.as_deref())
                        .await;
                let repo = args.repo();
                let registry = args.registry();
                let run_id = args.run_id().unwrap_or(0);
                let job_id = args.job_id();
                let system = args.system();
                let signing_key = args
                    .signing_key_file()
                    .map(|p| p.to_string_lossy().to_string());
                let output_receipt = args.output_receipt();
                let proxy_url = args.proxy_url();
                let snapshot_before = args.snapshot_before();

                let capture_opts = SessionCaptureOptions {
                    repo: &repo,
                    registry: &registry,
                    run_id,
                    job_id: &job_id,
                    system_opt: system.as_deref(),
                    signing_key_file: signing_key.as_deref(),
                    github_token: &active_token,
                    output_receipt_path: output_receipt.as_deref(),
                    proxy_url: Some(&proxy_url),
                    snapshot_before: Some(&snapshot_before),
                    explicit_paths: &args.paths,
                };

                if let Err(e) = run_session_capture(&capture_opts).await {
                    eprintln!("Session capture failed: {}", e);
                    process::exit(1);
                }
            }
            SessionCommands::Clean(args) => {
                let snapshot_path = args.snapshot_path();
                if let Err(e) = run_session_clean(Some(&snapshot_path)).await {
                    eprintln!("Session clean failed: {}", e);
                    process::exit(1);
                }
            }
        },

        Commands::Build(args) => {
            let active_token =
                resolve_github_token(args.github_token.as_deref(), args.gh_token.as_deref()).await;

            let flake_path = args
                .flake_path()
                .or_else(|| args.config_dir())
                .unwrap_or_else(|| ".".to_string());

            let attributes_str = args.attributes().unwrap_or_default();
            let attributes = attributes_str
                .split([' ', ','])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();

            let build_config = BuildConfig {
                system: args.system(),
                mode: args.mode(),
                flake_path,
                file: args.file(),
                attributes,
            };

            let signing_key = args
                .signing_key_file()
                .map(|p| p.to_string_lossy().to_string());

            let fail_fast = args.fail_fast();

            let system_name = args.system();
            let default_receipt_name = format!(
                "receipt-{}.json",
                system_name.as_deref().unwrap_or("output")
            );
            let receipt_path = args
                .output_receipt()
                .unwrap_or_else(|| PathBuf::from(default_receipt_name));

            let repo = args.repo();
            let registry = args.registry();

            if let Err(e) = run_build_worker(
                &build_config,
                &repo,
                &registry,
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
            let active_token =
                resolve_github_token(args.github_token.as_deref(), args.gh_token.as_deref()).await;
            let run_id = args.run_id();
            let cleanup_session = args.cleanup_session();
            let repo = args.repo();
            let registry = args.registry();
            let target_tag = args.target_tag();
            let receipts_dir = args.receipts_dir();

            let mut paths = args.receipts;
            if let Some(dir) = receipts_dir {
                paths.push(dir);
            }
            paths.extend(args.positional_paths);

            if let Err(e) = run_promote(
                run_id,
                &paths,
                &repo,
                &registry,
                &target_tag,
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
            let active_token =
                resolve_github_token(args.github_token.as_deref(), args.gh_token.as_deref()).await;
            let repo = args.repo();
            let registry = args.registry();
            let retention_days = args.retention_days();

            if let Err(e) = run_gc(
                retention_days,
                args.dry_run,
                &repo,
                &registry,
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
