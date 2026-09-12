use clap::Parser;
use mimalloc::MiMalloc;
use nixcache_builder::{
    cli::{Cli, Commands, SessionCommands},
    error::BuilderError,
    gc::run_gc,
    list::run_list,
    nix::BuildConfig,
    promote::run_promote,
    purge::run_purge,
    session::{
        SessionCaptureOptions, SessionInitOptions, run_session_capture, run_session_clean,
        run_session_init,
    },
    worker::{BuildWorkerOptions, run_build_worker},
};
use nixcache_cli::{
    CachePolicyArgs, DEFAULT_NIXCACHE_REPO, DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT,
};
use std::{path::Path, process};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> Result<(), BuilderError> {
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
                let upstream = args.cache.resolve_upstream();
                let baseline_ttl = args.cache.resolve_baseline_ttl();
                let baseline_tag = args.cache.resolve_baseline_tag();
                let signing_key = args.signing.resolve_signing_key_str();
                let snapshot_path = args.cache.resolve_snapshot_path();
                let registry_username = args.auth.resolve_registry_username();

                let init_opts = SessionInitOptions {
                    repo: &repo,
                    registry: &registry,
                    port,
                    listen: &listen,
                    upstream: &upstream,
                    baseline_ttl,
                    baseline_tag: &baseline_tag,
                    github_token: &active_token,
                    registry_username: registry_username.as_deref(),
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
                let credentials = args.auth.credentials_from_token(active_token);
                let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);
                let run_id = args.session.resolve_run_id().unwrap_or(0);
                let job_id = args.session.resolve_job_id("default-job");
                let system = args.session.resolve_system();
                let signing_key = args.signing.resolve_signing_key_str();
                let output_receipt = args.resolve_output_receipt();
                let proxy_url = args.resolve_proxy_url();
                let snapshot_before = args.resolve_snapshot_before();
                let export_concurrency = args.resolve_export_concurrency();
                let out_link = args.resolve_out_link();
                let targets = args.resolve_targets();
                let capture_mode = args.resolve_capture_mode();
                let strict_closure = args.resolve_strict_closure();
                let baseline_tag = CachePolicyArgs::default().resolve_baseline_tag();
                let workspace_root = Path::new(".");

                let capture_opts = SessionCaptureOptions {
                    repo: &repo,
                    registry: &registry,
                    run_id,
                    job_id: &job_id,
                    system_opt: system.as_deref(),
                    signing_key_file: signing_key.as_deref(),
                    credentials,
                    baseline_tag: &baseline_tag,
                    output_receipt_path: output_receipt.as_deref(),
                    proxy_url: Some(&proxy_url),
                    snapshot_before: Some(&snapshot_before),
                    export_concurrency,
                    explicit_paths: &args.paths,
                    out_link_pattern: out_link.as_deref(),
                    targets_expr: targets.as_deref(),
                    capture_mode,
                    strict_closure,
                    workspace_root,
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
            let credentials = args.auth.credentials_from_token(active_token.clone());
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);
            let signing_key = args.signing.resolve_signing_key_str();
            let system_name = args.resolve_system();
            let mode = args.resolve_mode();
            let flake_path = args.resolve_flake_path();
            let file = args.resolve_file();
            let attributes = args.resolve_attributes();
            let strict = args.resolve_strict();
            let receipt_path = args.resolve_output_receipt(system_name.as_deref());
            let export_concurrency = args.resolve_export_concurrency();
            let baseline_tag = CachePolicyArgs::default().resolve_baseline_tag();

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
                credentials,
                baseline_tag: &baseline_tag,
                output_receipt_path: &receipt_path,
                strict,
                export_concurrency,
            };

            if let Err(e) = run_build_worker(&worker_opts).await {
                eprintln!("Worker build failed: {}", e);
                process::exit(1);
            }
        }

        Commands::Promote(args) => {
            let credentials = args.auth.resolve_credentials().await;
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);
            let target_tag = args.resolve_target_tag();
            let paths = args.resolve_receipt_paths();

            if let Err(e) = run_promote(&paths, &repo, &registry, &target_tag, credentials).await {
                eprintln!("Promote failed: {}", e);
                process::exit(1);
            }
        }

        Commands::List(args) => {
            let credentials = args.auth.resolve_credentials().await;
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);

            if let Err(e) = run_list(&args, &repo, &registry, credentials).await {
                eprintln!("Cache list failed: {}", e);
                process::exit(1);
            }
        }

        Commands::Gc(args) => {
            let credentials = args.auth.resolve_credentials().await;
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);

            if let Err(e) = run_gc(&args, &repo, &registry, credentials).await {
                eprintln!("Garbage collection failed: {}", e);
                process::exit(1);
            }
        }

        Commands::Purge(args) => {
            let credentials = args.auth.resolve_credentials().await;
            let (repo, registry) = args.oci.resolve(DEFAULT_NIXCACHE_REPO);

            if let Err(e) = run_purge(&args, &repo, &registry, credentials).await {
                eprintln!("Purge failed: {}", e);
                process::exit(1);
            }
        }
    }

    Ok(())
}
