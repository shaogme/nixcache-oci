use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Mutex,
};
use worker::{
    Env,
    js_sys::Date,
};
use crate::oci::OciClient;

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

#[derive(Serialize, Deserialize, Clone, Debug)]
struct KVCacheWrapper {
    pub data: CacheIndexData,
    pub last_refresh: f64,
}

static IN_MEMORY_CACHE: Mutex<Option<(CacheIndexData, f64)>> = Mutex::new(None);

pub struct CacheStore {
    oci_client: OciClient,
    ttl_ms: f64,
}

impl CacheStore {
    pub fn new(oci_client: OciClient, ttl_seconds: u64) -> Self {
        Self {
            oci_client,
            ttl_ms: (ttl_seconds * 1000) as f64,
        }
    }

    pub async fn get_data(&self, env: &Env) -> Result<CacheIndexData, String> {
        let now = Date::now();

        // 1. Check in-memory cache first
        {
            let cache = IN_MEMORY_CACHE.lock().map_err(|e| e.to_string())?;
            if let Some((ref data, expiry)) = *cache {
                if now < expiry {
                    return Ok(data.clone());
                }
            }
        }

        // 2. Check Cloudflare KV
        let kv = env.kv("NIXCACHE_KV").map_err(|e| e.to_string())?;
        if let Ok(Some(wrapper)) = kv.get("cache_index_wrapper").json::<KVCacheWrapper>().await {
            if now - wrapper.last_refresh < self.ttl_ms {
                let mut cache = IN_MEMORY_CACHE.lock().map_err(|e| e.to_string())?;
                *cache = Some((wrapper.data.clone(), now + self.ttl_ms));
                return Ok(wrapper.data);
            }
        }

        // 3. TTL expired or not found, refresh from GHCR
        let refreshed_data = self.refresh_from_ghcr().await?;

        // Write to KV
        let wrapper = KVCacheWrapper {
            data: refreshed_data.clone(),
            last_refresh: now,
        };
        let _ = kv.put("cache_index_wrapper", &wrapper)
            .map_err(|e| e.to_string())?
            .execute()
            .await;

        // Write to In-Memory Cache
        let mut cache = IN_MEMORY_CACHE.lock().map_err(|e| e.to_string())?;
        *cache = Some((refreshed_data.clone(), now + self.ttl_ms));

        Ok(refreshed_data)
    }

    pub async fn force_refresh(&self, env: &Env) -> Result<CacheIndexData, String> {
        let now = Date::now();
        let refreshed_data = self.refresh_from_ghcr().await?;

        let kv = env.kv("NIXCACHE_KV").map_err(|e| e.to_string())?;
        let wrapper = KVCacheWrapper {
            data: refreshed_data.clone(),
            last_refresh: now,
        };
        let _ = kv.put("cache_index_wrapper", &wrapper)
            .map_err(|e| e.to_string())?
            .execute()
            .await;

        let mut cache = IN_MEMORY_CACHE.lock().map_err(|e| e.to_string())?;
        *cache = Some((refreshed_data.clone(), now + self.ttl_ms));

        Ok(refreshed_data)
    }

    async fn refresh_from_ghcr(&self) -> Result<CacheIndexData, String> {
        match self.oci_client.get_manifest("cache-index").await? {
            Some(manifest_json) => {
                let manifest: serde_json::Value = serde_json::from_str(&manifest_json)
                    .map_err(|e| e.to_string())?;
                let layers = manifest.get("layers")
                    .and_then(|l| l.as_array())
                    .ok_or_else(|| "Manifest layers not found".to_string())?;

                if layers.is_empty() {
                    return Err("Manifest layers are empty".to_string());
                }

                let digest = layers[0].get("digest")
                    .and_then(|d| d.as_str())
                    .ok_or_else(|| "Layer digest missing".to_string())?;

                let blob_bytes = self.oci_client.get_blob(digest).await?;
                let index_data: CacheIndexData = serde_json::from_slice(&blob_bytes)
                    .map_err(|e| e.to_string())?;

                Ok(index_data)
            }
            None => Err("Cache index manifest not found on GHCR".to_string()),
        }
    }
}
