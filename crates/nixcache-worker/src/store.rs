use crate::{
    error::WorkerStoreError,
    state::{CachedBaselineEntry, CachedShardEntry, L1_MEM_TTL_MS, WorkerState},
    transport::WorkerFetchTransport,
};
use nixcache_core::{
    NarDigest, ShardDataPayload, ShardDescriptor, ShardedArchCacheIndexData, StoreHash, SystemArch,
    build_nar_lookup_map, calculate_shard_id, diff_shard_descriptors, extract_nar_basename,
    extract_store_hash,
};
use nixcache_oci::OciClient;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use worker::{Env, js_sys::Date};

pub type WorkerOciClient = OciClient<WorkerFetchTransport>;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RefreshResult {
    pub total_entries: usize,
    pub warmed_shards: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KVCacheWrapper<T> {
    pub data: T,
    pub last_refresh: f64,
    pub manifest_digest: String,
}

#[derive(Clone, Debug)]
pub struct NarInfoLookupResult {
    pub narinfo_content: String,
    pub shard_id: Option<u16>,
    pub manifest_digest: Option<String>,
    pub self_healed: bool,
}

#[derive(Clone, Debug)]
pub struct WorkerProxyConfig {
    pub registry: String,
    pub repo: String,
    pub baseline_tag: String,
    pub upstream_caches: Vec<String>,
    pub baseline_ttl_secs: u64,
    pub target_system: SystemArch,
}

impl Default for WorkerProxyConfig {
    fn default() -> Self {
        Self {
            registry: "ghcr.io".to_string(),
            repo: String::new(),
            baseline_tag: "cache-index".to_string(),
            upstream_caches: vec!["https://cache.nixos.org".to_string()],
            baseline_ttl_secs: 300,
            target_system: SystemArch::X86_64Linux,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteStatus {
    pub remote_connected: bool,
    pub remote_error: Option<String>,
    pub registry: String,
    pub repo: String,
    pub tier0_hot_entries: usize,
    pub baseline_entries: usize,
    pub total_unique_entries: usize,
    pub index_entries: usize,
    pub index_ttl: u64,
    pub baseline_ttl: u64,
    pub upstream: Vec<String>,
    pub manifest_digest: String,
    pub generated: String,
}

pub struct CacheStore {
    oci_client: WorkerOciClient,
    config: WorkerProxyConfig,
    baseline_ttl_ms: f64,
}

impl CacheStore {
    pub fn new(oci_client: WorkerOciClient, config: WorkerProxyConfig) -> Self {
        let baseline_ttl_ms = (config.baseline_ttl_secs * 1000) as f64;
        Self {
            oci_client,
            config,
            baseline_ttl_ms,
        }
    }

    pub fn config(&self) -> &WorkerProxyConfig {
        &self.config
    }

    pub fn oci_client(&self) -> &WorkerOciClient {
        &self.oci_client
    }

    fn validate_root(&self, root: &ShardedArchCacheIndexData) -> Result<(), WorkerStoreError> {
        root.validate_for(
            &self.config.target_system,
            &self.config.repo,
            &self.config.registry,
            self.oci_client.limits(),
        )?;
        Ok(())
    }

    fn validate_shard(
        &self,
        shard: &ShardDataPayload,
        shard_id: u16,
    ) -> Result<(), WorkerStoreError> {
        shard.validate_for(
            shard_id,
            &self.config.target_system,
            self.oci_client.limits(),
        )?;
        Ok(())
    }

    /// 设置全局远端 GHCR 连通状态
    pub fn set_remote_status(&self, connected: bool, error: Option<String>) {
        WorkerState::global().set_remote_status(connected, error);
    }

    /// 级联查询 Store Hash 对应的 narinfo (Tier 1 Session -> Tier 2 Branch -> Tier 3 Baseline -> Read-Through SWR 自愈)
    pub async fn lookup_narinfo(
        &self,
        env: &Env,
        store_hash: &str,
    ) -> Result<Option<NarInfoLookupResult>, WorkerStoreError> {
        let parsed_hash = match StoreHash::parse(store_hash) {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };

        // 1. Production Baseline (SMRI 1024 阶确定性分片)
        let (baseline, manifest_digest) = self.get_baseline_data(env).await?;
        let shard_id = calculate_shard_id(&parsed_hash);

        if let Some(shard_desc) = baseline.find_shard_by_id(shard_id)
            && !shard_desc.is_empty()
            && !shard_desc.blob_digest.is_empty()
        {
            let (shard_payload, _) = self.get_shard_data(env, shard_desc).await?;
            if let Some(entry) = shard_payload.entries.get(&parsed_hash) {
                return Ok(Some(NarInfoLookupResult {
                    narinfo_content: entry.to_narinfo_string(),
                    shard_id: Some(shard_id),
                    manifest_digest: Some(manifest_digest),
                    self_healed: false,
                }));
            }
        }

        // 2. Cache Miss: Read-Through SWR 防抖自愈穿透探查
        if WorkerState::global().should_revalidate() {
            let arch_tag = format!(
                "{}-{}",
                self.config.baseline_tag,
                self.config.target_system.as_str()
            );
            let remote_head = match self.oci_client.manifests().head(&arch_tag).await {
                Ok(Some(h)) => Some(h),
                Ok(None) => self
                    .oci_client
                    .manifests()
                    .head(&self.config.baseline_tag)
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };

            if let Some(remote_digest) = remote_head
                && !remote_digest.is_empty()
                && remote_digest != manifest_digest
            {
                // 探测到远端存在更新的基线清单摘要，执行轻量自愈刷新
                if let Ok((refreshed_baseline, new_manifest_digest)) =
                    self.refresh_baseline_from_ghcr(env).await
                    && let Some(new_shard_desc) = refreshed_baseline.find_shard_by_id(shard_id)
                    && !new_shard_desc.is_empty()
                    && !new_shard_desc.blob_digest.is_empty()
                {
                    let (refreshed_shard, _) = self.get_shard_data(env, new_shard_desc).await?;
                    if let Some(entry) = refreshed_shard.entries.get(&parsed_hash) {
                        return Ok(Some(NarInfoLookupResult {
                            narinfo_content: entry.to_narinfo_string(),
                            shard_id: Some(shard_id),
                            manifest_digest: Some(new_manifest_digest),
                            self_healed: true,
                        }));
                    }
                }
            }
        }

        Ok(None)
    }

    /// 级联反向解析 NAR 文件名对应的 Blob Digest (全链路 O(1) 查找)
    pub async fn lookup_nar_digest(
        &self,
        env: &Env,
        nar_basename: &str,
    ) -> Result<Option<NarDigest>, WorkerStoreError> {
        let normalized = extract_nar_basename(nar_basename);

        // Production Baseline (StoreHash Shard Routing)
        let (baseline, _) = self.get_baseline_data(env).await?;

        if let Some(store_hash) = extract_store_hash(nar_basename)
            && let Ok(parsed_hash) = StoreHash::parse(store_hash.as_str())
        {
            let shard_id = calculate_shard_id(&parsed_hash);
            if let Some(shard_desc) = baseline.find_shard_by_id(shard_id)
                && !shard_desc.is_empty()
                && !shard_desc.blob_digest.is_empty()
            {
                let (_, nar_lookup) = self.get_shard_data(env, shard_desc).await?;
                if let Some(digest) = nar_lookup.get(normalized) {
                    return Ok(Some(digest.clone()));
                }
            }
        }

        // 遍历已加载且属于当前 Baseline 的内存分片缓存 (防止过期分片污染)
        let mut found_digest = None;
        WorkerState::global()
            .mem_shard_cache
            .iter_sync(|shard_id, entry| {
                if let Some(shard_desc) = baseline.find_shard_by_id(*shard_id)
                    && shard_desc.blob_digest == entry.blob_digest
                    && let Some(digest) = entry.nar_lookup.get(normalized)
                {
                    found_digest = Some(digest.clone());
                    return false;
                }
                true
            });

        if let Some(digest) = found_digest {
            return Ok(Some(digest));
        }

        Ok(None)
    }

    /// 获取有效的签名公钥
    pub async fn get_public_key(&self, env: &Env) -> Result<Option<String>, WorkerStoreError> {
        let (baseline, _) = self.get_baseline_data(env).await?;
        if !baseline.public_key.is_empty() {
            Ok(Some(baseline.public_key))
        } else {
            Ok(None)
        }
    }

    /// 按需拉取或获取单个分片数据 (L1 Memory -> L2 KV -> L3 GHCR)
    pub async fn get_shard_data(
        &self,
        env: &Env,
        shard_desc: &ShardDescriptor,
    ) -> Result<(ShardDataPayload, HashMap<String, NarDigest>), WorkerStoreError> {
        let shard_id = shard_desc.shard_id;
        let blob_digest = shard_desc.blob_digest.as_str();
        if blob_digest.is_empty() {
            return Err(WorkerStoreError::Core(
                "Empty blob digest for shard".to_string(),
            ));
        }

        let now = Date::now();

        // 1. L1 Memory Cache
        if let Some(cached) = WorkerState::global()
            .mem_shard_cache
            .read_sync(&shard_id, |_, v| v.clone())
            && cached.blob_digest == blob_digest
            && now < cached.expires_at
        {
            return Ok((cached.payload.clone(), cached.nar_lookup.clone()));
        }

        // 2. L2 Cloudflare KV (Content-Addressable: shard_v6_{blob_digest})
        let kv_key = format!("shard_v6_{}", blob_digest);

        if let Ok(kv) = env.kv("NIXCACHE_KV")
            && let Ok(Some(wrapper)) = kv
                .get(&kv_key)
                .json::<KVCacheWrapper<ShardDataPayload>>()
                .await
        {
            let payload = wrapper.data;
            if self.validate_shard(&payload, shard_id).is_ok() {
                let nar_lookup = build_nar_lookup_map(&payload.entries);

                let _ = WorkerState::global().mem_shard_cache.upsert_sync(
                    shard_id,
                    Arc::new(CachedShardEntry {
                        payload: payload.clone(),
                        nar_lookup: nar_lookup.clone(),
                        blob_digest: blob_digest.to_string(),
                        expires_at: now + L1_MEM_TTL_MS,
                    }),
                );
                return Ok((payload, nar_lookup));
            }
        }

        // 3. L3 OCI GHCR
        match self
            .oci_client
            .indexes()
            .get_shard_data(shard_desc, &self.config.target_system)
            .await
        {
            Ok(payload) => {
                self.set_remote_status(true, None);
                let nar_lookup = build_nar_lookup_map(&payload.entries);

                if let Ok(kv) = env.kv("NIXCACHE_KV") {
                    let wrapper = KVCacheWrapper {
                        data: payload.clone(),
                        last_refresh: now,
                        manifest_digest: blob_digest.to_string(),
                    };
                    let _ = kv
                        .put(&kv_key, &wrapper)
                        .map_err(|e| WorkerStoreError::KvPutFailed {
                            key: kv_key.clone(),
                            message: e.to_string(),
                        })?
                        .execute()
                        .await;
                }

                let _ = WorkerState::global().mem_shard_cache.upsert_sync(
                    shard_id,
                    Arc::new(CachedShardEntry {
                        payload: payload.clone(),
                        nar_lookup: nar_lookup.clone(),
                        blob_digest: blob_digest.to_string(),
                        expires_at: now + L1_MEM_TTL_MS,
                    }),
                );

                Ok((payload, nar_lookup))
            }
            Err(e) => {
                self.set_remote_status(false, Some(format!("GHCR shard {}: {}", blob_digest, e)));
                if let Ok(kv) = env.kv("NIXCACHE_KV")
                    && let Ok(Some(wrapper)) = kv
                        .get(&kv_key)
                        .json::<KVCacheWrapper<ShardDataPayload>>()
                        .await
                {
                    let payload = wrapper.data;
                    if self.validate_shard(&payload, shard_id).is_ok() {
                        let nar_lookup = build_nar_lookup_map(&payload.entries);
                        return Ok((payload, nar_lookup));
                    }
                }
                Err(e.into())
            }
        }
    }

    /// 获取生产基线分片根索引 (L1 Memory -> L2 KV -> L3 GHCR，纯粹单原子 baseline_v6_{system})
    pub async fn get_baseline_data(
        &self,
        env: &Env,
    ) -> Result<(ShardedArchCacheIndexData, String), WorkerStoreError> {
        let now = Date::now();

        // 1. L1 Memory Cache
        if let Some(cached) = WorkerState::global().mem_baseline_cache.load_full()
            && now < cached.expires_at
        {
            return Ok((cached.root.clone(), cached.manifest_digest.clone()));
        }

        // 2. L2 Cloudflare KV (单原子 baseline_v6_{system})
        let kv = env
            .kv("NIXCACHE_KV")
            .map_err(|e| WorkerStoreError::KvGetFailed {
                key: "NIXCACHE_KV".to_string(),
                message: e.to_string(),
            })?;
        let baseline_key = format!("baseline_v6_{}", self.config.target_system.as_str());

        if let Ok(Some(wrapper)) = kv
            .get(&baseline_key)
            .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
            .await
        {
            let root_data = wrapper.data;
            let manifest_digest = wrapper.manifest_digest;
            if now - wrapper.last_refresh < self.baseline_ttl_ms
                && self.validate_root(&root_data).is_ok()
            {
                WorkerState::global()
                    .mem_baseline_cache
                    .store(Some(Arc::new(CachedBaselineEntry {
                        root: root_data.clone(),
                        manifest_digest: manifest_digest.clone(),
                        expires_at: now + L1_MEM_TTL_MS,
                    })));
                return Ok((root_data, manifest_digest));
            }
        }

        // 3. L3 OCI GHCR
        match self.refresh_baseline_from_ghcr(env).await {
            Ok(res) => Ok(res),
            Err(e) => {
                if let Ok(Some(wrapper)) = kv
                    .get(&baseline_key)
                    .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
                    .await
                    && self.validate_root(&wrapper.data).is_ok()
                {
                    return Ok((wrapper.data, wrapper.manifest_digest));
                }
                Err(e)
            }
        }
    }

    async fn refresh_baseline_from_ghcr(
        &self,
        env: &Env,
    ) -> Result<(ShardedArchCacheIndexData, String), WorkerStoreError> {
        let now = Date::now();
        let old_cached = WorkerState::global().mem_baseline_cache.load_full();

        let fetch_root = self
            .oci_client
            .indexes()
            .get_sharded_root(&self.config.baseline_tag, &self.config.target_system)
            .await;

        let (root_data, manifest_digest) = match fetch_root {
            Ok(Some((data, digest))) => {
                self.set_remote_status(true, None);
                (data, digest)
            }
            Ok(None) => {
                self.set_remote_status(true, None);
                (
                    ShardedArchCacheIndexData::new(
                        self.config.target_system,
                        &self.config.repo,
                        &self.config.registry,
                    ),
                    String::new(),
                )
            }
            Err(e) => {
                self.set_remote_status(false, Some(format!("GHCR baseline root: {}", e)));
                return Err(e.into());
            }
        };
        self.validate_root(&root_data)?;

        // 比对新旧基线 Merkle Root 与 Shards 描述符，淘汰已失效分片
        if let Some(old_b) = old_cached
            && old_b.root.merkle_root != root_data.merkle_root
        {
            let invalidated_shards = diff_shard_descriptors(&old_b.root.shards, &root_data.shards);
            for shard_id in invalidated_shards {
                WorkerState::global().mem_shard_cache.remove_sync(&shard_id);
            }
        }

        let kv = env
            .kv("NIXCACHE_KV")
            .map_err(|e| WorkerStoreError::KvGetFailed {
                key: "NIXCACHE_KV".to_string(),
                message: e.to_string(),
            })?;
        let baseline_key = format!("baseline_v6_{}", self.config.target_system.as_str());

        let wrapper = KVCacheWrapper {
            data: root_data.clone(),
            last_refresh: now,
            manifest_digest: manifest_digest.clone(),
        };
        let _ = kv
            .put(&baseline_key, &wrapper)
            .map_err(|e| WorkerStoreError::KvPutFailed {
                key: baseline_key.clone(),
                message: e.to_string(),
            })?
            .execute()
            .await;

        WorkerState::global()
            .mem_baseline_cache
            .store(Some(Arc::new(CachedBaselineEntry {
                root: root_data.clone(),
                manifest_digest: manifest_digest.clone(),
                expires_at: now + L1_MEM_TTL_MS,
            })));

        Ok((root_data, manifest_digest))
    }

    /// 强制刷新所有层级的索引并主动预热变更分片
    pub async fn force_refresh(&self, env: &Env) -> Result<RefreshResult, WorkerStoreError> {
        let mut errors = Vec::new();

        // 1. 获取旧的分片列表以供后续 diff 对比
        let old_shards = if let Some(cached) = WorkerState::global().mem_baseline_cache.load_full()
        {
            cached.root.shards.clone()
        } else if let Ok(kv) = env.kv("NIXCACHE_KV") {
            let baseline_key = format!("baseline_v6_{}", self.config.target_system.as_str());
            if let Ok(Some(wrapper)) = kv
                .get(&baseline_key)
                .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
                .await
            {
                if self.validate_root(&wrapper.data).is_ok() {
                    wrapper.data.shards
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        WorkerState::global().clear_l1_caches();

        let (baseline, _) = match self.refresh_baseline_from_ghcr(env).await {
            Ok(b) => b,
            Err(e) => {
                errors.push(format!("Baseline: {}", e));
                (
                    ShardedArchCacheIndexData::new(
                        self.config.target_system,
                        &self.config.repo,
                        &self.config.registry,
                    ),
                    String::new(),
                )
            }
        };

        // 2. 比对变动分片并主动预热 (Active Shard Warm-Up)
        let changed_shard_ids = diff_shard_descriptors(&old_shards, &baseline.shards);
        let mut warm_up_futures = Vec::new();
        for shard_id in changed_shard_ids {
            if let Some(shard_desc) = baseline.find_shard_by_id(shard_id)
                && !shard_desc.is_empty()
                && !shard_desc.blob_digest.is_empty()
            {
                let descriptor = shard_desc.clone();
                warm_up_futures.push(async move { self.get_shard_data(env, &descriptor).await });
            }
        }

        let warm_up_results = futures_util::future::join_all(warm_up_futures).await;
        let mut warmed_shards = 0;
        for res in warm_up_results {
            match res {
                Ok(_) => warmed_shards += 1,
                Err(e) => errors.push(format!("Shard warmup failed: {}", e)),
            }
        }

        let status = self.get_status(env).await;
        if errors.is_empty() || status.total_unique_entries > 0 || baseline.total_entries() > 0 {
            Ok(RefreshResult {
                total_entries: status.total_unique_entries,
                warmed_shards,
            })
        } else {
            Err(WorkerStoreError::AggregatedRefreshFailed { errors })
        }
    }

    /// 获取完整的状态元信息与各层级统计 (实时 RCU 远端连通度)
    pub async fn get_status(&self, env: &Env) -> RemoteStatus {
        let baseline_res = self.get_baseline_data(env).await;
        let (baseline_count, manifest_digest, generated) = match baseline_res {
            Ok((ref b, ref digest)) => (b.total_entries(), digest.clone(), b.generated.clone()),
            Err(_) => {
                let baseline_key = format!("baseline_v6_{}", self.config.target_system.as_str());
                let kv_data = match env.kv("NIXCACHE_KV") {
                    Ok(kv) => kv
                        .get(&baseline_key)
                        .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
                        .await
                        .ok()
                        .flatten(),
                    Err(_) => None,
                };
                match kv_data {
                    Some(w) if self.validate_root(&w.data).is_ok() => {
                        (w.data.total_entries(), w.manifest_digest, w.data.generated)
                    }
                    None => (0, String::new(), String::new()),
                    Some(_) => (0, String::new(), String::new()),
                }
            }
        };

        let remote_state = WorkerState::global().remote_status.load();
        let remote_connected = remote_state.connected;
        let remote_error = remote_state.last_error.clone();

        let total_unique = baseline_count;

        RemoteStatus {
            remote_connected,
            remote_error,
            registry: self.config.registry.clone(),
            repo: self.config.repo.clone(),
            tier0_hot_entries: 0,
            baseline_entries: baseline_count,
            total_unique_entries: total_unique,
            index_entries: total_unique,
            index_ttl: self.config.baseline_ttl_secs,
            baseline_ttl: self.config.baseline_ttl_secs,
            upstream: self.config.upstream_caches.clone(),
            manifest_digest,
            generated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RemoteStatus, WorkerProxyConfig};
    use nixcache_core::{
        IndexEntry, NarDigest, NarInfoMeta, SCHEMA_VERSION_V6, ShardDataPayload,
        ShardedArchCacheIndexData, StoreHash, SystemArch, build_nar_lookup_map,
        diff_shard_descriptors,
    };
    use std::collections::HashMap;

    #[test]
    fn test_build_nar_lookup_map() {
        let mut entries = HashMap::new();
        let hash1 = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let digest1 = NarDigest::new_sha256(
            "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
        )
        .unwrap();

        entries.insert(
            hash1.clone(),
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-pkg1", hash1),
                    nar_basename: "test.nar.xz".to_string(),
                    nar_hash:
                        "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                            .to_string(),
                    ..Default::default()
                },
                nar_digest: digest1.clone(),
                nar_size: 100,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        let map = build_nar_lookup_map(&entries);
        assert_eq!(map.get("test.nar.xz"), Some(&digest1));
    }

    #[test]
    fn test_remote_status_serialization() {
        let status = RemoteStatus {
            remote_connected: true,
            remote_error: None,
            registry: "ghcr.io".to_string(),
            repo: "test/repo".to_string(),
            tier0_hot_entries: 0,
            baseline_entries: 3,
            total_unique_entries: 3,
            index_entries: 3,
            index_ttl: 300,
            baseline_ttl: 300,
            upstream: vec!["https://cache.nixos.org".to_string()],
            manifest_digest: "sha256:digest".to_string(),
            generated: "2026-08-29T10:00:00Z".to_string(),
        };

        let json = serde_json::to_string(&status).unwrap();
        let parsed: RemoteStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, parsed);
    }

    #[test]
    fn test_worker_proxy_config_default() {
        let config = WorkerProxyConfig::default();
        assert_eq!(config.registry, "ghcr.io");
        assert_eq!(config.baseline_tag, "cache-index");
        assert_eq!(config.baseline_ttl_secs, 300);
        assert_eq!(config.target_system, SystemArch::X86_64Linux);
    }

    #[test]
    fn test_schema_v6_sharding_serialization() {
        let root = ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        assert_eq!(root.version, SCHEMA_VERSION_V6);
        assert_eq!(root.shards.len(), 1024);

        let shard = ShardDataPayload::new(0);
        assert_eq!(shard.version, SCHEMA_VERSION_V6);
    }

    #[test]
    fn test_merkle_diff_invalidation() {
        let root1 = ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        let mut root2 = root1.clone();

        root2.shards[0].blob_digest = "sha256:new_digest_0".to_string();
        root2.shards[5].blob_digest = "sha256:new_digest_5".to_string();

        let diff = diff_shard_descriptors(&root1.shards, &root2.shards);
        assert_eq!(diff, vec![0, 5]);
    }
}
