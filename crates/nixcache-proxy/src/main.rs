use clap::Parser;
use nixcache_core::SystemArch;
use nixcache_oci_backend::create_tokio_reqwest_client;
use std::{error::Error, net::SocketAddr, path::PathBuf, time::Duration};
use tokio::net::TcpListener;
use tracing::info;

#[cfg(not(unix))]
use std::future;

// Module declarations
mod index;
mod proxy;

use index::{CacheIndex, CascadingProxyConfig, detect_current_system};
use proxy::{AppState, create_router};

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|s| {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn env_path_non_empty(key: &str) -> Option<PathBuf> {
    env_non_empty(key).map(PathBuf::from)
}

fn env_u64_non_empty(key: &str) -> Option<u64> {
    env_non_empty(key).and_then(|s| s.parse().ok())
}

#[derive(Parser, Debug)]
#[command(
    name = "nixcache-proxy",
    version,
    about = "OCI-backed Nix Cache Proxy with 4-tier cascading resolution"
)]
struct Args {
    #[arg(long, help = "OCI repository (e.g., shaogme/nixcache-oci) [env: NIXCACHE_REPO]")]
    repo: Option<String>,

    #[arg(long, help = "OCI registry [env: NIXCACHE_REGISTRY]")]
    registry: Option<String>,

    #[arg(long, help = "Port to listen on [env: NIXCACHE_PORT]")]
    port: Option<u16>,

    #[arg(long, help = "Address to listen on [env: NIXCACHE_LISTEN]")]
    listen: Option<String>,

    #[arg(long, help = "GitHub Actions Workflow Run ID (Tier 1 Session) [env: NIXCACHE_RUN_ID]")]
    run_id: Option<u64>,

    #[arg(long, help = "Branch name or PR number (Tier 2 Branch/PR Session) [env: NIXCACHE_BRANCH]")]
    branch: Option<String>,

    #[arg(long, help = "Directory to store cache index [env: NIXCACHE_INDEX_DIR]")]
    index_dir: Option<PathBuf>,

    #[arg(long, help = "Session index TTL in seconds [env: NIXCACHE_SESSION_TTL]")]
    session_ttl: Option<u64>,

    #[arg(long, help = "Baseline index TTL in seconds [env: NIXCACHE_INDEX_TTL]")]
    index_ttl: Option<u64>,

    #[arg(long, help = "Baseline production tag [env: NIXCACHE_BASELINE_TAG]")]
    baseline_tag: Option<String>,

    #[arg(long, help = "Upstream cache URLs [env: NIXCACHE_UPSTREAM]")]
    upstream: Option<String>,

    #[arg(long, help = "Target system architecture [env: NIXCACHE_SYSTEM]")]
    system: Option<String>,

    #[arg(long, help = "GitHub token for authentication [env: GITHUB_TOKEN]")]
    github_token: Option<String>,

    #[arg(long, help = "GitHub token fallback [env: GH_TOKEN]")]
    gh_token: Option<String>,
}

fn get_index_dir(repo: &str, explicit_dir: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = explicit_dir {
        return dir;
    }
    if let Some(cache_dir) = env_non_empty("CACHE_DIRECTORY") {
        return PathBuf::from(cache_dir);
    }

    let home = env_non_empty("HOME").unwrap_or_else(|| ".".to_string());
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

    let repo = args
        .repo
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env_non_empty("NIXCACHE_REPO"))
        .unwrap_or_default();

    let registry = args
        .registry
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env_non_empty("NIXCACHE_REGISTRY"))
        .unwrap_or_else(|| "ghcr.io".to_string());

    let port = args
        .port
        .or_else(|| env_non_empty("NIXCACHE_PORT").and_then(|s| s.parse().ok()))
        .unwrap_or(37515);

    let listen = args
        .listen
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env_non_empty("NIXCACHE_LISTEN"))
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let github_token = args
        .github_token
        .filter(|s| !s.trim().is_empty())
        .or_else(|| args.gh_token.filter(|s| !s.trim().is_empty()))
        .or_else(|| env_non_empty("GITHUB_TOKEN"))
        .or_else(|| env_non_empty("GH_TOKEN"))
        .unwrap_or_default();

    let index_dir_opt = args
        .index_dir
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| env_path_non_empty("NIXCACHE_INDEX_DIR"));
    let index_dir = get_index_dir(&repo, index_dir_opt);

    let upstream_str = args
        .upstream
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env_non_empty("NIXCACHE_UPSTREAM"))
        .unwrap_or_else(|| "https://cache.nixos.org".to_string());

    let upstream_caches: Vec<String> = upstream_str
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();

    let run_id = args
        .run_id
        .or_else(|| env_u64_non_empty("NIXCACHE_RUN_ID"))
        .or_else(|| env_u64_non_empty("GITHUB_RUN_ID"));

    let branch = args
        .branch
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env_non_empty("NIXCACHE_BRANCH"))
        .or_else(|| env_non_empty("GITHUB_REF_NAME"))
        .or_else(|| env_non_empty("GITHUB_HEAD_REF"));

    let session_ttl = args
        .session_ttl
        .or_else(|| env_u64_non_empty("NIXCACHE_SESSION_TTL"))
        .unwrap_or(10);

    let index_ttl = args
        .index_ttl
        .or_else(|| env_u64_non_empty("NIXCACHE_INDEX_TTL"))
        .unwrap_or(300);

    let baseline_tag = args
        .baseline_tag
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env_non_empty("NIXCACHE_BASELINE_TAG"))
        .unwrap_or_else(|| "cache-index".to_string());

    let system_arch_opt = args
        .system
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env_non_empty("NIXCACHE_SYSTEM"));

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
