use clap::Parser;
use nixcache_oci::OciClient;
use std::{env, net::SocketAddr, path::PathBuf};
use tokio::net::TcpListener;
use tracing::info;

// Module declarations
mod index;
mod proxy;

use index::CacheIndex;
use proxy::{AppState, create_router};

#[derive(Parser, Debug)]
#[command(name = "nixcache-proxy", version, about = "OCI-backed Nix Cache Proxy")]
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
        env = "NIXCACHE_INDEX_DIR",
        help = "Directory to store cache index"
    )]
    index_dir: Option<PathBuf>,

    #[arg(
        long,
        env = "NIXCACHE_INDEX_TTL",
        default_value_t = 300,
        help = "Index TTL in seconds"
    )]
    index_ttl: u64,

    #[arg(
        long,
        env = "NIXCACHE_UPSTREAM",
        default_value = "https://cache.nixos.org",
        help = "Upstream cache URLs"
    )]
    upstream: String,

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
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutting down...");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let github_token = args.github_token.or(args.gh_token).unwrap_or_default();

    let index_dir = get_index_dir(&args.repo, args.index_dir);

    let upstream_caches: Vec<String> = args
        .upstream
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();

    info!(
        "nixcache-proxy starting on http://{}:{}",
        args.listen, args.port
    );
    info!("  Repo: {}", args.repo);
    info!("  Upstream: {:?}", upstream_caches);
    info!("  Index TTL: {}s", args.index_ttl);
    info!("  Index Dir: {:?}", index_dir);

    let index = CacheIndex::new(
        &args.registry,
        &args.repo,
        &github_token,
        index_dir,
        args.index_ttl,
    );
    let oci_client = OciClient::new(&args.registry, &args.repo, &github_token, false);
    let http_client = reqwest::Client::new();

    // Trigger pre-fetch of the index in the background
    let index_clone = index.clone();
    tokio::spawn(async move {
        let _ = index_clone.get_data().await;
    });

    let state = AppState {
        repo: args.repo,
        index_ttl: args.index_ttl,
        upstream_caches,
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
