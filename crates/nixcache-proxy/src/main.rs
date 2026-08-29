use clap::Parser;
use mimalloc::MiMalloc;
use nixcache_cli::{
    AuthTokenArgs, CachePolicyArgs, DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT, OciTargetArgs,
    ServerBindArgs, SessionContextArgs,
};
use nixcache_core::SystemArch;
use nixcache_oci_backend::create_tokio_reqwest_client;
use nixcache_utils::Env;
use std::{error::Error, net::SocketAddr, time::Duration};
use tokio::net::TcpListener;
use tracing::info;

#[cfg(not(unix))]
use std::future;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// Module declarations
mod index;
mod proxy;

use index::{CacheIndex, CascadingProxyConfig, detect_current_system};
use proxy::{AppState, create_router};

#[derive(Parser, Debug)]
#[command(
    name = "nixcache-proxy",
    version,
    about = "OCI-backed Nix Cache Proxy with 4-tier cascading resolution"
)]
struct Args {
    #[command(flatten)]
    oci: OciTargetArgs,

    #[command(flatten)]
    bind: ServerBindArgs,

    #[command(flatten)]
    session: SessionContextArgs,

    #[command(flatten)]
    auth: AuthTokenArgs,

    #[command(flatten)]
    cache: CachePolicyArgs,

    #[arg(long, help = "Target system architecture [env: NIXCACHE_SYSTEM]")]
    system: Option<String>,
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutting down...");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let (repo, registry) = args.oci.resolve("");
    let (listen, port) = args
        .bind
        .resolve(DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT);
    let github_token = args.auth.resolve_token().await;
    let run_id = args.session.resolve_run_id();
    let branch = args.session.resolve_branch();

    let index_dir = args.cache.resolve_index_dir(&repo);
    let upstream_caches = args.cache.resolve_upstream_list();
    let session_ttl = args.cache.resolve_session_ttl();
    let index_ttl = args.cache.resolve_baseline_ttl();
    let baseline_tag = args.cache.resolve_baseline_tag();

    let system_arch_opt = args
        .system
        .as_deref()
        .and_then(Env::non_empty_str)
        .map(|s| s.to_string())
        .or_else(|| args.session.resolve_system());

    let target_system = match system_arch_opt {
        Some(ref s) => SystemArch::from(s.as_str()),
        None => detect_current_system(),
    };

    info!(
        "Starting nixcache-proxy for {}/{} on {}:{}",
        registry, repo, listen, port
    );
    info!(
        "Config: run_id={:?}, branch={:?}, baseline_tag={}, session_ttl={}s, index_ttl={}s, system={:?}",
        run_id, branch, baseline_tag, session_ttl, index_ttl, target_system
    );
    info!("Index cache directory: {:?}", index_dir);
    info!("Upstream caches: {:?}", upstream_caches);

    let oci = create_tokio_reqwest_client(&registry, &repo, &github_token, false);

    let proxy_config = CascadingProxyConfig {
        repo: repo.clone(),
        registry,
        run_id,
        branch_or_pr: branch,
        baseline_tag,
        upstream_caches,
        session_ttl: Duration::from_secs(session_ttl),
        baseline_ttl: Duration::from_secs(index_ttl),
        index_dir,
        target_system,
    };

    let cache_index = CacheIndex::with_config(proxy_config, &github_token);

    let state = AppState {
        repo,
        index: cache_index,
        oci_client: oci,
        http_client: reqwest::Client::new(),
    };

    let app = create_router(state);

    let addr: SocketAddr = format!("{}:{}", listen, port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!("Listening on http://{}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}
