use clap::Parser;
use nixcache_oci_backend::create_tokio_reqwest_client;
use std::{env, error::Error, net::SocketAddr, path::PathBuf, time::Duration};
use tokio::net::TcpListener;
use tracing::info;

#[cfg(not(unix))]
use std::future;

// Module declarations
mod index;
mod proxy;

use index::{CacheIndex, CascadingProxyConfig};
use proxy::{AppState, create_router};

#[derive(Parser, Debug)]
#[command(
    name = "nixcache-proxy",
    version,
    about = "OCI-backed Nix Cache Proxy with 4-tier cascading resolution"
)]
struct Args {
    #[arg(
        long,
        env = "NIXCACHE_REPO",
        help = "OCI repository (e.g., shaogme/nixcache-oci)"
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
        env = "NIXCACHE_PORT",
        default_value_t = 37515,
        help = "Port to listen on"
    )]
    port: u16,

    #[arg(
        long,
        env = "NIXCACHE_LISTEN",
        default_value = "127.0.0.1",
        help = "Address to listen on"
    )]
    listen: String,

    #[arg(
        long,
        env = "NIXCACHE_RUN_ID",
        help = "GitHub Actions Workflow Run ID (Tier 1 Session)"
    )]
    run_id: Option<u64>,

    #[arg(
        long,
        env = "NIXCACHE_BRANCH",
        help = "Branch name or PR number (Tier 2 Branch/PR Session)"
    )]
    branch: Option<String>,

    #[arg(
        long,
        env = "NIXCACHE_INDEX_DIR",
        help = "Directory to store cache index"
    )]
    index_dir: Option<PathBuf>,

    #[arg(
        long,
        env = "NIXCACHE_SESSION_TTL",
        default_value_t = 10,
        help = "Session index TTL in seconds"
    )]
    session_ttl: u64,

    #[arg(
        long,
        env = "NIXCACHE_INDEX_TTL",
        default_value_t = 300,
        help = "Baseline index TTL in seconds"
    )]
    index_ttl: u64,

    #[arg(
        long,
        env = "NIXCACHE_BASELINE_TAG",
        default_value = "cache-index",
        help = "Baseline production tag"
    )]
    baseline_tag: String,

    #[arg(
        long,
        env = "NIXCACHE_UPSTREAM",
        default_value = "https://cache.nixos.org",
        help = "Upstream cache URLs"
    )]
    upstream: String,

    #[arg(
        long,
        env = "NIXCACHE_SYSTEM",
        help = "Target system platform (e.g. x86_64-linux, defaults to current host)"
    )]
    system: Option<String>,

    #[arg(long, env = "GITHUB_TOKEN", help = "GitHub token for authentication")]
    github_token: Option<String>,

    #[arg(long, env = "GH_TOKEN", help = "GitHub token fallback")]
    gh_token: Option<String>,
}

fn get_index_dir(repo: &str, explicit_dir: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = explicit_dir {
        return dir;
    }
    if let Ok(cache_dir) = env::var("CACHE_DIRECTORY") {
        return PathBuf::from(cache_dir);
    }

    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".cache")
        .join("nixcache-proxy")
        .join(repo.replace('/', "--"))
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

    let github_token = args.github_token.or(args.gh_token).unwrap_or_default();

    let index_dir = get_index_dir(&args.repo, args.index_dir);

    let upstream_caches: Vec<String> = args
        .upstream
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();

    // Fallback run_id from GITHUB_RUN_ID if not explicitly provided
    let run_id = args.run_id.or_else(|| {
        env::var("GITHUB_RUN_ID")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    });

    // Fallback branch from GITHUB_REF_NAME if not explicitly provided
    let branch = args
        .branch
        .or_else(|| env::var("GITHUB_REF_NAME").ok())
        .or_else(|| env::var("GITHUB_HEAD_REF").ok());

    let target_system = args
        .system
        .map(|s| nixcache_core::SystemArch::from(s.as_str()))
        .unwrap_or_else(index::detect_current_system);

    info!(
        "nixcache-proxy starting on http://{}:{}",
        args.listen, args.port
    );
    info!("  Repo: {}", args.repo);
    info!("  System (Target Platform): {}", target_system);
    info!("  Run ID (Tier 1): {:?}", run_id);
    info!("  Branch/PR (Tier 2): {:?}", branch);
    info!("  Baseline Tag (Tier 3): {}", args.baseline_tag);
    info!("  Upstream: {:?}", upstream_caches);
    info!("  Session TTL: {}s", args.session_ttl);
    info!("  Baseline TTL: {}s", args.index_ttl);
    info!("  Index Dir: {:?}", index_dir);

    let config = CascadingProxyConfig {
        repo: args.repo.clone(),
        registry: args.registry.clone(),
        run_id,
        branch_or_pr: branch,
        baseline_tag: args.baseline_tag,
        upstream_caches,
        session_ttl: Duration::from_secs(args.session_ttl),
        baseline_ttl: Duration::from_secs(args.index_ttl),
        index_dir,
        target_system,
    };

    let index = CacheIndex::with_config(config, &github_token);
    let oci_client = create_tokio_reqwest_client(&args.registry, &args.repo, &github_token, false);
    let http_client = reqwest::Client::new();

    // Trigger pre-fetch of the index in the background
    let index_clone = index.clone();
    tokio::spawn(async move {
        let _ = index_clone.get_data().await;
    });

    let state = AppState {
        repo: args.repo,
        index,
        oci_client,
        http_client,
    };

    let router = create_router(state);
    let bind_addr: SocketAddr = format!("{}:{}", args.listen, args.port).parse()?;
    let listener = TcpListener::bind(&bind_addr).await?;

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}
