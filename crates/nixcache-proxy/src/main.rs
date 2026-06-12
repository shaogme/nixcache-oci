use std::{env, net::SocketAddr, path::PathBuf};
use tokio::net::TcpListener;
use nixcache_oci::OciClient;
use tracing::info;
use tracing_subscriber;

// Module declarations
mod index;
mod proxy;

use index::CacheIndex;
use proxy::{create_router, AppState};

fn get_index_dir(repo: &str) -> PathBuf {
    if let Ok(explicit) = env::var("NIXCACHE_INDEX_DIR") {
        return PathBuf::from(explicit);
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

    let repo = env::var("NIXCACHE_REPO").unwrap_or_else(|_| "cmspam/mynixcache-oci".to_string());
    let registry = env::var("NIXCACHE_REGISTRY").unwrap_or_else(|_| "ghcr.io".to_string());
    let port_str = env::var("NIXCACHE_PORT").unwrap_or_else(|_| "37515".to_string());
    let port: u16 = port_str.parse().unwrap_or(37515);
    let listen_addr_str = env::var("NIXCACHE_LISTEN").unwrap_or_else(|_| "127.0.0.1".to_string());

    let github_token = env::var("GITHUB_TOKEN")
        .or_else(|_| env::var("GH_TOKEN"))
        .unwrap_or_default();

    let index_dir = get_index_dir(&repo);
    let ttl_str = env::var("NIXCACHE_INDEX_TTL").unwrap_or_else(|_| "300".to_string());
    let ttl: u64 = ttl_str.parse().unwrap_or(300);

    let upstream_str = env::var("NIXCACHE_UPSTREAM").unwrap_or_else(|_| "https://cache.nixos.org".to_string());
    let upstream_caches: Vec<String> = upstream_str
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();

    info!("nixcache-proxy starting on http://{}:{}", listen_addr_str, port);
    info!("  Repo: {}", repo);
    info!("  Upstream: {:?}", upstream_caches);
    info!("  Index TTL: {}s", ttl);
    info!("  Index Dir: {:?}", index_dir);

    let index = CacheIndex::new(&registry, &repo, &github_token, index_dir, ttl);
    let oci_client = OciClient::new(&registry, &repo, &github_token, false);
    let http_client = reqwest::Client::new();

    // Trigger pre-fetch of the index in the background
    let index_clone = index.clone();
    tokio::spawn(async move {
        let _ = index_clone.get_data().await;
    });

    let state = AppState {
        repo,
        index_ttl: ttl,
        upstream_caches,
        index,
        oci_client,
        http_client,
    };

    let router = create_router(state);
    let bind_addr: SocketAddr = format!("{}:{}", listen_addr_str, port).parse()?;
    let listener = TcpListener::bind(&bind_addr).await?;

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}
