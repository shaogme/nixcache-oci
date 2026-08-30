use arc_swap::ArcSwap;
use nixcache_core::{
    DeltaPatchData, FastBlockedBloomFilter, IndexEntry, NarDigest, ShardDataPayload,
    ShardedArchCacheIndexData, StoreHash, SystemArch, build_nar_lookup_map, calculate_shard_id,
    diff_shard_descriptors, extract_nar_basename, extract_store_hash,
};
use nixcache_oci::{CacheLayerMediaTypeV5, DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec, OciClient};
use nixcache_oci_backend::{ReqwestTransport, create_tokio_reqwest_client};
use scc::HashMap as SccHashMap;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::fs;
use tracing::{error, info, warn};

pub fn detect_current_system() -> SystemArch {
    SystemArch::detect_current()
}

#[derive(Clone, Debug)]
pub struct CascadingProxyConfig {
    pub repo: String,
    pub registry: String,
    pub run_id: Option<u64>,
    pub branch_or_pr: Option<String>,
    pub baseline_tag: String,
    pub upstream_caches: Vec<String>,
    pub session_ttl: Duration,
    pub baseline_ttl: Duration,
    pub index_dir: PathBuf,
    pub target_system: SystemArch,
}

impl Default for CascadingProxyConfig {
    fn default() -> Self {
        Self {
            repo: String::new(),
            registry: "ghcr.io".to_string(),
            run_id: None,
            branch_or_pr: None,
            baseline_tag: "cache-index".to_string(),
            upstream_caches: vec!["https://cache.nixos.org".to_string()],
            session_ttl: Duration::from_secs(10),
            baseline_ttl: Duration::from_secs(300),
            index_dir: PathBuf::from("/tmp"),
            target_system: detect_current_system(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct StatusEntryCounts {
    pub tier0_hot_entries: usize,
    pub tier1_session_entries: usize,
    pub tier2_branch_entries: usize,
    pub tier3_baseline_entries: usize,
    pub total_unique_entries: usize,
}

#[derive(Clone, Debug, Default)]
struct RemoteStatus {
    connected: bool,
    error: Option<String>,
}

/// 带有 O(1) 反向 NAR 映射表的会话增量缓存模型
#[derive(Clone, Debug)]
pub struct CachedSession {
    pub delta: DeltaPatchData,
    pub nar_lookup: HashMap<String, NarDigest>,
}

impl CachedSession {
    pub fn new(delta: DeltaPatchData) -> Self {
        let nar_lookup = build_nar_lookup_map(&delta.new_entries);
        Self { delta, nar_lookup }
    }
}

/// 带有布隆过滤器与分片元数据的生产基线缓存模型
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct CachedBaseline {
    pub root: ShardedArchCacheIndexData,
    pub bloom_filter: Arc<FastBlockedBloomFilter>,
    pub manifest_digest: String,
}

impl CachedBaseline {
    pub fn new(
        root: ShardedArchCacheIndexData,
        bloom_filter: Arc<FastBlockedBloomFilter>,
        manifest_digest: String,
    ) -> Self {
        Self {
            root,
            bloom_filter,
            manifest_digest,
        }
    }
}

pub type ShardCacheEntry = (
    Arc<ShardDataPayload>,
    Arc<HashMap<String, NarDigest>>,
    Instant,
);

#[derive(Clone)]
pub struct CacheIndex {
    config: CascadingProxyConfig,
    oci_client: OciClient<ReqwestTransport>,
    // Tier 0: 本地内存热注册表 (In-Memory Hot Registry)
    hot_entries: Arc<SccHashMap<StoreHash, Arc<IndexEntry>>>,
    hot_nar_lookup: Arc<SccHashMap<String, NarDigest>>,
    hot_count: Arc<AtomicUsize>,
    // Tier 1 & Tier 2: 工作流及分支/PR 会话缓存 (key 为 tag 如 "run-123", "branch-main")
    session_cache: Arc<SccHashMap<String, (Arc<CachedSession>, Instant)>>,
    // Tier 3: 生产主干分片根索引元数据缓存 (key 为 config.baseline_tag-system)
    baseline_cache: Arc<SccHashMap<String, (Arc<CachedBaseline>, Instant)>>,
    // Tier 3: 二级分片缓存 (key 为 shard_id 0..1023)
    shard_cache: Arc<SccHashMap<u16, ShardCacheEntry>>,
    // 远端连接与错误状态 (RCU 无锁指针替换)
    remote_status: Arc<ArcSwap<RemoteStatus>>,
}

impl CacheIndex {
    pub fn with_config(config: CascadingProxyConfig, github_token: &str) -> Self {
        let oci_client =
            create_tokio_reqwest_client(&config.registry, &config.repo, github_token, false);

        Self {
            config,
            oci_client,
            hot_entries: Arc::new(SccHashMap::new()),
            hot_nar_lookup: Arc::new(SccHashMap::new()),
            hot_count: Arc::new(AtomicUsize::new(0)),
            session_cache: Arc::new(SccHashMap::new()),
            baseline_cache: Arc::new(SccHashMap::new()),
            shard_cache: Arc::new(SccHashMap::new()),
            remote_status: Arc::new(ArcSwap::from_pointee(RemoteStatus::default())),
        }
    }

    pub fn config(&self) -> &CascadingProxyConfig {
        &self.config
    }

    pub fn registry(&self) -> &str {
        &self.config.registry
    }

    pub fn upstream_caches(&self) -> &[String] {
        &self.config.upstream_caches
    }

    #[inline]
    pub fn remote_status(&self) -> (bool, Option<String>) {
        let guard = self.remote_status.load();
        (guard.connected, guard.error.clone())
    }

    #[inline]
    pub fn set_remote_status(&self, connected: bool, error: Option<String>) {
        self.remote_status
            .store(Arc::new(RemoteStatus { connected, error }));
    }

    /// Tier 0: 动态注册新编译完成的条目到内存热表和 NAR 查找表中 (0ms 延迟可用)
    pub async fn register_hot_entries(&self, entries: HashMap<StoreHash, IndexEntry>) {
        let count = entries.len();
        let nar_map = build_nar_lookup_map(&entries);

        for (key, entry) in entries {
            let _ = self.hot_entries.upsert_sync(key, Arc::new(entry));
        }
        for (nar_name, digest) in nar_map {
            let _ = self.hot_nar_lookup.upsert_sync(nar_name, digest);
        }
        self.hot_count.fetch_add(count, Ordering::Relaxed);

        info!(
            "[nixcache-proxy] Registered {} entries into Tier 0 In-Memory Hot Registry (Total: {})",
            count,
            self.hot_count.load(Ordering::Relaxed)
        );
    }

    /// 按需拉取或获取单个分片数据 (通过 LRU/SccHashMap 缓存)
    pub async fn get_shard_data(
        &self,
        shard_id: u16,
        blob_digest: &str,
    ) -> Option<Arc<ShardDataPayload>> {
        if let Some((cached_payload, _, exp)) = self
            .shard_cache
            .read_sync(&shard_id, |_, v| (v.0.clone(), v.1.clone(), v.2))
            && exp > Instant::now()
        {
            return Some(cached_payload);
        }

        if blob_digest.is_empty() {
            return None;
        }

        match self.oci_client.get_shard_data(blob_digest).await {
            Ok(payload) => {
                let nar_map = build_nar_lookup_map(&payload.entries);
                let arc_payload = Arc::new(payload);
                let _ = self.shard_cache.upsert_sync(
                    shard_id,
                    (
                        arc_payload.clone(),
                        Arc::new(nar_map),
                        Instant::now() + self.config.baseline_ttl,
                    ),
                );
                Some(arc_payload)
            }
            Err(e) => {
                error!(
                    "[nixcache-proxy] Failed to fetch shard data for shard {} (digest: {}): {}",
                    shard_id, blob_digest, e
                );
                None
            }
        }
    }

    /// 级联查询 Store Hash 对应的 IndexEntry (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3)
    pub async fn lookup(&self, store_hash: &str) -> Option<IndexEntry> {
        let parsed_hash = StoreHash::parse(store_hash).ok()?;

        // Tier 0: 内存热注册表
        if let Some(entry) = self
            .hot_entries
            .read_sync(&parsed_hash, |_, v| (**v).clone())
        {
            return Some(entry);
        }

        // Tier 1: 工作流会话 (run-<run_id>)
        if self.config.run_id.is_some()
            && let Some(session) = self.get_session_data().await
            && let Some(entry) = session.delta.new_entries.get(&parsed_hash)
        {
            return Some(entry.clone());
        }

        // Tier 2: 分支/PR 会话
        if self.config.branch_or_pr.is_some()
            && let Some(branch) = self.get_branch_data().await
            && let Some(entry) = branch.delta.new_entries.get(&parsed_hash)
        {
            return Some(entry.clone());
        }

        // Tier 3: 生产主干基线分片索引
        let baseline = self.get_baseline_data().await;

        // Step 1: 布隆过滤器前置守卫 (False 判定则 100% 不存在，0ms 快速直通回退)
        if !baseline.bloom_filter.contains(&parsed_hash) {
            return None;
        }

        // Step 2: 定位分片 ID (0..1023)
        let shard_id = calculate_shard_id(&parsed_hash);
        let shard_desc = baseline.root.find_shard_by_id(shard_id)?;
        if shard_desc.is_empty() || shard_desc.blob_digest.is_empty() {
            return None;
        }

        // Step 3: 按需获取分片并在分片内部完成 O(1) 检索
        let shard_payload = self
            .get_shard_data(shard_id, &shard_desc.blob_digest)
            .await?;
        shard_payload.entries.get(&parsed_hash).cloned()
    }

    /// 级联反向解析 NAR 文件名对应的 Blob Digest (全链路 O(1) 检索)
    pub async fn find_nar_digest(&self, nar_basename: &str) -> Option<NarDigest> {
        let normalized = extract_nar_basename(nar_basename);

        // Tier 0: O(1) 查找
        if let Some(digest) = self.hot_nar_lookup.read_sync(normalized, |_, v| v.clone()) {
            return Some(digest);
        }

        // Tier 1: O(1) 查找
        if self.config.run_id.is_some()
            && let Some(session) = self.get_session_data().await
            && let Some(digest) = session.nar_lookup.get(normalized)
        {
            return Some(digest.clone());
        }

        // Tier 2: O(1) 查找
        if self.config.branch_or_pr.is_some()
            && let Some(branch) = self.get_branch_data().await
            && let Some(digest) = branch.nar_lookup.get(normalized)
        {
            return Some(digest.clone());
        }

        // Tier 3: 若文件名包含 StoreHash 前缀，通过定位 Shard 实现 O(1) 查找
        if let Some(store_hash) = extract_store_hash(nar_basename)
            && let Some(entry) = self.lookup(store_hash.as_str()).await
        {
            return Some(entry.nar_digest);
        }

        // 检查当前所有已加载分片的内存 NAR 查找表
        let mut found_digest = None;
        self.shard_cache.iter_sync(|_, v| {
            if let Some(digest) = v.1.get(normalized) {
                found_digest = Some(digest.clone());
                return false;
            }
            true
        });

        found_digest
    }

    /// 获取有效的签名公钥 (按会话 -> 分支 -> 基线优先级查找)
    pub async fn get_public_key(&self) -> Option<String> {
        if self.config.run_id.is_some()
            && let Some(session) = self.get_session_data().await
            && !session.delta.new_entries.is_empty()
        {
            // 如果会话中没有显式 public_key 字段，可回退到基线
        }

        let baseline = self.get_baseline_data().await;
        if !baseline.root.public_key.is_empty() {
            Some(baseline.root.public_key.clone())
        } else {
            None
        }
    }

    /// 获取各层级的条目统计信息
    pub async fn get_entry_counts(&self) -> StatusEntryCounts {
        let hot_count = self.hot_count.load(Ordering::Relaxed);

        let session_count = if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            self.session_cache
                .read_sync(&tag, |_, v| v.0.delta.new_entries.len())
                .unwrap_or(0)
        } else {
            0
        };

        let branch_count = if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            self.session_cache
                .read_sync(&tag, |_, v| v.0.delta.new_entries.len())
                .unwrap_or(0)
        } else {
            0
        };

        let cache_key = format!(
            "{}-{}",
            self.config.baseline_tag,
            self.config.target_system.as_str()
        );
        let baseline_count = self
            .baseline_cache
            .read_sync(&cache_key, |_, v| v.0.root.total_entries())
            .or_else(|| {
                self.baseline_cache
                    .read_sync(&self.config.baseline_tag, |_, v| v.0.root.total_entries())
            })
            .unwrap_or(0);

        let mut all_unique_hashes = HashSet::new();
        self.hot_entries.iter_sync(|k, _| {
            all_unique_hashes.insert((*k).clone());
            true
        });

        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Some(sess) = self.session_cache.read_sync(&tag, |_, v| v.0.clone()) {
                all_unique_hashes.extend(sess.delta.new_entries.keys().cloned());
            }
        }

        if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            if let Some(branch) = self.session_cache.read_sync(&tag, |_, v| v.0.clone()) {
                all_unique_hashes.extend(branch.delta.new_entries.keys().cloned());
            }
        }

        let total_unique = if baseline_count > 0 {
            all_unique_hashes.len() + baseline_count
        } else {
            all_unique_hashes.len()
        };

        StatusEntryCounts {
            tier0_hot_entries: hot_count,
            tier1_session_entries: session_count,
            tier2_branch_entries: branch_count,
            tier3_baseline_entries: baseline_count,
            total_unique_entries: total_unique,
        }
    }

    pub async fn get_session_data(&self) -> Option<Arc<CachedSession>> {
        let run_id = self.config.run_id?;
        let tag = format!("run-{}", run_id);
        self.fetch_or_get_session(&tag).await
    }

    pub async fn get_branch_data(&self) -> Option<Arc<CachedSession>> {
        let branch_or_pr = self.config.branch_or_pr.as_ref()?;
        let tag = if branch_or_pr.starts_with("pr-") || branch_or_pr.starts_with("branch-") {
            branch_or_pr.to_string()
        } else {
            format!("branch-{}", branch_or_pr.replace(['/', ':'], "-"))
        };
        self.fetch_or_get_session(&tag).await
    }

    /// 获取生产基线全局分片索引与布隆过滤器 (Schema v5 Root)
    pub async fn get_baseline_data(&self) -> Arc<CachedBaseline> {
        let tag = &self.config.baseline_tag;
        let system = &self.config.target_system;
        let cache_key = format!("{}-{}", tag, system.as_str());

        if let Some((cached, exp)) = self
            .baseline_cache
            .read_sync(&cache_key, |_, v| (v.0.clone(), v.1))
            && exp > Instant::now()
        {
            return cached;
        }

        let tag_str = tag.clone();
        let system_clone = *system;
        info!(
            "[nixcache-proxy] Refreshing Tier 3 Sharded Baseline Index (Tag: {}, System: {})...",
            tag_str, system_clone
        );
        let mut fetched_baseline = None;

        match self
            .oci_client
            .get_sharded_root_index(&tag_str, &system_clone)
            .await
        {
            Ok(Some((root_data, manifest_digest))) => {
                self.set_remote_status(true, None);

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
                        Err(e) => {
                            warn!(
                                "[nixcache-proxy] Failed to fetch bloom filter blob {}: {}",
                                root_data.bloom_filter.blob_digest, e
                            );
                            Arc::new(FastBlockedBloomFilter::new_with_defaults(
                                root_data.bloom_filter.num_entries,
                            ))
                        }
                    }
                } else {
                    Arc::new(FastBlockedBloomFilter::new_with_defaults(0))
                };

                // 增量失效发生变更的分片
                if let Some((old_baseline, _)) = self
                    .baseline_cache
                    .read_sync(&cache_key, |_, v| (v.0.clone(), v.1))
                    && old_baseline.root.merkle_root != root_data.merkle_root
                {
                    let changed =
                        diff_shard_descriptors(&old_baseline.root.shards, &root_data.shards);
                    for sid in changed {
                        let _ = self.shard_cache.remove_sync(&sid);
                    }
                }

                // 保存本地单架构根索引与布隆过滤器备份
                let file_name = format!("cache-index-{}.json.zst", system_clone.as_str());
                let file_path = self.config.index_dir.join(&file_name);
                if let Some(parent) = file_path.parent() {
                    let _ = fs::create_dir_all(parent).await;
                }
                if let Ok(bytes) =
                    IndexCodec::encode_zstd(&root_data, DEFAULT_ZSTD_COMPRESSION_LEVEL)
                {
                    if let Err(e) = fs::write(&file_path, &bytes).await {
                        error!("[nixcache-proxy] Failed to write backup index: {}", e);
                    } else {
                        info!(
                            "[nixcache-proxy] Backup cache index saved to {:?}",
                            file_path
                        );
                    }
                }

                fetched_baseline = Some(CachedBaseline::new(
                    root_data,
                    bloom_filter,
                    manifest_digest,
                ));
            }
            Ok(None) => {
                info!(
                    "[nixcache-proxy] Baseline tag {} not found on registry for system {}.",
                    tag_str, system_clone
                );
                self.set_remote_status(true, None);
            }
            Err(e) => {
                error!(
                    "[nixcache-proxy] Failed to fetch sharded baseline index: {}",
                    e
                );
                self.set_remote_status(false, Some(format!("Failed to connect to remote: {}", e)));
            }
        }

        if fetched_baseline.is_none() {
            let arch_backup = self
                .config
                .index_dir
                .join(format!("cache-index-{}.json.zst", system_clone.as_str()));

            if arch_backup.exists() {
                match fs::read(&arch_backup).await {
                    Ok(bytes) => {
                        if let Ok(root_data) = IndexCodec::decode_zstd::<ShardedArchCacheIndexData>(
                            &bytes,
                            CacheLayerMediaTypeV5::ROOT_INDEX_V5_ZSTD,
                        ) {
                            info!(
                                "[nixcache-proxy] Loaded backup sharded root index from {:?}",
                                arch_backup
                            );
                            fetched_baseline = Some(CachedBaseline::new(
                                root_data,
                                Arc::new(FastBlockedBloomFilter::new_with_defaults(0)),
                                String::new(),
                            ));
                        }
                    }
                    Err(e) => {
                        error!("[nixcache-proxy] Failed to read backup cache index: {}", e);
                    }
                }
            }
        }

        let result = if let Some(baseline) = fetched_baseline {
            info!(
                "[nixcache-proxy] Baseline index refreshed successfully with {} entries for {}.",
                baseline.root.total_entries(),
                system_clone
            );
            Arc::new(baseline)
        } else {
            Arc::new(CachedBaseline::new(
                ShardedArchCacheIndexData::new(
                    system_clone,
                    &self.config.repo,
                    &self.config.registry,
                ),
                Arc::new(FastBlockedBloomFilter::new_with_defaults(0)),
                String::new(),
            ))
        };

        let _ = self.baseline_cache.upsert_sync(
            cache_key,
            (result.clone(), Instant::now() + self.config.baseline_ttl),
        );
        result
    }

    /// 强制刷新所有层级的索引
    pub async fn force_refresh(&self) -> Result<usize, String> {
        let mut errs = Vec::new();
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            let _ = self.session_cache.remove_sync(&tag);
            if self.fetch_or_get_session(&tag).await.is_none() {
                errs.push(format!("Session: failed to refresh tag {}", tag));
            }
        }
        if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            let _ = self.session_cache.remove_sync(&tag);
            if self.fetch_or_get_session(&tag).await.is_none() {
                errs.push(format!("Branch: failed to refresh tag {}", tag));
            }
        }

        let cache_key = format!(
            "{}-{}",
            self.config.baseline_tag,
            self.config.target_system.as_str()
        );
        let _ = self.baseline_cache.remove_sync(&cache_key);
        let _ = self.baseline_cache.remove_sync(&self.config.baseline_tag);
        self.shard_cache.clear_sync();

        let baseline = self.get_baseline_data().await;
        if baseline.root.total_entries() == 0 {
            let (_, remote_err) = self.remote_status();
            if let Some(e) = remote_err {
                errs.push(format!("Baseline: {}", e));
            }
        }

        let counts = self.get_entry_counts().await;
        if errs.is_empty() || counts.total_unique_entries > 0 {
            Ok(counts.total_unique_entries)
        } else {
            Err(errs.join("; "))
        }
    }

    async fn fetch_or_get_session(&self, tag: &str) -> Option<Arc<CachedSession>> {
        if let Some((session, exp)) = self.session_cache.read_sync(tag, |_, v| (v.0.clone(), v.1))
            && exp > Instant::now()
        {
            return Some(session);
        }

        let tag_str = tag.to_string();
        let system_clone = self.config.target_system;
        info!(
            "[nixcache-proxy] Refreshing Session Manifest (Tag: {}, System: {})...",
            tag_str, system_clone
        );

        let arch_tag = format!("{}-{}", tag_str, system_clone.as_str());
        let fetch_res = match self.oci_client.get_delta_patch_manifest(&arch_tag).await {
            Ok(Some(res)) => Some(res),
            Ok(None) => match self.oci_client.get_delta_patch_manifest(&tag_str).await {
                Ok(res) => res,
                Err(e) => {
                    warn!(
                        "[nixcache-proxy] Failed to fetch session delta {}: {}",
                        tag_str, e
                    );
                    None
                }
            },
            Err(e) => {
                warn!(
                    "[nixcache-proxy] Failed to fetch session delta {}: {}",
                    arch_tag, e
                );
                None
            }
        };

        match fetch_res {
            Some((delta, _)) => {
                self.set_remote_status(true, None);
                let cached = Arc::new(CachedSession::new(delta));
                let _ = self.session_cache.upsert_sync(
                    tag_str,
                    (cached.clone(), Instant::now() + self.config.session_ttl),
                );
                Some(cached)
            }
            None => {
                info!(
                    "[nixcache-proxy] Session tag {} not found on remote for system {}.",
                    tag_str, system_clone
                );
                self.set_remote_status(true, None);
                None
            }
        }
    }

    #[cfg(test)]
    pub async fn update_sharded_baseline_in_memory(
        &self,
        mut root: ShardedArchCacheIndexData,
        shards: Vec<ShardDataPayload>,
        bloom: Option<FastBlockedBloomFilter>,
    ) {
        use nixcache_core::{BloomFilterManifest, ShardDescriptor};

        let cache_key = format!(
            "{}-{}",
            self.config.baseline_tag,
            self.config.target_system.as_str()
        );

        let mut bloom_filter =
            bloom.unwrap_or_else(|| FastBlockedBloomFilter::new_with_defaults(100));
        for shard in &shards {
            for hash in shard.entries.keys() {
                bloom_filter.insert(hash);
            }
            let shard_desc = ShardDescriptor::new(
                shard.shard_id,
                format!("sha256:mock_shard_{}", shard.shard_id),
                100,
                200,
                shard.entries.len(),
                shard.compute_merkle_hash(),
            );
            if let Some(d) = root.find_shard_by_id_mut(shard.shard_id) {
                *d = shard_desc;
            }
            let nar_map = build_nar_lookup_map(&shard.entries);
            let _ = self.shard_cache.upsert_sync(
                shard.shard_id,
                (
                    Arc::new(shard.clone()),
                    Arc::new(nar_map),
                    Instant::now() + Duration::from_secs(3600),
                ),
            );
        }

        root.bloom_filter = BloomFilterManifest::new(
            bloom_filter.num_entries(),
            bloom_filter.num_bits(),
            bloom_filter.num_hashes(),
            "sha256:mock_bloom",
            100,
        );
        root.recalculate_merkle_root();

        let baseline = Arc::new(CachedBaseline::new(
            root,
            Arc::new(bloom_filter),
            "sha256:mock_manifest".to_string(),
        ));
        let _ = self.baseline_cache.upsert_sync(
            self.config.baseline_tag.clone(),
            (baseline.clone(), Instant::now() + Duration::from_secs(3600)),
        );
        let _ = self.baseline_cache.upsert_sync(
            cache_key,
            (baseline, Instant::now() + Duration::from_secs(3600)),
        );
        self.set_remote_status(true, None);
    }

    #[cfg(test)]
    pub async fn update_session_in_memory(&self, tag: &str, delta: DeltaPatchData) {
        let _ = self.session_cache.upsert_sync(
            tag.to_string(),
            (
                Arc::new(CachedSession::new(delta)),
                Instant::now() + Duration::from_secs(3600),
            ),
        );
        self.set_remote_status(true, None);
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheIndex, CascadingProxyConfig, DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec};
    use nixcache_core::{
        DeltaPatchData, IndexEntry, NarDigest, NarInfoMeta, ShardDataPayload,
        ShardedArchCacheIndexData, StoreHash, SystemArch, calculate_shard_id,
    };
    use std::{collections::HashMap, time::Duration};

    #[tokio::test]
    async fn test_cascading_lookup_tiers() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CascadingProxyConfig {
            repo: "test/repo".to_string(),
            registry: "ghcr.io".to_string(),
            run_id: Some(123456),
            branch_or_pr: Some("pr-42".to_string()),
            baseline_tag: "cache-index".to_string(),
            upstream_caches: vec![],
            session_ttl: Duration::from_secs(60),
            baseline_ttl: Duration::from_secs(60),
            index_dir: temp_dir.path().to_path_buf(),
            target_system: SystemArch::X86_64Linux,
        };

        let index = CacheIndex::with_config(config, "");

        let hash_base = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let hash_sess = StoreHash::parse("00000000000000000000000000000002").unwrap();
        let hash_hot = StoreHash::parse("00000000000000000000000000000003").unwrap();

        // 1. 设置 Tier 3 Baseline 分片产物
        let baseline_entry = IndexEntry {
            name: "pkg-baseline".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_base),
                nar_basename: format!("{}-hash-base.nar.xz", hash_base),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 100,
            added: "2026-08-29T00:00:00Z".to_string(),
            origin_job: None,
        };
        let mut base_root =
            ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        base_root.public_key = "base-pubkey:AAA=".to_string();
        let shard_id = calculate_shard_id(&hash_base);
        let mut shard_payload = ShardDataPayload::new(shard_id);
        shard_payload
            .entries
            .insert(hash_base.clone(), baseline_entry);

        index
            .update_sharded_baseline_in_memory(base_root, vec![shard_payload], None)
            .await;

        // 2. 设置 Tier 1 Run Session 产物
        let session_entry = IndexEntry {
            name: "pkg-session".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_sess),
                nar_basename: "hash-sess.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap(),
            nar_size: 200,
            added: "2026-08-29T10:00:00Z".to_string(),
            origin_job: Some("job:vm-test".to_string()),
        };
        let mut sess_data = DeltaPatchData::new(123456, "job:vm-test", SystemArch::X86_64Linux);
        sess_data
            .new_entries
            .insert(hash_sess.clone(), session_entry);
        index
            .update_session_in_memory("run-123456", sess_data)
            .await;

        // 3. 动态注入 Tier 0 Hot Entry
        let hot_entry = IndexEntry {
            name: "pkg-hot".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_hot),
                nar_basename: "hot.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap(),
            nar_size: 300,
            added: "2026-08-29T10:05:00Z".to_string(),
            origin_job: Some("job:matrix-x86".to_string()),
        };
        let mut hot_map = HashMap::new();
        hot_map.insert(hash_hot.clone(), hot_entry);
        index.register_hot_entries(hot_map).await;

        // 4. 验证四级级联查找
        let e_hot = index
            .lookup("00000000000000000000000000000003")
            .await
            .expect("Must find in Tier 0");
        assert_eq!(e_hot.name, "pkg-hot");

        let e_sess = index
            .lookup("00000000000000000000000000000002")
            .await
            .expect("Must find in Tier 1");
        assert_eq!(e_sess.name, "pkg-session");

        let e_base = index
            .lookup("00000000000000000000000000000001")
            .await
            .expect("Must find in Tier 3");
        assert_eq!(e_base.name, "pkg-baseline");

        assert!(
            index
                .lookup("00000000000000000000000000000004")
                .await
                .is_none()
        );

        // 5. 验证 NAR Digest 解析 (O(1))
        assert_eq!(
            index.find_nar_digest("hot.nar.xz").await,
            Some(
                NarDigest::new_sha256(
                    "2222222222222222222222222222222222222222222222222222222222222222"
                )
                .unwrap()
            )
        );
        assert_eq!(
            index.find_nar_digest("hash-sess.nar.xz").await,
            Some(
                NarDigest::new_sha256(
                    "1111111111111111111111111111111111111111111111111111111111111111"
                )
                .unwrap()
            )
        );
        assert_eq!(
            index
                .find_nar_digest(&format!("{}-hash-base.nar.xz", hash_base))
                .await,
            Some(
                NarDigest::new_sha256(
                    "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                )
                .unwrap()
            )
        );

        // 6. 验证 Public Key
        let pubkey = index.get_public_key().await;
        assert_eq!(pubkey, Some("base-pubkey:AAA=".to_string()));

        // 7. 验证条目总数统计
        let counts = index.get_entry_counts().await;
        assert_eq!(counts.tier0_hot_entries, 1);
        assert_eq!(counts.tier1_session_entries, 1);
        assert_eq!(counts.tier3_baseline_entries, 1);
        assert_eq!(counts.total_unique_entries, 3);
    }

    #[tokio::test]
    async fn test_proxy_index_backup_zstd_disk_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CascadingProxyConfig {
            repo: "test/repo".to_string(),
            registry: "127.0.0.1:9".to_string(), // Unreachable port to force fallback to local backup
            run_id: None,
            branch_or_pr: None,
            baseline_tag: "cache-index".to_string(),
            upstream_caches: vec![],
            session_ttl: Duration::from_secs(60),
            baseline_ttl: Duration::from_secs(60),
            index_dir: temp_dir.path().to_path_buf(),
            target_system: SystemArch::X86_64Linux,
        };

        let mut root_data =
            ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        root_data.public_key = "backup-pubkey:CCC=".to_string();

        // Pre-create cache-index-x86_64-linux.json.zst in the index dir
        let backup_file = temp_dir.path().join("cache-index-x86_64-linux.json.zst");
        let compressed = IndexCodec::encode_zstd(&root_data, DEFAULT_ZSTD_COMPRESSION_LEVEL)
            .expect("Compression should succeed");
        tokio::fs::write(&backup_file, &compressed).await.unwrap();

        let index = CacheIndex::with_config(config, "");
        let baseline = index.get_baseline_data().await;

        assert_eq!(baseline.root.public_key, "backup-pubkey:CCC=");
        assert_eq!(
            index.get_public_key().await,
            Some("backup-pubkey:CCC=".to_string())
        );
    }
}
