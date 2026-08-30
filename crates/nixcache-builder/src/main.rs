use clap::Parser;
use mimalloc::MiMalloc;
use nixcache_cli::{DEFAULT_NIXCACHE_REPO, DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT};
use std::{error::Error, process};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

mod cli;
mod env_injector;
mod error;
mod gc;
mod list;
mod nix;
mod promote;
mod purge;
mod session;
mod summary;
mod worker;

use cli::{Cli, Commands, SessionCommands};
use gc::run_gc;
use list::run_list;
use nix::BuildConfig;
use promote::run_promote;
use purge::run_purge;
use session::{
    SessionCaptureOptions, SessionInitOptions, run_session_capture, run_session_clean,
    run_session_init,
};
use worker::{BuildWorkerOptions, run_build_worker};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Session(session_args) => match session_args.command {
            SessionCommands::Init(args) => {
                let active_token = args.auth.resolve_token().await;
                let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);
                let (listen, port) = args
                    .bind
                    .resolve(DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT);
                let run_id = args.session.resolve_run_id();
                let branch = args.session.resolve_branch();
                let upstream = args.cache.resolve_upstream();
                let session_ttl = args.cache.resolve_session_ttl();
                let baseline_ttl = args.cache.resolve_baseline_ttl();
                let baseline_tag = args.cache.resolve_baseline_tag();
                let signing_key = args.signing.resolve_signing_key_str();
                let snapshot_path = args.cache.resolve_snapshot_path();

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
                let active_token = args.auth.resolve_token().await;
                let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);
                let run_id = args.session.resolve_run_id().unwrap_or(0);
                let job_id = args.session.resolve_job_id("default-job");
                let system = args.session.resolve_system();
                let signing_key = args.signing.resolve_signing_key_str();
                let output_receipt = args.resolve_output_receipt();
                let proxy_url = args.resolve_proxy_url();
                let snapshot_before = args.resolve_snapshot_before();
                let export_concurrency = args.resolve_export_concurrency();

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
                    export_concurrency,
                    explicit_paths: &args.paths,
                };

                if let Err(e) = run_session_capture(&capture_opts).await {
                    eprintln!("Session capture failed: {}", e);
                    process::exit(1);
                }
            }
            SessionCommands::Clean(args) => {
                let snapshot_path = args.resolve_snapshot_path();
                if let Err(e) = run_session_clean(Some(&snapshot_path)).await {
                    eprintln!("Session clean failed: {}", e);
                    process::exit(1);
                }
            }
        },

        Commands::Build(args) => {
            let active_token = args.auth.resolve_token().await;
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);
            let signing_key = args.signing.resolve_signing_key_str();
            let system_name = args.resolve_system();
            let mode = args.resolve_mode();
            let flake_path = args.resolve_flake_path();
            let file = args.resolve_file();
            let attributes = args.resolve_attributes();
            let fail_fast = args.resolve_fail_fast();
            let receipt_path = args.resolve_output_receipt(system_name.as_deref());
            let export_concurrency = args.resolve_export_concurrency();

            let build_config = BuildConfig {
                system: system_name,
                mode,
                flake_path,
                file,
                attributes,
            };

            let worker_opts = BuildWorkerOptions {
                build_config: &build_config,
                repo: &repo,
                registry: &registry,
                signing_key_file: signing_key.as_deref(),
                github_token: &active_token,
                output_receipt_path: &receipt_path,
                fail_fast,
                export_concurrency,
            };

            if let Err(e) = run_build_worker(&worker_opts).await {
                eprintln!("Worker build failed: {}", e);
                process::exit(1);
            }
        }

        Commands::Promote(args) => {
            let active_token = args.auth.resolve_token().await;
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);
            let run_id = args.resolve_run_id();
            let target_tag = args.resolve_target_tag();
            let cleanup_session = args.resolve_cleanup_session();
            let paths = args.resolve_receipt_paths();

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

        Commands::List(args) => {
            let active_token = args.auth.resolve_token().await;
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);

            if let Err(e) = run_list(&args, &repo, &registry, &active_token).await {
                eprintln!("Cache list failed: {}", e);
                process::exit(1);
            }
        }

        Commands::Gc(args) => {
            let active_token = args.auth.resolve_token().await;
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);

            if let Err(e) = run_gc(&args, &repo, &registry, &active_token).await {
                eprintln!("Garbage collection failed: {}", e);
                process::exit(1);
            }
        }

        Commands::Purge(args) => {
            let active_token = args.auth.resolve_token().await;
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);

            if let Err(e) = run_purge(&args, &repo, &registry, &active_token).await {
                eprintln!("Purge failed: {}", e);
                process::exit(1);
            }
        }
    }

    Ok(())
}
