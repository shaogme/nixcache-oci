use crate::oci::OciClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, sync::Mutex};
use worker::{Env, js_sys::Date};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IndexEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
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
    pub gc_roots: HashMap<String, Vec<String>>,
    #[serde(skip)]
    pub nar_lookup: HashMap<String, String>,
    #[serde(skip)]
    pub manifest_digest: String,
}

impl CacheIndexData {
    pub fn rebuild_lookup_table(&mut self) {
        let mut map = HashMap::with_capacity(self.entries.len());
        for entry in self.entries.values() {
            for line in entry.narinfo.lines() {
                if let Some(rest) = line.strip_prefix("URL: nar/") {
                    let nar_name = rest.split_whitespace().next().unwrap_or(rest);
                    map.insert(nar_name.to_string(), entry.nar_digest.clone());
                    break;
                } else if let Some(rest) = line.strip_prefix("URL: ") {
                    let path = rest.split_whitespace().next().unwrap_or(rest);
                    let nar_name = path.rsplit('/').next().unwrap_or(path);
                    map.insert(nar_name.to_string(), entry.nar_digest.clone());
                    break;
                }
            }
        }
        self.nar_lookup = map;
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct KVCacheWrapper {
    pub data: CacheIndexData,
    pub last_refresh: f64,
    pub manifest_digest: String,
}

// L1 实例内存缓存：(数据, 过期时间戳 ms)
static IN_MEMORY_CACHE: Mutex<Option<(CacheIndexData, f64)>> = Mutex::new(None);
// 读穿透防抖时钟：记录上一次向 GHCR 发起检查的时间戳 (ms)
static LAST_GHCR_CHECK: Mutex<f64> = Mutex::new(0.0);

const L1_MEM_TTL_MS: f64 = 10_000.0; // 内存缓存 10 秒
const DEBOUNCE_THRESHOLD_MS: f64 = 500.0; // Miss 读穿透防抖 500 毫秒

pub struct CacheStore {
    oci_client: OciClient,
    kv_ttl_ms: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RemoteStatus {
    pub connected: bool,
    pub error: Option<String>,
    pub entries_count: usize,
    pub generated: String,
    pub manifest_digest: String,
}

impl CacheStore {
    pub fn new(oci_client: OciClient, ttl_seconds: u64) -> Self {
        Self {
            oci_client,
            kv_ttl_ms: (ttl_seconds * 1000) as f64,
        }
    }

    /// 获取远程与缓存状态元信息
    pub async fn get_status(&self, env: &Env) -> RemoteStatus {
        match self.get_data(env).await {
            Ok(data) => RemoteStatus {
                connected: true,
                error: None,
                entries_count: data.entries.len(),
                generated: data.generated,
                manifest_digest: data.manifest_digest,
            },
            Err(e) => {
                let local_data = match env.kv("NIXCACHE_KV") {
                    Ok(kv) => {
                        if let Ok(Some(wrapper)) =
                            kv.get("cache_index_wrapper").json::<KVCacheWrapper>().await
                        {
                            Some(wrapper.data)
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                };

                let (entries_count, generated, manifest_digest) = match local_data {
                    Some(d) => (d.entries.len(), d.generated, d.manifest_digest),
                    None => (0, String::new(), String::new()),
                };

                RemoteStatus {
                    connected: false,
                    error: Some(e),
                    entries_count,
                    generated,
                    manifest_digest,
                }
            }
        }
    }

    /// 获取当前索引数据（常规读 L1 -> L2 -> L3）
    pub async fn get_data(&self, env: &Env) -> Result<CacheIndexData, String> {
        let now = Date::now();

        // 1. 检查 L1 内存缓存
        {
            let cache = IN_MEMORY_CACHE.lock().map_err(|e| e.to_string())?;
            if let Some((ref data, expiry)) = *cache
                && now < expiry
            {
                return Ok(data.clone());
            }
        }

        // 2. 检查 L2 Cloudflare KV
        let kv = env.kv("NIXCACHE_KV").map_err(|e| e.to_string())?;
        if let Ok(Some(wrapper)) = kv.get("cache_index_wrapper").json::<KVCacheWrapper>().await
            && now - wrapper.last_refresh < self.kv_ttl_ms
        {
            let mut data = wrapper.data;
            data.manifest_digest = wrapper.manifest_digest;
            data.rebuild_lookup_table();

            let mut cache = IN_MEMORY_CACHE.lock().map_err(|e| e.to_string())?;
            *cache = Some((data.clone(), now + L1_MEM_TTL_MS));
            return Ok(data);
        }

        // 3. 超过 TTL 或不存在，向 GHCR 刷新
        match self.force_refresh(env).await {
            Ok(data) => Ok(data),
            Err(e) => {
                if let Ok(Some(wrapper)) =
                    kv.get("cache_index_wrapper").json::<KVCacheWrapper>().await
                {
                    let mut data = wrapper.data;
                    data.manifest_digest = wrapper.manifest_digest;
                    data.rebuild_lookup_table();
                    return Ok(data);
                }
                Err(e)
            }
        }
    }

    /// 查询 Store Hash 对应的 narinfo，若未命中则智能触发读穿透
    pub async fn get_narinfo_with_fallback(
        &self,
        env: &Env,
        store_hash: &str,
    ) -> Result<Option<String>, String> {
        let data = self.get_data(env).await?;

        // 1. 直接命中
        if let Some(entry) = data.entries.get(store_hash) {
            return Ok(Some(entry.narinfo.clone()));
        }

        // 2. 未命中，检查防抖后向 GHCR 发起读穿透检查
        let now = Date::now();
        let should_check_ghcr = {
            let mut last_check = LAST_GHCR_CHECK.lock().map_err(|e| e.to_string())?;
            if now - *last_check > DEBOUNCE_THRESHOLD_MS {
                *last_check = now;
                true
            } else {
                false
            }
        };

        if should_check_ghcr
            && let Ok(refreshed) = self.force_refresh(env).await
            && let Some(entry) = refreshed.entries.get(store_hash)
        {
            return Ok(Some(entry.narinfo.clone()));
        }

        Ok(None)
    }

    /// 查询 NAR 文件对应的 Blob Digest，若未命中则智能触发读穿透
    pub async fn get_nar_digest_with_fallback(
        &self,
        env: &Env,
        nar_name: &str,
    ) -> Result<Option<String>, String> {
        let data = self.get_data(env).await?;

        // 1. O(1) 快速反向索引查找
        if let Some(digest) = data.nar_lookup.get(nar_name) {
            return Ok(Some(digest.clone()));
        }

        // 2. 未命中，检查防抖后向 GHCR 发起读穿透检查
        let now = Date::now();
        let should_check_ghcr = {
            let mut last_check = LAST_GHCR_CHECK.lock().map_err(|e| e.to_string())?;
            if now - *last_check > DEBOUNCE_THRESHOLD_MS {
                *last_check = now;
                true
            } else {
                false
            }
        };

        if should_check_ghcr
            && let Ok(refreshed) = self.force_refresh(env).await
            && let Some(digest) = refreshed.nar_lookup.get(nar_name)
        {
            return Ok(Some(digest.clone()));
        }

        Ok(None)
    }

    /// 强制从 GHCR 拉取最新 manifest 并更新 L1/L2 缓存
    pub async fn force_refresh(&self, env: &Env) -> Result<CacheIndexData, String> {
        let now = Date::now();
        let mut refreshed_data = self.refresh_from_ghcr().await?;
        refreshed_data.rebuild_lookup_table();

        // 写入 KV
        let kv = env.kv("NIXCACHE_KV").map_err(|e| e.to_string())?;
        let wrapper = KVCacheWrapper {
            data: refreshed_data.clone(),
            last_refresh: now,
            manifest_digest: refreshed_data.manifest_digest.clone(),
        };
        let _ = kv
            .put("cache_index_wrapper", &wrapper)
            .map_err(|e| e.to_string())?
            .execute()
            .await;

        // 写入 L1 内存
        let mut cache = IN_MEMORY_CACHE.lock().map_err(|e| e.to_string())?;
        *cache = Some((refreshed_data.clone(), now + L1_MEM_TTL_MS));

        // 更新刷新时间戳
        if let Ok(mut last_check) = LAST_GHCR_CHECK.lock() {
            *last_check = now;
        }

        Ok(refreshed_data)
    }

    async fn refresh_from_ghcr(&self) -> Result<CacheIndexData, String> {
        match self
            .oci_client
            .get_manifest_with_digest("cache-index")
            .await?
        {
            Some((manifest_json, manifest_digest)) => {
                let manifest: Value =
                    serde_json::from_str(&manifest_json).map_err(|e| e.to_string())?;
                let layers = manifest
                    .get("layers")
                    .and_then(|l| l.as_array())
                    .ok_or_else(|| "Manifest layers not found".to_string())?;

                if layers.is_empty() {
                    return Err("Manifest layers are empty".to_string());
                }

                let digest = layers[0]
                    .get("digest")
                    .and_then(|d| d.as_str())
                    .ok_or_else(|| "Layer digest missing".to_string())?;

                let blob_bytes = self.oci_client.get_blob(digest).await?;
                let mut index_data: CacheIndexData =
                    serde_json::from_slice(&blob_bytes).map_err(|e| e.to_string())?;
                index_data.manifest_digest = manifest_digest;

                Ok(index_data)
            }
            None => {
                // Manifest not found on GHCR (HTTP 404) - remote is connected, but no cache index pushed yet
                Ok(CacheIndexData::default())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheIndexData, IndexEntry, RemoteStatus};
    use std::collections::HashMap;

    #[test]
    fn test_rebuild_lookup_table() {
        let mut entries = HashMap::new();
        entries.insert(
            "hash1".to_string(),
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo:
                    "StorePath: /nix/store/hash1-pkg1\nURL: nar/pkg1.nar.xz\nCompression: xz\n"
                        .to_string(),
                nar_digest: "sha256:digest1".to_string(),
                nar_size: 100,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );
        entries.insert(
            "hash2".to_string(),
            IndexEntry {
                name: "pkg2".to_string(),
                system: None,
                narinfo: "StorePath: /nix/store/hash2-pkg2\nURL: pkg2.nar\nCompression: none\n"
                    .to_string(),
                nar_digest: "sha256:digest2".to_string(),
                nar_size: 200,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );

        let mut data = CacheIndexData {
            entries,
            ..Default::default()
        };

        data.rebuild_lookup_table();

        assert_eq!(
            data.nar_lookup.get("pkg1.nar.xz"),
            Some(&"sha256:digest1".to_string())
        );
        assert_eq!(
            data.nar_lookup.get("pkg2.nar"),
            Some(&"sha256:digest2".to_string())
        );
        assert_eq!(data.nar_lookup.get("nonexistent.nar"), None);
    }

    #[test]
    fn test_cache_index_data_serialization() {
        let mut entries = HashMap::new();
        entries.insert(
            "hash1".to_string(),
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/hash1-pkg1\n".to_string(),
                nar_digest: "sha256:digest1".to_string(),
                nar_size: 1024,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );

        let data = CacheIndexData {
            version: 2,
            repo: "owner/repo".to_string(),
            registry: "ghcr.io".to_string(),
            image: "ghcr.io/owner/repo/nix-cache".to_string(),
            generated: "2026-08-28T00:00:00Z".to_string(),
            public_key: "key:pub".to_string(),
            entries,
            gc_roots: HashMap::new(),
            nar_lookup: HashMap::new(),
            manifest_digest: "sha256:manifest".to_string(),
        };

        let json = serde_json::to_string(&data).unwrap();
        let parsed: CacheIndexData = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.repo, "owner/repo");
        assert_eq!(parsed.entries.len(), 1);
        // nar_lookup and manifest_digest are skipped in serialization
        assert!(parsed.nar_lookup.is_empty());
        assert!(parsed.manifest_digest.is_empty());
    }

    #[test]
    fn test_remote_status_serialization() {
        let status = RemoteStatus {
            connected: true,
            error: None,
            entries_count: 5,
            generated: "2026-08-28T00:00:00Z".to_string(),
            manifest_digest: "sha256:digest".to_string(),
        };

        let json = serde_json::to_string(&status).unwrap();
        let parsed: RemoteStatus = serde_json::from_str(&json).unwrap();
        assert!(parsed.connected);
        assert_eq!(parsed.entries_count, 5);
        assert_eq!(parsed.error, None);
    }
}
