use nixcache_oci::OciClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, sync::RwLock};
use tracing::{error, info};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IndexEntry {
    pub name: String,
    pub narinfo: String,
    pub nar_digest: String,
    pub nar_size: u64,
    pub added: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CacheIndexData {
    pub version: u32,
    pub repo: String,
    pub registry: String,
    pub image: String,
    pub generated: String,
    pub public_key: String,
    pub entries: HashMap<String, IndexEntry>,
    pub gc_roots: Vec<String>,
}

#[derive(Clone)]
pub struct CacheIndex {
    index_dir: PathBuf,
    ttl: Duration,
    oci_client: OciClient,
    data: Arc<RwLock<CacheIndexData>>,
    last_refresh: Arc<RwLock<Option<Instant>>>,
}

impl CacheIndex {
    pub fn new(
        registry: &str,
        repo: &str,
        github_token: &str,
        index_dir: PathBuf,
        ttl_seconds: u64,
    ) -> Self {
        let oci_client = OciClient::new(registry, repo, github_token, false);
        Self {
            index_dir,
            ttl: Duration::from_secs(ttl_seconds),
            oci_client,
            data: Arc::new(RwLock::new(CacheIndexData::default())),
            last_refresh: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn get_data(&self) -> CacheIndexData {
        let should_refresh = {
            let last = self.last_refresh.read().await;
            match *last {
                None => true,
                Some(inst) => inst.elapsed() > self.ttl,
            }
        };

        if should_refresh {
            // Drop locks before calling refresh to avoid deadlock
            let _ = self.refresh().await;
        }

        let current = self.data.read().await;
        current.clone()
    }

    pub async fn force_refresh(&self) -> Result<usize, String> {
        self.refresh().await?;
        let current = self.data.read().await;
        Ok(current.entries.len())
    }

    async fn refresh(&self) -> Result<(), String> {
        let mut last_ref = self.last_refresh.write().await;
        // Re-check after acquiring write lock
        if let Some(inst) = *last_ref
            && inst.elapsed() < self.ttl
        {
            return Ok(());
        }

        info!("[nixcache-proxy] Refreshing cache index from GHCR...");
        let mut refresh_ok = false;

        match self.oci_client.get_manifest("cache-index").await {
            Ok(Some(manifest_json)) => {
                if let Ok(manifest) = serde_json::from_str::<Value>(&manifest_json)
                    && let Some(layers) = manifest.get("layers").and_then(|l| l.as_array())
                    && !layers.is_empty()
                    && let Some(digest) = layers[0].get("digest").and_then(|d| d.as_str())
                {
                    match self.oci_client.get_blob(digest).await {
                        Ok(blob_bytes) => {
                            if let Ok(index_data) =
                                serde_json::from_slice::<CacheIndexData>(&blob_bytes)
                            {
                                let mut current_data = self.data.write().await;
                                *current_data = index_data;
                                refresh_ok = true;

                                // Save backup file
                                let file_path = self.index_dir.join("cache-index.json");
                                if let Some(parent) = file_path.parent() {
                                    let _ = fs::create_dir_all(parent).await;
                                }
                                if let Err(e) = fs::write(&file_path, &blob_bytes).await {
                                    error!("[nixcache-proxy] Failed to write backup index: {}", e);
                                } else {
                                    info!(
                                        "[nixcache-proxy] Backup cache index saved to {:?}",
                                        file_path
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            error!("[nixcache-proxy] Failed to fetch index blob: {}", e);
                        }
                    }
                }
            }
            Ok(None) => {
                info!("[nixcache-proxy] Cache index manifest not found on GHCR.");
            }
            Err(e) => {
                error!(
                    "[nixcache-proxy] Failed to fetch cache index manifest: {}",
                    e
                );
            }
        }

        if !refresh_ok {
            // Load backup if remote refresh failed
            let file_path = self.index_dir.join("cache-index.json");
            if file_path.exists() {
                match fs::read(&file_path).await {
                    Ok(bytes) => {
                        if let Ok(index_data) = serde_json::from_slice::<CacheIndexData>(&bytes) {
                            let mut current_data = self.data.write().await;
                            *current_data = index_data;
                            info!(
                                "[nixcache-proxy] Loaded backup cache index from {:?}",
                                file_path
                            );
                            refresh_ok = true;
                        }
                    }
                    Err(e) => {
                        error!("[nixcache-proxy] Failed to read backup cache index: {}", e);
                    }
                }
            }
        }

        *last_ref = Some(Instant::now());

        if refresh_ok {
            let current = self.data.read().await;
            info!(
                "[nixcache-proxy] Index refreshed successfully with {} entries.",
                current.entries.len()
            );
            Ok(())
        } else {
            Err("Failed to refresh index from both remote registry and backup".to_string())
        }
    }

    pub async fn lookup(&self, store_hash: &str) -> Option<IndexEntry> {
        let index = self.get_data().await;
        index.entries.get(store_hash).cloned()
    }

    pub async fn find_nar_digest(&self, nar_basename: &str) -> Option<String> {
        let index = self.get_data().await;
        for entry in index.entries.values() {
            for line in entry.narinfo.lines() {
                if line.starts_with("URL: ") && line.contains(nar_basename) {
                    return Some(entry.nar_digest.clone());
                }
            }
        }
        None
    }
}
