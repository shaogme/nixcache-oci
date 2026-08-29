use crate::{
    state::{DEBOUNCE_THRESHOLD_MS, L1_MEM_TTL_MS, WorkerState},
    transport::WorkerFetchTransport,
};
use nixcache_core::{
    CacheIndexData, IndexEntry, NarDigest, RunSessionManifest, StoreHash, build_nar_lookup_map,
    extract_nar_basename,
};
use nixcache_oci::OciClient;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use worker::{Env, js_sys::Date};

pub type WorkerOciClient = OciClient<WorkerFetchTransport>;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KVCacheWrapper<T> {
    pub data: T,
    pub last_refresh: f64,
    pub manifest_digest: String,
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
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteStatus {
    pub remote_connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_error: Option<String>,
    pub registry: String,
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
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
        if let Ok(mut state) = WorkerState::global().lock() {
            state.register_hot(entries);
        }
    }

    /// 级联查询 Store Hash 对应的 narinfo (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3 -> 智能防抖穿透)
    pub async fn lookup_narinfo_cascading(
        &self,
        env: &Env,
        store_hash: &str,
    ) -> Result<Option<String>, String> {
        let parsed_hash = match StoreHash::parse(store_hash) {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };

        // 1. Tier 0: In-Memory Hot Registry
        if let Ok(state) = WorkerState::global().lock()
            && let Some(entry) = state.hot_entries.get(&parsed_hash)
        {
            return Ok(Some(entry.to_narinfo_string()));
        }

        // 2. Tier 1: Workflow Run Session (run-<run_id>)
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Ok(Some((session, _))) = self.get_session_data(env, &tag).await
                && let Some(entry) = session.entries.get(&parsed_hash)
            {
                return Ok(Some(entry.to_narinfo_string()));
            }
        }

        // 3. Tier 2: Branch / PR Session
        if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            if let Ok(Some((branch_sess, _))) = self.get_session_data(env, &tag).await
                && let Some(entry) = branch_sess.entries.get(&parsed_hash)
            {
                return Ok(Some(entry.to_narinfo_string()));
            }
        }

        // 4. Tier 3: Baseline Global Index
        if let Ok((baseline, _)) = self.get_baseline_data(env).await
            && let Some(entry) = baseline.entries.get(&parsed_hash)
        {
            return Ok(Some(entry.to_narinfo_string()));
        }

        // 5. Miss: Debounced Read-Through to GHCR
        let now = Date::now();
        let should_check_ghcr = {
            if let Ok(mut state) = WorkerState::global().lock() {
                if now - state.last_ghcr_check > DEBOUNCE_THRESHOLD_MS {
                    state.last_ghcr_check = now;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if should_check_ghcr && self.force_refresh(env).await.is_ok() {
            if let Some(run_id) = self.config.run_id {
                let tag = format!("run-{}", run_id);
                if let Ok(Some((session, _))) = self.get_session_data(env, &tag).await
                    && let Some(entry) = session.entries.get(&parsed_hash)
                {
                    return Ok(Some(entry.to_narinfo_string()));
                }
            }
            if let Ok((baseline, _)) = self.get_baseline_data(env).await
                && let Some(entry) = baseline.entries.get(&parsed_hash)
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
    ) -> Result<Option<NarDigest>, String> {
        let normalized = extract_nar_basename(nar_basename);

        // 1. Tier 0: In-Memory Hot Registry
        if let Ok(state) = WorkerState::global().lock()
            && let Some(digest) = state.hot_nar_lookup.get(normalized)
        {
            return Ok(Some(digest.clone()));
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

        // 4. Tier 3: Baseline Global Index
        if let Ok((_, nar_lookup)) = self.get_baseline_data(env).await
            && let Some(digest) = nar_lookup.get(normalized)
        {
            return Ok(Some(digest.clone()));
        }

        // 5. Miss: Debounced Read-Through to GHCR
        let now = Date::now();
        let should_check_ghcr = {
            if let Ok(mut state) = WorkerState::global().lock() {
                if now - state.last_ghcr_check > DEBOUNCE_THRESHOLD_MS {
                    state.last_ghcr_check = now;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if should_check_ghcr && self.force_refresh(env).await.is_ok() {
            if let Some(run_id) = self.config.run_id {
                let tag = format!("run-{}", run_id);
                if let Ok(Some((_, nar_lookup))) = self.get_session_data(env, &tag).await
                    && let Some(digest) = nar_lookup.get(normalized)
                {
                    return Ok(Some(digest.clone()));
                }
            }
            if let Ok((_, nar_lookup)) = self.get_baseline_data(env).await
                && let Some(digest) = nar_lookup.get(normalized)
            {
                return Ok(Some(digest.clone()));
            }
        }

        Ok(None)
    }

    /// 获取有效的签名公钥 (按 会话 -> 分支 -> 基线 优先级查找)
    pub async fn get_public_key(&self, env: &Env) -> Result<Option<String>, String> {
        // Tier 1
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Ok(Some((session, _))) = self.get_session_data(env, &tag).await
                && let Some(ref pk) = session.public_key
                && !pk.is_empty()
            {
                return Ok(Some(pk.clone()));
            }
        }

        // Tier 2
        if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            if let Ok(Some((branch_sess, _))) = self.get_session_data(env, &tag).await
                && let Some(ref pk) = branch_sess.public_key
                && !pk.is_empty()
            {
                return Ok(Some(pk.clone()));
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

    /// 获取会话清单数据 (L1 Memory -> L2 KV -> L3 GHCR)
    pub async fn get_session_data(
        &self,
        env: &Env,
        tag: &str,
    ) -> Result<Option<(RunSessionManifest, HashMap<String, NarDigest>)>, String> {
        let now = Date::now();

        // 1. L1 Memory Cache
        if let Ok(state) = WorkerState::global().lock()
            && let Some((session, nar_lookup, expiry)) = state.mem_session_cache.get(tag)
            && now < *expiry
        {
            return Ok(Some((session.clone(), nar_lookup.clone())));
        }

        // 2. L2 Cloudflare KV
        let kv_key = format!("session_wrapper_{}", tag);
        if let Ok(kv) = env.kv("NIXCACHE_KV")
            && let Ok(Some(wrapper)) = kv
                .get(&kv_key)
                .json::<KVCacheWrapper<RunSessionManifest>>()
                .await
            && now - wrapper.last_refresh < self.session_ttl_ms
        {
            let session = wrapper.data;
            let nar_lookup = build_nar_lookup_map(&session.entries);

            if let Ok(mut state) = WorkerState::global().lock() {
                state.mem_session_cache.insert(
                    tag.to_string(),
                    (session.clone(), nar_lookup.clone(), now + L1_MEM_TTL_MS),
                );
            }
            return Ok(Some((session, nar_lookup)));
        }

        // 3. L3 OCI GHCR
        match self.oci_client.get_session_manifest(tag).await {
            Ok(Some((session, manifest_digest))) => {
                let nar_lookup = build_nar_lookup_map(&session.entries);

                if let Ok(kv) = env.kv("NIXCACHE_KV") {
                    let wrapper = KVCacheWrapper {
                        data: session.clone(),
                        last_refresh: now,
                        manifest_digest,
                    };
                    let _ = kv
                        .put(&kv_key, &wrapper)
                        .map_err(|e| e.to_string())?
                        .execute()
                        .await;
                }

                if let Ok(mut state) = WorkerState::global().lock() {
                    state.mem_session_cache.insert(
                        tag.to_string(),
                        (session.clone(), nar_lookup.clone(), now + L1_MEM_TTL_MS),
                    );
                }

                Ok(Some((session, nar_lookup)))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                if let Ok(kv) = env.kv("NIXCACHE_KV")
                    && let Ok(Some(wrapper)) = kv
                        .get(&kv_key)
                        .json::<KVCacheWrapper<RunSessionManifest>>()
                        .await
                {
                    let session = wrapper.data;
                    let nar_lookup = build_nar_lookup_map(&session.entries);
                    return Ok(Some((session, nar_lookup)));
                }
                Err(e.to_string())
            }
        }
    }

    /// 获取生产基线全局索引 (L1 Memory -> L2 KV -> L3 GHCR)
    pub async fn get_baseline_data(
        &self,
        env: &Env,
    ) -> Result<(CacheIndexData, HashMap<String, NarDigest>), String> {
        let now = Date::now();

        // 1. L1 Memory Cache
        if let Ok(state) = WorkerState::global().lock()
            && let Some((ref data, ref nar_lookup, expiry)) = state.mem_baseline_cache
            && now < expiry
        {
            return Ok((data.clone(), nar_lookup.clone()));
        }

        // 2. L2 Cloudflare KV
        let kv = env.kv("NIXCACHE_KV").map_err(|e| e.to_string())?;
        if let Ok(Some(wrapper)) = kv
            .get("cache_index_wrapper")
            .json::<KVCacheWrapper<CacheIndexData>>()
            .await
            && now - wrapper.last_refresh < self.baseline_ttl_ms
        {
            let data = wrapper.data;
            let nar_lookup = build_nar_lookup_map(&data.entries);

            if let Ok(mut state) = WorkerState::global().lock() {
                state.mem_baseline_cache =
                    Some((data.clone(), nar_lookup.clone(), now + L1_MEM_TTL_MS));
            }
            return Ok((data, nar_lookup));
        }

        // 3. L3 OCI GHCR
        match self.refresh_baseline_from_ghcr(env).await {
            Ok(res) => Ok(res),
            Err(e) => {
                if let Ok(Some(wrapper)) = kv
                    .get("cache_index_wrapper")
                    .json::<KVCacheWrapper<CacheIndexData>>()
                    .await
                {
                    let data = wrapper.data;
                    let nar_lookup = build_nar_lookup_map(&data.entries);
                    return Ok((data, nar_lookup));
                }
                Err(e)
            }
        }
    }

    async fn refresh_baseline_from_ghcr(
        &self,
        env: &Env,
    ) -> Result<(CacheIndexData, HashMap<String, NarDigest>), String> {
        let now = Date::now();
        let (index_data, manifest_digest) = match self
            .oci_client
            .get_cache_index(&self.config.baseline_tag)
            .await
            .map_err(|e| e.to_string())?
        {
            Some((data, digest)) => (data, digest),
            None => (CacheIndexData::default(), String::new()),
        };

        let nar_lookup = build_nar_lookup_map(&index_data.entries);

        let kv = env.kv("NIXCACHE_KV").map_err(|e| e.to_string())?;
        let wrapper = KVCacheWrapper {
            data: index_data.clone(),
            last_refresh: now,
            manifest_digest,
        };
        let _ = kv
            .put("cache_index_wrapper", &wrapper)
            .map_err(|e| e.to_string())?
            .execute()
            .await;

        if let Ok(mut state) = WorkerState::global().lock() {
            state.mem_baseline_cache =
                Some((index_data.clone(), nar_lookup.clone(), now + L1_MEM_TTL_MS));
            state.last_ghcr_check = now;
        }

        Ok((index_data, nar_lookup))
    }

    /// 强制刷新所有层级的索引 (Tier 1 -> Tier 2 -> Tier 3)
    pub async fn force_refresh(&self, env: &Env) -> Result<usize, String> {
        let mut errors = Vec::new();

        if let Ok(mut state) = WorkerState::global().lock() {
            state.clear_l1_caches();
        }

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
                (CacheIndexData::default(), HashMap::new())
            }
        };

        let status = self.get_status(env).await;
        if errors.is_empty() || status.total_unique_entries > 0 || !baseline.entries.is_empty() {
            Ok(status.total_unique_entries)
        } else {
            Err(errors.join("; "))
        }
    }

    /// 获取完整的状态元信息与各层级统计
    pub async fn get_status(&self, env: &Env) -> RemoteStatus {
        let hot_count = WorkerState::global()
            .lock()
            .map(|s| s.hot_entries.len())
            .unwrap_or(0);

        let mut tier1_count = 0;
        let mut session_opt = None;
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Ok(Some((sess, _))) = self.get_session_data(env, &tag).await {
                tier1_count = sess.entries.len();
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
                tier2_count = b_sess.entries.len();
                branch_opt = Some(b_sess);
            }
        }

        let baseline_res = self.get_baseline_data(env).await;
        let (remote_connected, remote_error, tier3_count, manifest_digest, generated) =
            match baseline_res {
                Ok((ref b, _)) => (
                    true,
                    None,
                    b.entries.len(),
                    String::new(),
                    b.generated.clone(),
                ),
                Err(ref e) => {
                    let kv_data = match env.kv("NIXCACHE_KV") {
                        Ok(kv) => kv
                            .get("cache_index_wrapper")
                            .json::<KVCacheWrapper<CacheIndexData>>()
                            .await
                            .ok()
                            .flatten(),
                        Err(_) => None,
                    };
                    let (count, digest, gen_str) = match kv_data {
                        Some(w) => (w.data.entries.len(), w.manifest_digest, w.data.generated),
                        None => (0, String::new(), String::new()),
                    };
                    (false, Some(e.clone()), count, digest, gen_str)
                }
            };

        let mut unique_hashes: HashSet<StoreHash> = HashSet::new();
        if let Ok(state) = WorkerState::global().lock() {
            unique_hashes.extend(state.hot_entries.keys().cloned());
        }
        if let Some(s) = session_opt {
            unique_hashes.extend(s.entries.keys().cloned());
        }
        if let Some(b) = branch_opt {
            unique_hashes.extend(b.entries.keys().cloned());
        }
        if let Ok((ref b, _)) = baseline_res {
            unique_hashes.extend(b.entries.keys().cloned());
        }

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
            total_unique_entries: unique_hashes.len(),
            index_entries: unique_hashes.len(),
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
        CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, JobSummaryMetadata, NarDigest,
        NarInfoMeta, RUN_SESSION_VERSION, RunSessionManifest, StoreHash, SystemArch,
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
    }

    #[test]
    fn test_schema_v3_data_structures_serialization() {
        let mut session = RunSessionManifest {
            run_id: 12345,
            head_sha: "abc".to_string(),
            ref_name: "refs/heads/main".to_string(),
            ..Default::default()
        };
        session.completed_jobs.push(JobSummaryMetadata {
            job_id: "job1".to_string(),
            system: SystemArch::X86_64Linux,
            uploaded_blobs: 1,
            uploaded_bytes: 1024,
            timestamp: "2026-08-29T10:00:00Z".to_string(),
        });

        assert_eq!(session.version, RUN_SESSION_VERSION);
        let session_json = serde_json::to_string(&session).unwrap();
        let loaded: RunSessionManifest = serde_json::from_str(&session_json).unwrap();
        assert_eq!(loaded.run_id, 12345);

        let index = CacheIndexData::default();
        assert_eq!(index.version, CACHE_INDEX_VERSION);
    }
}
