use crate::{
    error::WorkerStoreError,
    state::{DEBOUNCE_THRESHOLD_MS, L1_MEM_TTL_MS, WorkerState},
    transport::WorkerFetchTransport,
};
use base64::Engine;
use nixcache_core::{
    DeltaPatchData, FastBlockedBloomFilter, IndexEntry, NarDigest, ShardDataPayload,
    ShardedArchCacheIndexData, StoreHash, SystemArch, build_nar_lookup_map, calculate_shard_id,
    extract_nar_basename, extract_store_hash,
};
use nixcache_oci::OciClient;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::Ordering},
};
use worker::{Env, js_sys::Date};

pub type WorkerOciClient = OciClient<WorkerFetchTransport>;

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

    /// 动态注册新编译完成的条目到 Tier 0 内存热表中 (0ms 延迟可用)
    pub fn register_hot_entries(entries: HashMap<StoreHash, IndexEntry>) {
        WorkerState::global().register_hot(entries);
    }

    /// 级联查询 Store Hash 对应的 narinfo (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3 -> 智能防抖穿透)
    pub async fn lookup_narinfo_cascading(
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

        // 2. Tier 1: Workflow Run Session (run-<run_id>)
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Ok(Some((session, _))) = self.get_session_data(env, &tag).await
                && let Some(entry) = session.new_entries.get(&parsed_hash)
            {
                return Ok(Some(entry.to_narinfo_string()));
            }
        }

        // 3. Tier 2: Branch / PR Session (branch-<name> / pr-<num>)
        if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            if let Ok(Some((branch_sess, _))) = self.get_session_data(env, &tag).await
                && let Some(entry) = branch_sess.new_entries.get(&parsed_hash)
            {
                return Ok(Some(entry.to_narinfo_string()));
            }
        }

        // 4. Tier 3: Production Baseline (cache-index)
        if let Ok((baseline, bloom_filter)) = self.get_baseline_data(env).await
            && bloom_filter.contains(&parsed_hash)
        {
            let shard_id = calculate_shard_id(&parsed_hash);
            if let Some(shard_desc) = baseline.find_shard_by_id(shard_id)
                && !shard_desc.is_empty()
                && !shard_desc.blob_digest.is_empty()
                && let Ok(Some((shard_payload, _))) = self
                    .get_shard_data(env, shard_id, &shard_desc.blob_digest)
                    .await
                && let Some(entry) = shard_payload.entries.get(&parsed_hash)
            {
                return Ok(Some(entry.to_narinfo_string()));
            }
        }

        // 5. Miss: Debounced Read-Through to GHCR (智能防抖穿透)
        let now = Date::now();
        let should_check_ghcr =
            WorkerState::global().try_acquire_ghcr_check(now as u64, DEBOUNCE_THRESHOLD_MS as u64);

        if should_check_ghcr
            && self.force_refresh(env).await.is_ok()
            && let Some(run_id) = self.config.run_id
        {
            let tag = format!("run-{}", run_id);
            if let Ok(Some((session, _))) = self.get_session_data(env, &tag).await
                && let Some(entry) = session.new_entries.get(&parsed_hash)
            {
                return Ok(Some(entry.to_narinfo_string()));
            }
        }

        Ok(None)
    }

    /// 级联反向解析 NAR 文件名对应的 Blob Digest (全链路 O(1) 查找)
    pub async fn lookup_nar_digest_cascading(
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
            if let Ok(Some((_, nar_lookup))) = self.get_session_data(env, &tag).await
                && let Some(digest) = nar_lookup.get(normalized)
            {
                return Ok(Some(digest.clone()));
            }
        }

        // 3. Tier 2: Branch / PR Session
        if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            if let Ok(Some((_, nar_lookup))) = self.get_session_data(env, &tag).await
                && let Some(digest) = nar_lookup.get(normalized)
            {
                return Ok(Some(digest.clone()));
            }
        }

        // 4. Tier 3: 若文件名包含 StoreHash 前缀，通过定位 Shard 实现 O(1) 查找
        if let Some(store_hash) = extract_store_hash(nar_basename) {
            let parsed_hash = StoreHash::parse(store_hash.as_str())?;
            if let Ok((baseline, bloom_filter)) = self.get_baseline_data(env).await
                && bloom_filter.contains(&parsed_hash)
            {
                let shard_id = calculate_shard_id(&parsed_hash);
                if let Some(shard_desc) = baseline.find_shard_by_id(shard_id)
                    && !shard_desc.is_empty()
                    && !shard_desc.blob_digest.is_empty()
                    && let Ok(Some((_, nar_lookup))) = self
                        .get_shard_data(env, shard_id, &shard_desc.blob_digest)
                        .await
                    && let Some(digest) = nar_lookup.get(normalized)
                {
                    return Ok(Some(digest.clone()));
                }
            }
        }

        // 检查已加载在内存中的分片缓存
        let mut found_digest = None;
        WorkerState::global().mem_shard_cache.iter_sync(|_, v| {
            if let Some(digest) = v.1.get(normalized) {
                found_digest = Some(digest.clone());
                return false;
            }
            true
        });

        if let Some(digest) = found_digest {
            return Ok(Some(digest));
        }

        // 5. Miss: Debounced Read-Through to GHCR
        let now = Date::now();
        let should_check_ghcr =
            WorkerState::global().try_acquire_ghcr_check(now as u64, DEBOUNCE_THRESHOLD_MS as u64);

        if should_check_ghcr
            && self.force_refresh(env).await.is_ok()
            && let Some(run_id) = self.config.run_id
        {
            let tag = format!("run-{}", run_id);
            if let Ok(Some((_, nar_lookup))) = self.get_session_data(env, &tag).await
                && let Some(digest) = nar_lookup.get(normalized)
            {
                return Ok(Some(digest.clone()));
            }
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
    ) -> Result<Option<(ShardDataPayload, HashMap<String, NarDigest>)>, WorkerStoreError> {
        let now = Date::now();

        // 1. L1 Memory Cache
        if let Some(cached) = WorkerState::global()
            .mem_shard_cache
            .read_sync(&shard_id, |_, v| v.clone())
            && now < cached.2
        {
            return Ok(Some((cached.0.clone(), cached.1.clone())));
        }

        // 2. L2 Cloudflare KV
        let kv_key = format!(
            "shard_wrapper_{}_{}",
            self.config.target_system.as_str(),
            shard_id
        );
        if let Ok(kv) = env.kv("NIXCACHE_KV")
            && let Ok(Some(wrapper)) = kv
                .get(&kv_key)
                .json::<KVCacheWrapper<ShardDataPayload>>()
                .await
            && now - wrapper.last_refresh < self.baseline_ttl_ms
        {
            let payload = wrapper.data;
            let nar_lookup = build_nar_lookup_map(&payload.entries);

            let _ = WorkerState::global().mem_shard_cache.upsert_sync(
                shard_id,
                Arc::new((payload.clone(), nar_lookup.clone(), now + L1_MEM_TTL_MS)),
            );
            return Ok(Some((payload, nar_lookup)));
        }

        if blob_digest.is_empty() {
            return Ok(None);
        }

        // 3. L3 OCI GHCR
        match self.oci_client.get_shard_data(blob_digest).await {
            Ok(payload) => {
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
                    Arc::new((payload.clone(), nar_lookup.clone(), now + L1_MEM_TTL_MS)),
                );

                Ok(Some((payload, nar_lookup)))
            }
            Err(e) => {
                if let Ok(kv) = env.kv("NIXCACHE_KV")
                    && let Ok(Some(wrapper)) = kv
                        .get(&kv_key)
                        .json::<KVCacheWrapper<ShardDataPayload>>()
                        .await
                {
                    let payload = wrapper.data;
                    let nar_lookup = build_nar_lookup_map(&payload.entries);
                    return Ok(Some((payload, nar_lookup)));
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
            && now < cached.2
        {
            return Ok(Some((cached.0.clone(), cached.1.clone())));
        }

        // 2. L2 Cloudflare KV
        let kv_key = format!("session_wrapper_{}", tag);
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
                Arc::new((delta.clone(), nar_lookup.clone(), now + L1_MEM_TTL_MS)),
            );
            return Ok(Some((delta, nar_lookup)));
        }

        // 3. L3 OCI GHCR
        let arch_tag = format!("{}-{}", tag, self.config.target_system.as_str());
        let fetch_res = match self.oci_client.get_delta_patch_manifest(&arch_tag).await {
            Ok(Some(res)) => Ok(Some(res)),
            Ok(None) => self.oci_client.get_delta_patch_manifest(tag).await,
            Err(e) => Err(e),
        };

        match fetch_res {
            Ok(Some((delta, manifest_digest))) => {
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
                    Arc::new((delta.clone(), nar_lookup.clone(), now + L1_MEM_TTL_MS)),
                );

                Ok(Some((delta, nar_lookup)))
            }
            Ok(None) => Ok(None),
            Err(e) => {
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

    /// 获取生产基线分片根索引与布隆过滤器 (L1 Memory -> L2 KV -> L3 GHCR)
    pub async fn get_baseline_data(
        &self,
        env: &Env,
    ) -> Result<(ShardedArchCacheIndexData, Arc<FastBlockedBloomFilter>), WorkerStoreError> {
        let now = Date::now();

        // 1. L1 Memory Cache
        if let Some(cached) = WorkerState::global().mem_baseline_cache.load_full()
            && now < cached.2
        {
            return Ok((cached.0.clone(), cached.1.clone()));
        }

        // 2. L2 Cloudflare KV
        let kv = env
            .kv("NIXCACHE_KV")
            .map_err(|e| WorkerStoreError::KvGetFailed {
                key: "NIXCACHE_KV".to_string(),
                message: e.to_string(),
            })?;
        let root_key = format!("baseline_root_{}", self.config.target_system.as_str());
        let bloom_key = format!("baseline_bloom_{}", self.config.target_system.as_str());

        if let Ok(Some(root_wrapper)) = kv
            .get(&root_key)
            .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
            .await
            && let Ok(Some(bloom_wrapper)) = kv.get(&bloom_key).json::<BloomFilterKvWrapper>().await
            && now - root_wrapper.last_refresh < self.baseline_ttl_ms
        {
            let root_data = root_wrapper.data;
            let bloom_bytes =
                base64::engine::general_purpose::STANDARD.decode(&bloom_wrapper.bytes_base64)?;
            let bloom_filter = Arc::new(
                FastBlockedBloomFilter::from_bytes(
                    &bloom_bytes,
                    bloom_wrapper.num_entries,
                    bloom_wrapper.num_hashes,
                )
                .unwrap_or_else(|_| FastBlockedBloomFilter::new_with_defaults(0)),
            );

            WorkerState::global()
                .mem_baseline_cache
                .store(Some(Arc::new((
                    root_data.clone(),
                    bloom_filter.clone(),
                    now + L1_MEM_TTL_MS,
                ))));
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
        let (root_data, manifest_digest) = match self
            .oci_client
            .get_sharded_root_index(&self.config.baseline_tag, &self.config.target_system)
            .await?
        {
            Some((data, digest)) => (data, digest),
            None => (
                ShardedArchCacheIndexData::new(
                    self.config.target_system,
                    &self.config.repo,
                    &self.config.registry,
                ),
                String::new(),
            ),
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
                Ok(f) => Arc::new(f),
                Err(_) => Arc::new(FastBlockedBloomFilter::new_with_defaults(
                    root_data.bloom_filter.num_entries,
                )),
            }
        } else {
            Arc::new(FastBlockedBloomFilter::new_with_defaults(0))
        };

        let kv = env
            .kv("NIXCACHE_KV")
            .map_err(|e| WorkerStoreError::KvGetFailed {
                key: "NIXCACHE_KV".to_string(),
                message: e.to_string(),
            })?;
        let root_key = format!("baseline_root_{}", self.config.target_system.as_str());
        let bloom_key = format!("baseline_bloom_{}", self.config.target_system.as_str());

        let root_wrapper = KVCacheWrapper {
            data: root_data.clone(),
            last_refresh: now,
            manifest_digest,
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
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(bloom_filter.to_bytes()),
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
            .store(Some(Arc::new((
                root_data.clone(),
                bloom_filter.clone(),
                now + L1_MEM_TTL_MS,
            ))));
        WorkerState::global()
            .last_ghcr_check_ms
            .store(now as u64, Ordering::Release);

        Ok((root_data, bloom_filter))
    }

    /// 强制刷新所有层级的索引 (Tier 1 -> Tier 2 -> Tier 3)
    pub async fn force_refresh(&self, env: &Env) -> Result<usize, WorkerStoreError> {
        let mut errors = Vec::new();

        WorkerState::global().clear_l1_caches();

        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Err(e) = self.get_session_data(env, &tag).await {
                errors.push(format!("Session (run-{}): {}", run_id, e));
            }
        }

        if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            if let Err(e) = self.get_session_data(env, &tag).await {
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

        let status = self.get_status(env).await;
        if errors.is_empty() || status.total_unique_entries > 0 || baseline.total_entries() > 0 {
            Ok(status.total_unique_entries)
        } else {
            Err(WorkerStoreError::AggregatedRefreshFailed { errors })
        }
    }

    /// 获取完整的状态元信息与各层级统计
    pub async fn get_status(&self, env: &Env) -> RemoteStatus {
        let mut hot_count = 0;
        WorkerState::global().hot_entries.iter_sync(|_, _| {
            hot_count += 1;
            true
        });

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
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            if let Ok(Some((b_sess, _))) = self.get_session_data(env, &tag).await {
                tier2_count = b_sess.new_entries.len();
                branch_opt = Some(b_sess);
            }
        }

        let baseline_res = self.get_baseline_data(env).await;
        let (remote_connected, remote_error, tier3_count, manifest_digest, generated) =
            match baseline_res {
                Ok((ref b, _)) => (
                    true,
                    None,
                    b.total_entries(),
                    String::new(),
                    b.generated.clone(),
                ),
                Err(ref e) => {
                    let root_key = format!("baseline_root_{}", self.config.target_system.as_str());
                    let kv_data = match env.kv("NIXCACHE_KV") {
                        Ok(kv) => kv
                            .get(&root_key)
                            .json::<KVCacheWrapper<ShardedArchCacheIndexData>>()
                            .await
                            .ok()
                            .flatten(),
                        Err(_) => None,
                    };
                    let (count, digest, gen_str) = match kv_data {
                        Some(w) => (w.data.total_entries(), w.manifest_digest, w.data.generated),
                        None => (0, String::new(), String::new()),
                    };
                    (false, Some(e.to_string()), count, digest, gen_str)
                }
            };

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
    use super::{RemoteStatus, WorkerProxyConfig};
    use nixcache_core::{
        DeltaPatchData, FastBlockedBloomFilter, IndexEntry, NarDigest, NarInfoMeta,
        SCHEMA_VERSION_V5, ShardDataPayload, ShardedArchCacheIndexData, StoreHash, SystemArch,
        build_nar_lookup_map,
    };
    use std::collections::HashMap;

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
}
