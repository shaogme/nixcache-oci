use crate::{
    error::WorkerStoreError,
    state::{
        CachedBaselineEntry, CachedSessionEntry, CachedShardEntry, L1_MEM_TTL_MS, WorkerState,
    },
    transport::WorkerFetchTransport,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use nixcache_core::{
    DeltaPatchData, FastBlockedBloomFilter, IndexEntry, NarDigest, ShardDataPayload,
    ShardedArchCacheIndexData, StoreHash, SystemArch, build_nar_lookup_map, calculate_shard_id,
    diff_shard_descriptors, extract_nar_basename, extract_store_hash,
};
use nixcache_oci::OciClient;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::Ordering},
};
use worker::{Env, js_sys::Date};

pub type WorkerOciClient = OciClient<WorkerFetchTransport>;

pub fn format_branch_tag(br: &str) -> String {
    if br.chars().all(|c| c.is_ascii_digit()) {
        format!("pr-{}", br)
    } else if br.starts_with("pr-") || br.starts_with("branch-") {
        br.to_string()
    } else if let Some(stripped) = br.strip_prefix("refs/heads/") {
        format!("branch-{}", stripped.replace(['/', ':'], "-"))
    } else if let Some(stripped) = br.strip_prefix("refs/pull/") {
        let pr_id = stripped.split('/').next().unwrap_or(stripped);
        format!("pr-{}", pr_id)
    } else {
        format!("branch-{}", br.replace(['/', ':'], "-"))
    }
}

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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BloomFilterKvWrapper {
    pub num_entries: usize,
    pub num_hashes: u8,
    pub bytes_base64: String,
    pub last_refresh: f64,
    pub blob_digest: String,
}

#[derive(Clone, Debug)]
pub struct WorkerProxyConfig {
    pub registry: String,
    pub repo: String,
    pub run_id: Option<u64>,
    pub branch_or_pr: Option<String>,
    pub baseline_tag: String,
    pub upstream_caches: Vec<String>,
    pub session_ttl_secs: u64,
    pub baseline_ttl_secs: u64,
    pub target_system: SystemArch,
}

impl Default for WorkerProxyConfig {
    fn default() -> Self {
        Self {
            registry: "ghcr.io".to_string(),
            repo: String::new(),
            run_id: None,
            branch_or_pr: None,
            baseline_tag: "cache-index".to_string(),
            upstream_caches: vec!["https://cache.nixos.org".to_string()],
            session_ttl_secs: 10,
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
    pub run_id: Option<u64>,
    pub branch_or_pr: Option<String>,
    pub tier0_hot_entries: usize,
    pub tier1_session_entries: usize,
    pub tier2_branch_entries: usize,
    pub tier3_baseline_entries: usize,
    pub total_unique_entries: usize,
    pub index_entries: usize,
    pub index_ttl: u64,
    pub session_ttl: u64,
    pub baseline_ttl: u64,
    pub upstream: Vec<String>,
    pub manifest_digest: String,
    pub generated: String,
}

pub struct CacheStore {
    oci_client: WorkerOciClient,
    config: WorkerProxyConfig,
    session_ttl_ms: f64,
    baseline_ttl_ms: f64,
}

impl CacheStore {
    pub fn new(oci_client: WorkerOciClient, config: WorkerProxyConfig) -> Self {
        let session_ttl_ms = (config.session_ttl_secs * 1000) as f64;
        let baseline_ttl_ms = (config.baseline_ttl_secs * 1000) as f64;
        Self {
            oci_client,
            config,
            session_ttl_ms,
            baseline_ttl_ms,
        }
    }

    pub fn config(&self) -> &WorkerProxyConfig {
        &self.config
    }

    pub fn oci_client(&self) -> &WorkerOciClient {
        &self.oci_client
    }

    /// 设置全局远端 GHCR 连通状态
    pub fn set_remote_status(&self, connected: bool, error: Option<String>) {
        WorkerState::global().set_remote_status(connected, error);
    }

    /// 动态注册新编译完成的条目到 Tier 0 内存热表中 (0ms 延迟可用)
    pub fn register_hot_entries(entries: HashMap<StoreHash, IndexEntry>) {
        WorkerState::global().register_hot(entries);
    }

    /// 级联查询 Store Hash 对应的 narinfo (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3 布隆过滤器拦截与分片精准查找)
    pub async fn lookup_narinfo(
        &self,
        env: &Env,
        store_hash: &str,
    ) -> Result<Option<String>, WorkerStoreError> {
        let parsed_hash = match StoreHash::parse(store_hash) {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };

        // 1. Tier 0: In-Memory Hot Registry
        if let Some(entry) = WorkerState::global()
            .hot_entries
            .read_sync(&parsed_hash, |_, v| (**v).clone())
        {
            return Ok(Some(entry.to_narinfo_string()));
        }

        // 2. Tier 1: Workflow Run Session (run-<id>)
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Some((session, _)) = self.get_session_data(env, &tag).await?
                && let Some(entry) = session.new_entries.get(&parsed_hash)
            {
                return Ok(Some(entry.to_narinfo_string()));
            }
        }

        // 3. Tier 2: Branch / PR Session
        if let Some(ref br) = self.config.branch_or_pr {
            let tag = format_branch_tag(br);
            if let Some((branch_sess, _)) = self.get_session_data(env, &tag).await?
                && let Some(entry) = branch_sess.new_entries.get(&parsed_hash)
            {
                return Ok(Some(entry.to_narinfo_string()));
            }
        }

        // 4. Tier 3: Production Baseline (SMRI with Bloom Guard)
        let (baseline, bloom_filter) = self.get_baseline_data(env).await?;

        // 前置布隆过滤器 O(1) 负向拒绝拦截
        if !bloom_filter.is_empty() && !bloom_filter.contains(&parsed_hash) {
            return Ok(None);
        }

        let shard_id = calculate_shard_id(&parsed_hash);
        if let Some(shard_desc) = baseline.find_shard_by_id(shard_id)
            && !shard_desc.is_empty()
            && !shard_desc.blob_digest.is_empty()
        {
            let (shard_payload, _) = self
                .get_shard_data(env, shard_id, &shard_desc.blob_digest)
                .await?;
            if let Some(entry) = shard_payload.entries.get(&parsed_hash) {
                return Ok(Some(entry.to_narinfo_string()));
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

        // 1. Tier 0: In-Memory Hot Registry
        if let Some(digest) = WorkerState::global()
            .hot_nar_lookup
            .read_sync(normalized, |_, v| v.clone())
        {
            return Ok(Some(digest));
        }

        // 2. Tier 1: Workflow Run Session (run-<run_id>)
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Some((_, nar_lookup)) = self.get_session_data(env, &tag).await?
                && let Some(digest) = nar_lookup.get(normalized)
            {
                return Ok(Some(digest.clone()));
            }
        }

        // 3. Tier 2: Branch / PR Session
        if let Some(ref br) = self.config.branch_or_pr {
            let tag = format_branch_tag(br);
            if let Some((_, nar_lookup)) = self.get_session_data(env, &tag).await?
                && let Some(digest) = nar_lookup.get(normalized)
            {
                return Ok(Some(digest.clone()));
            }
        }

        // 4. Tier 3: Production Baseline (StoreHash Shard Routing with Bloom Guard)
        let (baseline, bloom_filter) = self.get_baseline_data(env).await?;

        if let Some(store_hash) = extract_store_hash(nar_basename)
            && let Ok(parsed_hash) = StoreHash::parse(store_hash.as_str())
            && (bloom_filter.is_empty() || bloom_filter.contains(&parsed_hash))
        {
            let shard_id = calculate_shard_id(&parsed_hash);
            if let Some(shard_desc) = baseline.find_shard_by_id(shard_id)
                && !shard_desc.is_empty()
                && !shard_desc.blob_digest.is_empty()
            {
                let (_, nar_lookup) = self
                    .get_shard_data(env, shard_id, &shard_desc.blob_digest)
                    .await?;
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

    /// 获取有效的签名公钥 (按 会话 -> 分支 -> 基线 优先级查找)
    pub async fn get_public_key(&self, env: &Env) -> Result<Option<String>, WorkerStoreError> {
        // Tier 1
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Ok(Some((session, _))) = self.get_session_data(env, &tag).await
                && !session.new_entries.is_empty()
            {
                // 可回退
            }
        }

        // Tier 3
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
        shard_id: u16,
        blob_digest: &str,
    ) -> Result<(ShardDataPayload, HashMap<String, NarDigest>), WorkerStoreError> {
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

        // 2. L2 Cloudflare KV (Content-Addressable: shard_v5_{blob_digest})
        let kv_key = format!("shard_v5_{}", blob_digest);

        if let Ok(kv) = env.kv("NIXCACHE_KV")
            && let Ok(Some(wrapper)) = kv
                .get(&kv_key)
                .json::<KVCacheWrapper<ShardDataPayload>>()
                .await
        {
            let payload = wrapper.data;
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

        // 3. L3 OCI GHCR
        match self.oci_client.get_shard_data(blob_digest).await {
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
                    let nar_lookup = build_nar_lookup_map(&payload.entries);
                    return Ok((payload, nar_lookup));
                }
                Err(e.into())
            }
        }
    }

    /// 从 OCI GHCR 强制刷新指定会话清单并写回 KV 和 L1
    pub async fn refresh_session_from_ghcr(
        &self,
        env: &Env,
        tag: &str,
    ) -> Result<Option<(DeltaPatchData, HashMap<String, NarDigest>)>, WorkerStoreError> {
        let now = Date::now();
        let arch_tag = format!("{}-{}", tag, self.config.target_system.as_str());
        let fetch_res = match self.oci_client.get_delta_patch_manifest(&arch_tag).await {
            Ok(Some(res)) => Ok(Some(res)),
            Ok(None) => self.oci_client.get_delta_patch_manifest(tag).await,
            Err(e) => Err(e),
        };

        let kv_key = format!("session_v5_{}_{}", self.config.target_system.as_str(), tag);

        match fetch_res {
            Ok(Some((delta, manifest_digest))) => {
                self.set_remote_status(true, None);
                let nar_lookup = build_nar_lookup_map(&delta.new_entries);

                if let Ok(kv) = env.kv("NIXCACHE_KV") {
                    let wrapper = KVCacheWrapper {
                        data: delta.clone(),
                        last_refresh: now,
                        manifest_digest,
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

                let _ = WorkerState::global().mem_session_cache.upsert_sync(
                    tag.to_string(),
                    Arc::new(CachedSessionEntry {
                        delta: delta.clone(),
                        nar_lookup: nar_lookup.clone(),
                        expires_at: now + L1_MEM_TTL_MS,
                    }),
                );

                Ok(Some((delta, nar_lookup)))
            }
            Ok(None) => {
                self.set_remote_status(true, None);
                Ok(None)
            }
            Err(e) => {
                self.set_remote_status(false, Some(format!("GHCR session {}: {}", tag, e)));
                if let Ok(kv) = env.kv("NIXCACHE_KV")
                    && let Ok(Some(wrapper)) = kv
                        .get(&kv_key)
                        .json::<KVCacheWrapper<DeltaPatchData>>()
                        .await
                {
                    let delta = wrapper.data;
                    let nar_lookup = build_nar_lookup_map(&delta.new_entries);
                    return Ok(Some((delta, nar_lookup)));
                }
                Err(e.into())
            }
        }
    }

    /// 获取会话清单数据 (L1 Memory -> L2 KV -> L3 GHCR)
    pub async fn get_session_data(
        &self,
        env: &Env,
        tag: &str,
    ) -> Result<Option<(DeltaPatchData, HashMap<String, NarDigest>)>, WorkerStoreError> {
        let now = Date::now();

        // 1. L1 Memory Cache
        if let Some(cached) = WorkerState::global()
            .mem_session_cache
            .read_sync(tag, |_, v| v.clone())
            && now < cached.expires_at
        {
            return Ok(Some((cached.delta.clone(), cached.nar_lookup.clone())));
        }

        // 2. L2 Cloudflare KV (带多架构命名空间隔离)
        let kv_key = format!("session_v5_{}_{}", self.config.target_system.as_str(), tag);
        if let Ok(kv) = env.kv("NIXCACHE_KV")
            && let Ok(Some(wrapper)) = kv
                .get(&kv_key)
                .json::<KVCacheWrapper<DeltaPatchData>>()
                .await
            && now - wrapper.last_refresh < self.session_ttl_ms
        {
            let delta = wrapper.data;
            let nar_lookup = build_nar_lookup_map(&delta.new_entries);

            let _ = WorkerState::global().mem_session_cache.upsert_sync(
                tag.to_string(),
                Arc::new(CachedSessionEntry {
                    delta: delta.clone(),
                    nar_lookup: nar_lookup.clone(),
                    expires_at: now + L1_MEM_TTL_MS,
                }),
            );
            return Ok(Some((delta, nar_lookup)));
        }

        // 3. L3 OCI GHCR
        self.refresh_session_from_ghcr(env, tag).await
    }

    /// 获取生产基线分片根索引与布隆过滤器 (L1 Memory -> L2 KV -> L3 GHCR)
    pub async fn get_baseline_data(
        &self,
        env: &Env,
    ) -> Result<(ShardedArchCacheIndexData, Arc<FastBlockedBloomFilter>), WorkerStoreError> {
        let now = Date::now();

        // 1. L1 Memory Cache
        if let Some(cached) = WorkerState::global().mem_baseline_cache.load_full()
            && now < cached.expires_at
        {
            return Ok((cached.root.clone(), cached.bloom_filter.clone()));
        }

        // 2. L2 Cloudflare KV (单次写入与读取，无 Legacy 双重冗余)
        let kv = env
            .kv("NIXCACHE_KV")
            .map_err(|e| WorkerStoreError::KvGetFailed {
                key: "NIXCACHE_KV".to_string(),
                message: e.to_string(),
            })?;
        let root_key = format!("baseline_root_v5_{}", self.config.target_system.as_str());
        let bloom_key = format!("baseline_bloom_v5_{}", self.config.target_system.as_str());

        if let Ok(Some(root_wrapper)) = kv
            .get(&root_key)
            .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
            .await
            && now - root_wrapper.last_refresh < self.baseline_ttl_ms
        {
            let root_data = root_wrapper.data;
            let manifest_digest = root_wrapper.manifest_digest;
            let bloom_filter = if let Ok(Some(bloom_wrapper)) =
                kv.get(&bloom_key).json::<BloomFilterKvWrapper>().await
            {
                let bloom_bytes = STANDARD.decode(&bloom_wrapper.bytes_base64)?;
                Arc::new(
                    FastBlockedBloomFilter::from_bytes(
                        &bloom_bytes,
                        bloom_wrapper.num_entries,
                        bloom_wrapper.num_hashes,
                    )
                    .unwrap_or_else(|_| FastBlockedBloomFilter::new_with_defaults(0)),
                )
            } else {
                Arc::new(FastBlockedBloomFilter::new_with_defaults(0))
            };

            WorkerState::global()
                .mem_baseline_cache
                .store(Some(Arc::new(CachedBaselineEntry {
                    root: root_data.clone(),
                    bloom_filter: bloom_filter.clone(),
                    manifest_digest,
                    expires_at: now + L1_MEM_TTL_MS,
                })));
            return Ok((root_data, bloom_filter));
        }

        // 3. L3 OCI GHCR
        match self.refresh_baseline_from_ghcr(env).await {
            Ok(res) => Ok(res),
            Err(e) => {
                if let Ok(Some(root_wrapper)) = kv
                    .get(&root_key)
                    .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
                    .await
                {
                    let root_data = root_wrapper.data;
                    let bloom_filter = Arc::new(FastBlockedBloomFilter::new_with_defaults(0));
                    return Ok((root_data, bloom_filter));
                }
                Err(e)
            }
        }
    }

    async fn refresh_baseline_from_ghcr(
        &self,
        env: &Env,
    ) -> Result<(ShardedArchCacheIndexData, Arc<FastBlockedBloomFilter>), WorkerStoreError> {
        let now = Date::now();
        let old_cached = WorkerState::global().mem_baseline_cache.load_full();

        let fetch_root = self
            .oci_client
            .get_sharded_root_index(&self.config.baseline_tag, &self.config.target_system)
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

        let bloom_filter = if !root_data.bloom_filter.blob_digest.is_empty() {
            match self
                .oci_client
                .get_bloom_filter(
                    &root_data.bloom_filter.blob_digest,
                    root_data.bloom_filter.num_entries,
                    root_data.bloom_filter.num_hashes,
                )
                .await
            {
                Ok(f) => {
                    self.set_remote_status(true, None);
                    Arc::new(f)
                }
                Err(e) => {
                    self.set_remote_status(
                        false,
                        Some(format!(
                            "GHCR bloom filter {}: {}",
                            root_data.bloom_filter.blob_digest, e
                        )),
                    );
                    Arc::new(FastBlockedBloomFilter::new_with_defaults(
                        root_data.bloom_filter.num_entries,
                    ))
                }
            }
        } else {
            Arc::new(FastBlockedBloomFilter::new_with_defaults(0))
        };

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
        let root_key = format!("baseline_root_v5_{}", self.config.target_system.as_str());
        let bloom_key = format!("baseline_bloom_v5_{}", self.config.target_system.as_str());

        let root_wrapper = KVCacheWrapper {
            data: root_data.clone(),
            last_refresh: now,
            manifest_digest: manifest_digest.clone(),
        };
        let _ = kv
            .put(&root_key, &root_wrapper)
            .map_err(|e| WorkerStoreError::KvPutFailed {
                key: root_key.clone(),
                message: e.to_string(),
            })?
            .execute()
            .await;

        let bloom_wrapper = BloomFilterKvWrapper {
            num_entries: bloom_filter.num_entries(),
            num_hashes: bloom_filter.num_hashes(),
            bytes_base64: STANDARD.encode(bloom_filter.to_bytes()),
            last_refresh: now,
            blob_digest: root_data.bloom_filter.blob_digest.clone(),
        };
        let _ = kv
            .put(&bloom_key, &bloom_wrapper)
            .map_err(|e| WorkerStoreError::KvPutFailed {
                key: bloom_key.clone(),
                message: e.to_string(),
            })?
            .execute()
            .await;

        WorkerState::global()
            .mem_baseline_cache
            .store(Some(Arc::new(CachedBaselineEntry {
                root: root_data.clone(),
                bloom_filter: bloom_filter.clone(),
                manifest_digest,
                expires_at: now + L1_MEM_TTL_MS,
            })));

        Ok((root_data, bloom_filter))
    }

    /// 强制刷新所有层级的索引并主动预热变更分片 (Tier 1 -> Tier 2 -> Tier 3)
    pub async fn force_refresh(&self, env: &Env) -> Result<RefreshResult, WorkerStoreError> {
        let mut errors = Vec::new();

        // 1. 获取旧的分片列表以供后续 diff 对比
        let old_shards = if let Some(cached) = WorkerState::global().mem_baseline_cache.load_full()
        {
            cached.root.shards.clone()
        } else if let Ok(kv) = env.kv("NIXCACHE_KV") {
            let root_key = format!("baseline_root_v5_{}", self.config.target_system.as_str());
            if let Ok(Some(wrapper)) = kv
                .get(&root_key)
                .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
                .await
            {
                wrapper.data.shards
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        WorkerState::global().clear_l1_caches();

        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Err(e) = self.refresh_session_from_ghcr(env, &tag).await {
                errors.push(format!("Session (run-{}): {}", run_id, e));
            }
        }

        if let Some(ref br) = self.config.branch_or_pr {
            let tag = format_branch_tag(br);
            if let Err(e) = self.refresh_session_from_ghcr(env, &tag).await {
                errors.push(format!("Branch ({}): {}", tag, e));
            }
        }

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
                    Arc::new(FastBlockedBloomFilter::new_with_defaults(0)),
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
                let digest = shard_desc.blob_digest.clone();
                warm_up_futures
                    .push(async move { self.get_shard_data(env, shard_id, &digest).await });
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
        let hot_count = WorkerState::global().hot_count.load(Ordering::Relaxed);

        let mut tier1_count = 0;
        let mut session_opt = None;
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Ok(Some((sess, _))) = self.get_session_data(env, &tag).await {
                tier1_count = sess.new_entries.len();
                session_opt = Some(sess);
            }
        }

        let mut tier2_count = 0;
        let mut branch_opt = None;
        if let Some(ref br) = self.config.branch_or_pr {
            let tag = format_branch_tag(br);
            if let Ok(Some((b_sess, _))) = self.get_session_data(env, &tag).await {
                tier2_count = b_sess.new_entries.len();
                branch_opt = Some(b_sess);
            }
        }

        let baseline_res = self.get_baseline_data(env).await;
        let (tier3_count, manifest_digest, generated) = match baseline_res {
            Ok((ref b, _)) => {
                let digest = WorkerState::global()
                    .mem_baseline_cache
                    .load_full()
                    .map(|c| c.manifest_digest.clone())
                    .unwrap_or_default();
                (b.total_entries(), digest, b.generated.clone())
            }
            Err(_) => {
                let root_key = format!("baseline_root_v5_{}", self.config.target_system.as_str());
                let kv_data = match env.kv("NIXCACHE_KV") {
                    Ok(kv) => kv
                        .get(&root_key)
                        .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
                        .await
                        .ok()
                        .flatten(),
                    Err(_) => None,
                };
                match kv_data {
                    Some(w) => (w.data.total_entries(), w.manifest_digest, w.data.generated),
                    None => (0, String::new(), String::new()),
                }
            }
        };

        let remote_state = WorkerState::global().remote_status.load();
        let remote_connected = remote_state.connected;
        let remote_error = remote_state.last_error.clone();

        let mut unique_hashes: HashSet<StoreHash> = HashSet::new();
        WorkerState::global().hot_entries.iter_sync(|k, _| {
            unique_hashes.insert((*k).clone());
            true
        });
        if let Some(s) = session_opt {
            unique_hashes.extend(s.new_entries.keys().cloned());
        }
        if let Some(b) = branch_opt {
            unique_hashes.extend(b.new_entries.keys().cloned());
        }

        let total_unique = unique_hashes.len() + tier3_count;

        RemoteStatus {
            remote_connected,
            remote_error,
            registry: self.config.registry.clone(),
            repo: self.config.repo.clone(),
            run_id: self.config.run_id,
            branch_or_pr: self.config.branch_or_pr.clone(),
            tier0_hot_entries: hot_count,
            tier1_session_entries: tier1_count,
            tier2_branch_entries: tier2_count,
            tier3_baseline_entries: tier3_count,
            total_unique_entries: total_unique,
            index_entries: total_unique,
            index_ttl: self.config.baseline_ttl_secs,
            session_ttl: self.config.session_ttl_secs,
            baseline_ttl: self.config.baseline_ttl_secs,
            upstream: self.config.upstream_caches.clone(),
            manifest_digest,
            generated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RemoteStatus, WorkerProxyConfig, format_branch_tag};
    use nixcache_core::{
        DeltaPatchData, FastBlockedBloomFilter, IndexEntry, NarDigest, NarInfoMeta,
        SCHEMA_VERSION_V5, ShardDataPayload, ShardedArchCacheIndexData, StoreHash, SystemArch,
        build_nar_lookup_map, diff_shard_descriptors,
    };
    use std::collections::HashMap;

    #[test]
    fn test_format_branch_tag() {
        assert_eq!(format_branch_tag("main"), "branch-main");
        assert_eq!(format_branch_tag("feat/test"), "branch-feat-test");
        assert_eq!(format_branch_tag("branch-xyz"), "branch-xyz");
        assert_eq!(format_branch_tag("pr-123"), "pr-123");
        assert_eq!(format_branch_tag("42"), "pr-42");
        assert_eq!(format_branch_tag("refs/heads/main"), "branch-main");
        assert_eq!(format_branch_tag("refs/pull/123/head"), "pr-123");
    }

    #[test]
    fn test_hot_registration_and_lookup() {
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
                    nar_basename: "pkg1.nar.xz".to_string(),
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

        let nar_map = build_nar_lookup_map(&entries);
        assert_eq!(nar_map.get("pkg1.nar.xz"), Some(&digest1));
    }

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
            run_id: Some(123456),
            branch_or_pr: Some("main".to_string()),
            tier0_hot_entries: 1,
            tier1_session_entries: 2,
            tier2_branch_entries: 0,
            tier3_baseline_entries: 3,
            total_unique_entries: 6,
            index_entries: 6,
            index_ttl: 300,
            session_ttl: 10,
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
        assert_eq!(config.session_ttl_secs, 10);
        assert_eq!(config.baseline_ttl_secs, 300);
        assert_eq!(config.target_system, SystemArch::X86_64Linux);
    }

    #[test]
    fn test_schema_v5_delta_and_sharding_serialization() {
        let mut delta = DeltaPatchData::new(12345, "job1", SystemArch::X86_64Linux);
        delta.active_gc_roots.push(StoreHash::default());

        assert_eq!(delta.version, SCHEMA_VERSION_V5);
        let delta_json = serde_json::to_string(&delta).unwrap();
        let loaded: DeltaPatchData = serde_json::from_str(&delta_json).unwrap();
        assert_eq!(loaded.run_id, 12345);

        let root = ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        assert_eq!(root.version, SCHEMA_VERSION_V5);
        assert_eq!(root.shards.len(), 1024);

        let shard = ShardDataPayload::new(0);
        assert_eq!(shard.version, SCHEMA_VERSION_V5);
    }

    #[test]
    fn test_bloom_filter_guard() {
        let hash1 = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let hash2 = StoreHash::parse("00000000000000000000000000000002").unwrap();

        let mut bloom = FastBlockedBloomFilter::new_with_defaults(10);
        bloom.insert(&hash1);

        assert!(bloom.contains(&hash1));
        assert!(!bloom.contains(&hash2));
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
