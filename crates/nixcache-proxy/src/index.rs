use moka::future::Cache;
use nixcache_core::{
    ArchCacheIndexData, ArchRunSessionManifest, CacheIndexData, IndexEntry, NarDigest,
    RunSessionManifest, StoreHash, SystemArch, build_nar_lookup_map, extract_nar_basename,
};
use nixcache_oci::{CacheLayerMediaType, DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec, OciClient};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{fs, sync::RwLock};
use tracing::{error, info, warn};

pub fn detect_current_system() -> SystemArch {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    SystemArch::from_oci(os, arch, None)
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

/// 带有 O(1) 反向 NAR 映射表的会话缓存模型
#[derive(Clone, Debug)]
pub struct CachedSession {
    pub manifest: RunSessionManifest,
    pub nar_lookup: HashMap<String, NarDigest>,
}

impl CachedSession {
    pub fn new(manifest: RunSessionManifest) -> Self {
        let nar_lookup = build_nar_lookup_map(&manifest.entries);
        Self {
            manifest,
            nar_lookup,
        }
    }

    pub fn from_arch_manifest(arch: ArchRunSessionManifest) -> Self {
        let mut gc_roots = HashMap::new();
        if !arch.gc_roots.is_empty() {
            gc_roots.insert(arch.system, arch.gc_roots);
        }
        let manifest = RunSessionManifest {
            version: arch.version,
            run_id: arch.run_id,
            head_sha: arch.head_sha,
            ref_name: arch.ref_name,
            created_at: arch.created_at,
            updated_at: arch.updated_at,
            public_key: arch.public_key,
            entries: arch.entries,
            gc_roots,
            completed_jobs: arch.completed_jobs,
        };
        Self::new(manifest)
    }
}

/// 带有 O(1) 反向 NAR 映射表的基线缓存模型
#[derive(Clone, Debug)]
pub struct CachedBaseline {
    pub data: CacheIndexData,
    pub nar_lookup: HashMap<String, NarDigest>,
}

impl CachedBaseline {
    pub fn new(data: CacheIndexData) -> Self {
        let nar_lookup = build_nar_lookup_map(&data.entries);
        Self { data, nar_lookup }
    }

    pub fn from_arch_data(arch_data: ArchCacheIndexData) -> Self {
        let data = CacheIndexData::from_arch_data(arch_data);
        Self::new(data)
    }
}

#[derive(Clone)]
pub struct CacheIndex {
    config: CascadingProxyConfig,
    oci_client: OciClient,
    // Tier 0: 本地内存热注册表 (In-Memory Hot Registry)
    hot_entries: Cache<StoreHash, IndexEntry>,
    hot_nar_lookup: Cache<String, NarDigest>,
    // Tier 1 & Tier 2: 工作流及分支/PR 会话缓存 (key 为 tag 如 "run-123", "branch-main")
    session_cache: Cache<String, Arc<CachedSession>>,
    // Tier 3: 生产主干基线索引缓存 (key 为 config.baseline_tag)
    baseline_cache: Cache<String, Arc<CachedBaseline>>,
    // 远端连接与错误状态
    remote_status: Arc<RwLock<RemoteStatus>>,
}

impl CacheIndex {
    pub fn with_config(config: CascadingProxyConfig, github_token: &str) -> Self {
        let oci_client = OciClient::new(&config.registry, &config.repo, github_token, false);
        let hot_entries = Cache::builder().build();
        let hot_nar_lookup = Cache::builder().build();
        let session_cache = Cache::builder().time_to_live(config.session_ttl).build();
        let baseline_cache = Cache::builder().time_to_live(config.baseline_ttl).build();

        Self {
            config,
            oci_client,
            hot_entries,
            hot_nar_lookup,
            session_cache,
            baseline_cache,
            remote_status: Arc::new(RwLock::new(RemoteStatus::default())),
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

    pub async fn remote_status(&self) -> (bool, Option<String>) {
        let status = self.remote_status.read().await;
        (status.connected, status.error.clone())
    }

    async fn set_remote_status(&self, connected: bool, error: Option<String>) {
        let mut status = self.remote_status.write().await;
        status.connected = connected;
        status.error = error;
    }

    /// Tier 0: 动态注册新编译完成的条目到内存热表和 NAR 查找表中 (0ms 延迟可用)
    pub async fn register_hot_entries(&self, entries: HashMap<StoreHash, IndexEntry>) {
        let count = entries.len();
        let nar_map = build_nar_lookup_map(&entries);

        for (key, entry) in entries {
            self.hot_entries.insert(key, entry).await;
        }
        for (nar_name, digest) in nar_map {
            self.hot_nar_lookup.insert(nar_name, digest).await;
        }

        info!(
            "[nixcache-proxy] Registered {} entries into Tier 0 In-Memory Hot Registry (Total: {})",
            count,
            self.hot_entries.entry_count()
        );
    }

    /// 级联查询 Store Hash 对应的 IndexEntry (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3)
    pub async fn lookup(&self, store_hash: &str) -> Option<IndexEntry> {
        let parsed_hash = StoreHash::parse(store_hash).ok()?;

        // Tier 0: 内存热注册表
        if let Some(entry) = self.hot_entries.get(&parsed_hash).await {
            return Some(entry);
        }

        // Tier 1: 工作流会话 (run-<run_id>)
        if self.config.run_id.is_some()
            && let Some(session) = self.get_session_data().await
            && let Some(entry) = session.manifest.entries.get(&parsed_hash)
        {
            return Some(entry.clone());
        }

        // Tier 2: 分支/PR 会话
        if self.config.branch_or_pr.is_some()
            && let Some(branch) = self.get_branch_data().await
            && let Some(entry) = branch.manifest.entries.get(&parsed_hash)
        {
            return Some(entry.clone());
        }

        // Tier 3: 生产主干基线
        let baseline = self.get_baseline_data().await;
        baseline.data.entries.get(&parsed_hash).cloned()
    }

    /// 级联反向解析 NAR 文件名对应的 Blob Digest (全链路 O(1) 检索)
    pub async fn find_nar_digest(&self, nar_basename: &str) -> Option<NarDigest> {
        let normalized = extract_nar_basename(nar_basename);

        // Tier 0: O(1) 查找
        if let Some(digest) = self.hot_nar_lookup.get(normalized).await {
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

        // Tier 3: O(1) 查找
        let baseline = self.get_baseline_data().await;
        baseline.nar_lookup.get(normalized).cloned()
    }

    /// 获取有效的签名公钥 (按会话 -> 分支 -> 基线优先级查找)
    pub async fn get_public_key(&self) -> Option<String> {
        if self.config.run_id.is_some()
            && let Some(session) = self.get_session_data().await
            && let Some(ref pk) = session.manifest.public_key
            && !pk.is_empty()
        {
            return Some(pk.clone());
        }

        if self.config.branch_or_pr.is_some()
            && let Some(branch) = self.get_branch_data().await
            && let Some(ref pk) = branch.manifest.public_key
            && !pk.is_empty()
        {
            return Some(pk.clone());
        }

        let baseline = self.get_baseline_data().await;
        if !baseline.data.public_key.is_empty() {
            Some(baseline.data.public_key.clone())
        } else {
            None
        }
    }

    /// 获取各层级的条目统计信息
    pub async fn get_entry_counts(&self) -> StatusEntryCounts {
        let hot_count = self.hot_entries.entry_count() as usize;

        let session_count = if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            self.session_cache
                .get(&tag)
                .await
                .map(|s| s.manifest.entries.len())
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
                .get(&tag)
                .await
                .map(|b| b.manifest.entries.len())
                .unwrap_or(0)
        } else {
            0
        };

        let baseline_count = self
            .baseline_cache
            .get(&self.config.baseline_tag)
            .await
            .map(|b| b.data.entries.len())
            .unwrap_or(0);

        let mut all_unique_hashes = HashSet::new();
        for (k, _) in self.hot_entries.iter() {
            all_unique_hashes.insert((*k).clone());
        }

        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Some(sess) = self.session_cache.get(&tag).await {
                all_unique_hashes.extend(sess.manifest.entries.keys().cloned());
            }
        }

        if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            if let Some(branch) = self.session_cache.get(&tag).await {
                all_unique_hashes.extend(branch.manifest.entries.keys().cloned());
            }
        }

        if let Some(baseline) = self.baseline_cache.get(&self.config.baseline_tag).await {
            all_unique_hashes.extend(baseline.data.entries.keys().cloned());
        }

        StatusEntryCounts {
            tier0_hot_entries: hot_count,
            tier1_session_entries: session_count,
            tier2_branch_entries: branch_count,
            tier3_baseline_entries: baseline_count,
            total_unique_entries: all_unique_hashes.len(),
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

    pub async fn get_baseline_data(&self) -> Arc<CachedBaseline> {
        let tag = &self.config.baseline_tag;
        let system = &self.config.target_system;
        let cache_key = format!("{}-{}", tag, system.as_str());

        if let Some(cached) = self.baseline_cache.get(&cache_key).await {
            return cached;
        }

        let tag_str = tag.clone();
        let system_clone = system.clone();
        let res = self
            .baseline_cache
            .try_get_with(cache_key.clone(), async {
                info!(
                    "[nixcache-proxy] Refreshing Tier 3 Baseline Index (Tag: {}, System: {})...",
                    tag_str, system_clone
                );
                let mut refresh_ok = false;
                let mut fetched_baseline = None;

                match self.oci_client.get_arch_cache_index(&tag_str, &system_clone).await {
                    Ok(Some((arch_data, _))) => {
                        refresh_ok = true;
                        self.set_remote_status(true, None).await;

                        // 保存本地单架构备份文件
                        let file_name = format!("cache-index-{}.json.zst", system_clone.as_str());
                        let file_path = self.config.index_dir.join(&file_name);
                        if let Some(parent) = file_path.parent() {
                            let _ = fs::create_dir_all(parent).await;
                        }
                        if let Ok(bytes) =
                            IndexCodec::encode_zstd(&arch_data, DEFAULT_ZSTD_COMPRESSION_LEVEL)
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
                        fetched_baseline = Some(CachedBaseline::from_arch_data(arch_data));
                    }
                    Ok(None) => {
                        info!(
                            "[nixcache-proxy] Baseline tag {} not found on registry for system {}.",
                            tag_str, system_clone
                        );
                        self.set_remote_status(true, None).await;
                    }
                    Err(e) => {
                        error!(
                            "[nixcache-proxy] Failed to fetch baseline cache index: {}",
                            e
                        );
                        self.set_remote_status(
                            false,
                            Some(format!("Failed to connect to remote: {}", e)),
                        )
                        .await;
                    }
                }

                if !refresh_ok {
                    let arch_backup = self
                        .config
                        .index_dir
                        .join(format!("cache-index-{}.json.zst", system_clone.as_str()));

                    if arch_backup.exists() {
                        match fs::read(&arch_backup).await {
                            Ok(bytes) => {
                                if let Ok(arch_data) = IndexCodec::decode_zstd::<ArchCacheIndexData>(
                                    &bytes,
                                    CacheLayerMediaType::INDEX_V3_ZSTD,
                                ) {
                                    info!(
                                        "[nixcache-proxy] Loaded backup arch index from {:?}",
                                        arch_backup
                                    );
                                    fetched_baseline = Some(CachedBaseline::from_arch_data(arch_data));
                                    refresh_ok = true;
                                }
                            }
                            Err(e) => {
                                error!("[nixcache-proxy] Failed to read backup cache index: {}", e);
                            }
                        }
                    }
                }

                if let Some(baseline) = fetched_baseline {
                    info!(
                        "[nixcache-proxy] Baseline index refreshed successfully with {} entries for {}.",
                        baseline.data.entries.len(), system_clone
                    );
                    Ok(Arc::new(baseline))
                } else if refresh_ok {
                    Ok(Arc::new(CachedBaseline::new(CacheIndexData::default())))
                } else {
                    Err(
                        "Failed to refresh baseline index from both remote registry and backup"
                            .to_string(),
                    )
                }
            })
            .await;

        match res {
            Ok(data) => data,
            Err(_) => Arc::new(CachedBaseline::new(CacheIndexData::default())),
        }
    }

    pub async fn get_data(&self) -> Arc<CacheIndexData> {
        let baseline = self.get_baseline_data().await;
        Arc::new(baseline.data.clone())
    }

    /// 强制刷新所有层级的索引
    pub async fn force_refresh(&self) -> Result<usize, String> {
        let mut errs = Vec::new();
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            self.session_cache.invalidate(&tag).await;
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
            self.session_cache.invalidate(&tag).await;
            if self.fetch_or_get_session(&tag).await.is_none() {
                errs.push(format!("Branch: failed to refresh tag {}", tag));
            }
        }

        let cache_key = format!(
            "{}-{}",
            self.config.baseline_tag,
            self.config.target_system.as_str()
        );
        self.baseline_cache.invalidate(&cache_key).await;
        self.baseline_cache
            .invalidate(&self.config.baseline_tag)
            .await;
        let baseline = self.get_baseline_data().await;
        if baseline.data.entries.is_empty() {
            let (_, remote_err) = self.remote_status().await;
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
        if let Some(manifest) = self.session_cache.get(tag).await {
            return Some(manifest);
        }

        let tag_str = tag.to_string();
        let system_clone = self.config.target_system.clone();
        let res = self
            .session_cache
            .try_get_with(tag_str.clone(), async {
                info!(
                    "[nixcache-proxy] Refreshing Session Manifest (Tag: {}, System: {})...",
                    tag_str, system_clone
                );
                match self
                    .oci_client
                    .get_arch_session_manifest(&tag_str, &system_clone)
                    .await
                {
                    Ok(Some((session, _))) => {
                        self.set_remote_status(true, None).await;
                        Ok(Arc::new(CachedSession::from_arch_manifest(session)))
                    }
                    Ok(None) => {
                        info!(
                            "[nixcache-proxy] Session tag {} not found on remote for system {}.",
                            tag_str, system_clone
                        );
                        self.set_remote_status(true, None).await;
                        Err("Session tag not found on remote".to_string())
                    }
                    Err(e) => {
                        warn!(
                            "[nixcache-proxy] Failed to fetch session manifest {}: {}",
                            tag_str, e
                        );
                        self.set_remote_status(false, Some(e.to_string())).await;
                        Err(e.to_string())
                    }
                }
            })
            .await;

        res.ok()
    }

    #[cfg(test)]
    pub async fn update_data_in_memory(&self, new_data: CacheIndexData) {
        let cache_key = format!(
            "{}-{}",
            self.config.baseline_tag,
            self.config.target_system.as_str()
        );
        let baseline = Arc::new(CachedBaseline::new(new_data));
        self.baseline_cache
            .insert(self.config.baseline_tag.clone(), baseline.clone())
            .await;
        self.baseline_cache.insert(cache_key, baseline).await;
        self.set_remote_status(true, None).await;
    }

    #[cfg(test)]
    pub async fn update_session_in_memory(&self, tag: &str, session: RunSessionManifest) {
        self.session_cache
            .insert(tag.to_string(), Arc::new(CachedSession::new(session)))
            .await;
        self.set_remote_status(true, None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheIndex, CascadingProxyConfig, DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec};
    use nixcache_core::{
        ArchCacheIndexData, CacheIndexData, IndexEntry, NarDigest, NarInfoMeta, RunSessionManifest,
        StoreHash, SystemArch,
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

        // 1. 设置 Tier 3 Baseline 产物
        let baseline_entry = IndexEntry {
            name: "pkg-baseline".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_base),
                nar_basename: "hash-base.nar.xz".to_string(),
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
        let mut base_data = CacheIndexData::default();
        base_data.entries.insert(hash_base.clone(), baseline_entry);
        base_data.public_key = "base-pubkey:AAA=".to_string();
        index.update_data_in_memory(base_data).await;

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
        let mut sess_data = RunSessionManifest {
            run_id: 123456,
            public_key: Some("sess-pubkey:BBB=".to_string()),
            ..Default::default()
        };
        sess_data.entries.insert(hash_sess.clone(), session_entry);
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
            index.find_nar_digest("hash-base.nar.xz").await,
            Some(
                NarDigest::new_sha256(
                    "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                )
                .unwrap()
            )
        );

        // 6. 验证 Public Key 优先级 (Session Key 优先于 Baseline Key)
        let pubkey = index.get_public_key().await;
        assert_eq!(pubkey, Some("sess-pubkey:BBB=".to_string()));

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

        let hash_backup = StoreHash::parse("11111111111111111111111111111111").unwrap();
        let backup_entry = IndexEntry {
            name: "pkg-from-backup".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_backup),
                nar_basename: "backup-pkg.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 512,
            added: "2026-08-29T00:00:00Z".to_string(),
            origin_job: None,
        };

        let mut arch_data = ArchCacheIndexData {
            version: 1,
            system: SystemArch::X86_64Linux,
            repo: "test/repo".to_string(),
            registry: "ghcr.io".to_string(),
            generated: "2026-08-29T00:00:00Z".to_string(),
            public_key: "backup-pubkey:CCC=".to_string(),
            entries: HashMap::new(),
            gc_roots: Vec::new(),
            last_promoted_run: None,
        };
        arch_data.entries.insert(hash_backup.clone(), backup_entry);

        // Pre-create cache-index-x86_64-linux.json.zst in the index dir
        let backup_file = temp_dir.path().join("cache-index-x86_64-linux.json.zst");
        let compressed = IndexCodec::encode_zstd(&arch_data, DEFAULT_ZSTD_COMPRESSION_LEVEL)
            .expect("Compression should succeed");
        tokio::fs::write(&backup_file, &compressed).await.unwrap();

        let index = CacheIndex::with_config(config, "");
        index.get_baseline_data().await;

        let entry = index
            .lookup("11111111111111111111111111111111")
            .await
            .expect("Must find entry recovered from zstd backup");
        assert_eq!(entry.name, "pkg-from-backup");
        assert_eq!(
            index.get_public_key().await,
            Some("backup-pubkey:CCC=".to_string())
        );
    }
}
