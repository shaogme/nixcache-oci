use nixcache_oci::{CacheIndexData, IndexEntry, OciClient};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, sync::RwLock};
use tracing::{error, info};

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

    #[cfg(test)]
    pub async fn update_data_in_memory(&self, new_data: CacheIndexData) {
        let mut data = self.data.write().await;
        *data = new_data;
        let mut last = self.last_refresh.write().await;
        *last = Some(Instant::now());
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

#[cfg(test)]
mod tests {
    use super::*;
    use nixcache_oci::IndexEntry;
    use std::collections::HashMap;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[tokio::test]
    async fn test_cache_index_lookup_and_find_nar_digest() {
        let index = CacheIndex::new(
            "ghcr.io",
            "test/repo",
            "",
            PathBuf::from("/tmp/test-index-dir"),
            300,
        );

        let mut entries = HashMap::new();
        entries.insert(
            "hash123".to_string(),
            IndexEntry {
                name: "mypkg".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/hash123-mypkg\nURL: nar/hash123.nar.xz\n"
                    .to_string(),
                nar_digest: "sha256:narblobdigest123".to_string(),
                nar_size: 1024,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );

        let data = CacheIndexData {
            entries,
            ..Default::default()
        };
        index.update_data_in_memory(data).await;

        let entry = index.lookup("hash123").await.expect("Should find entry");
        assert_eq!(entry.name, "mypkg");

        assert_eq!(
            index.find_nar_digest("hash123.nar.xz").await,
            Some("sha256:narblobdigest123".to_string())
        );
        assert_eq!(index.find_nar_digest("nonexistent.nar.xz").await, None);
    }

    #[tokio::test]
    async fn test_backup_index_loading_when_remote_fails() {
        let temp_dir = tempfile::tempdir().unwrap();
        let backup_file = temp_dir.path().join("cache-index.json");

        let mut entries = HashMap::new();
        entries.insert(
            "backuphash".to_string(),
            IndexEntry {
                name: "backup-pkg".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/backuphash-pkg\n".to_string(),
                nar_digest: "sha256:backupdigest".to_string(),
                nar_size: 2048,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );
        let data = CacheIndexData {
            repo: "test/backup-repo".to_string(),
            entries,
            ..Default::default()
        };

        let json = serde_json::to_vec(&data).unwrap();
        tokio::fs::write(&backup_file, json).await.unwrap();

        // Registry 端口无法连接（引发远端失败），验证自动降级读取 backup
        let index = CacheIndex::new(
            "127.0.0.1:59999",
            "test/backup-repo",
            "",
            temp_dir.path().to_path_buf(),
            1, // 1s TTL
        );

        let loaded = index.get_data().await;
        assert_eq!(loaded.repo, "test/backup-repo");
        assert!(loaded.entries.contains_key("backuphash"));
    }

    #[tokio::test]
    async fn test_remote_refresh_and_backup_saving() {
        let server = MockServer::start().await;
        let host = server.address().to_string();
        let temp_dir = tempfile::tempdir().unwrap();

        let mut entries = HashMap::new();
        entries.insert(
            "remotehash".to_string(),
            IndexEntry {
                name: "remote-pkg".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/remotehash-pkg\n".to_string(),
                nar_digest: "sha256:remotedigest".to_string(),
                nar_size: 512,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );
        let index_data = CacheIndexData {
            repo: "test/remote-repo".to_string(),
            entries,
            ..Default::default()
        };
        let blob_bytes = serde_json::to_vec(&index_data).unwrap();

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "layers": [{
                "digest": "sha256:indexblob1",
                "size": blob_bytes.len()
            }]
        });

        Mock::given(method("GET"))
            .and(path("/v2/test/remote-repo/nix-cache/manifests/cache-index"))
            .respond_with(ResponseTemplate::new(200).set_body_json(manifest))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(
                "/v2/test/remote-repo/nix-cache/blobs/sha256:indexblob1",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(blob_bytes))
            .mount(&server)
            .await;

        let index = CacheIndex::new(
            &host,
            "test/remote-repo",
            "",
            temp_dir.path().to_path_buf(),
            60,
        );

        let count = index.force_refresh().await.unwrap();
        assert_eq!(count, 1);

        // 验证 backup 文件已被自动写入
        let backup_file = temp_dir.path().join("cache-index.json");
        assert!(backup_file.exists());
    }
}
