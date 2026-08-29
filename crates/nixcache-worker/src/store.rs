use crate::oci::OciClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
};
use worker::{Env, js_sys::Date};

pub const RUN_SESSION_VERSION: u32 = 3;
pub const CACHE_INDEX_VERSION: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub narinfo: String,
    pub nar_digest: String,
    pub nar_size: u64,
    pub added: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_job: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct JobSummaryMetadata {
    pub job_id: String,
    pub system: String,
    pub uploaded_blobs: usize,
    pub uploaded_bytes: u64,
    pub timestamp: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunSessionManifest {
    pub version: u32,
    pub run_id: u64,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub ref_name: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    pub entries: HashMap<String, IndexEntry>,
    #[serde(default, deserialize_with = "deserialize_gc_roots")]
    pub gc_roots: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub completed_jobs: Vec<JobSummaryMetadata>,
    #[serde(skip)]
    pub nar_lookup: HashMap<String, String>,
}

impl Default for RunSessionManifest {
    fn default() -> Self {
        Self {
            version: RUN_SESSION_VERSION,
            run_id: 0,
            head_sha: String::new(),
            ref_name: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            public_key: None,
            entries: HashMap::new(),
            gc_roots: HashMap::new(),
            completed_jobs: Vec::new(),
            nar_lookup: HashMap::new(),
        }
    }
}

impl RunSessionManifest {
    pub fn rebuild_lookup_table(&mut self) {
        self.nar_lookup = build_nar_lookup_map(&self.entries);
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CacheIndexData {
    pub version: u32,
    pub repo: String,
    pub registry: String,
    pub image: String,
    pub generated: String,
    #[serde(default)]
    pub public_key: String,
    pub entries: HashMap<String, IndexEntry>,
    #[serde(default, deserialize_with = "deserialize_gc_roots")]
    pub gc_roots: HashMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_promoted_run: Option<u64>,
    #[serde(skip)]
    pub nar_lookup: HashMap<String, String>,
    #[serde(skip)]
    pub manifest_digest: String,
}

impl Default for CacheIndexData {
    fn default() -> Self {
        Self {
            version: CACHE_INDEX_VERSION,
            repo: String::new(),
            registry: String::new(),
            image: String::new(),
            generated: String::new(),
            public_key: String::new(),
            entries: HashMap::new(),
            gc_roots: HashMap::new(),
            last_promoted_run: None,
            nar_lookup: HashMap::new(),
            manifest_digest: String::new(),
        }
    }
}

impl CacheIndexData {
    pub fn rebuild_lookup_table(&mut self) {
        self.nar_lookup = build_nar_lookup_map(&self.entries);
    }
}

pub fn build_nar_lookup_map(entries: &HashMap<String, IndexEntry>) -> HashMap<String, String> {
    let mut map = HashMap::with_capacity(entries.len());
    for entry in entries.values() {
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
    map
}

pub fn deserialize_gc_roots<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = Value::deserialize(deserializer)?;
    match val {
        Value::Object(map) => {
            let mut result = HashMap::new();
            for (k, v) in map {
                if let Value::Array(arr) = v {
                    let strings: Vec<String> = arr
                        .into_iter()
                        .filter_map(|item| match item {
                            Value::String(s) => Some(s),
                            _ => None,
                        })
                        .collect();
                    result.insert(k, strings);
                }
            }
            Ok(result)
        }
        Value::Array(arr) => {
            let strings: Vec<String> = arr
                .into_iter()
                .filter_map(|item| match item {
                    Value::String(s) => Some(s),
                    _ => None,
                })
                .collect();
            let mut result = HashMap::new();
            if !strings.is_empty() {
                result.insert("default".to_string(), strings);
            }
            Ok(result)
        }
        _ => Ok(HashMap::new()),
    }
}

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

static HOT_ENTRIES: LazyLock<Mutex<HashMap<String, IndexEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static HOT_NAR_LOOKUP: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MEM_SESSION_CACHE: LazyLock<Mutex<HashMap<String, (RunSessionManifest, f64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MEM_BASELINE_CACHE: Mutex<Option<(CacheIndexData, f64)>> = Mutex::new(None);
static LAST_GHCR_CHECK: Mutex<f64> = Mutex::new(0.0);

const L1_MEM_TTL_MS: f64 = 10_000.0;
const DEBOUNCE_THRESHOLD_MS: f64 = 500.0;

pub struct CacheStore {
    oci_client: OciClient,
    config: WorkerProxyConfig,
    session_ttl_ms: f64,
    baseline_ttl_ms: f64,
}

impl CacheStore {
    pub fn new(oci_client: OciClient, config: WorkerProxyConfig) -> Self {
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

    pub fn oci_client(&self) -> &OciClient {
        &self.oci_client
    }

    /// 动态注册新编译完成的条目到 Tier 0 内存热表中 (0ms 延迟可用)
    pub fn register_hot_entries(entries: HashMap<String, IndexEntry>) {
        if entries.is_empty() {
            return;
        }
        let nar_map = build_nar_lookup_map(&entries);
        if let Ok(mut hot) = HOT_ENTRIES.lock() {
            hot.extend(entries);
        }
        if let Ok(mut lookup) = HOT_NAR_LOOKUP.lock() {
            lookup.extend(nar_map);
        }
    }

    /// 级联查询 Store Hash 对应的 narinfo (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3 -> 智能防抖穿透)
    pub async fn lookup_narinfo_cascading(
        &self,
        env: &Env,
        store_hash: &str,
    ) -> Result<Option<String>, String> {
        // 1. Tier 0: In-Memory Hot Registry
        if let Ok(hot) = HOT_ENTRIES.lock()
            && let Some(entry) = hot.get(store_hash)
        {
            return Ok(Some(entry.narinfo.clone()));
        }

        // 2. Tier 1: Workflow Run Session (run-<run_id>)
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Ok(Some(session)) = self.get_session_data(env, &tag).await
                && let Some(entry) = session.entries.get(store_hash)
            {
                return Ok(Some(entry.narinfo.clone()));
            }
        }

        // 3. Tier 2: Branch / PR Session
        if let Some(ref br) = self.config.branch_or_pr {
            let tag = if br.starts_with("pr-") || br.starts_with("branch-") {
                br.to_string()
            } else {
                format!("branch-{}", br.replace(['/', ':'], "-"))
            };
            if let Ok(Some(branch_sess)) = self.get_session_data(env, &tag).await
                && let Some(entry) = branch_sess.entries.get(store_hash)
            {
                return Ok(Some(entry.narinfo.clone()));
            }
        }

        // 4. Tier 3: Baseline Global Index
        if let Ok(baseline) = self.get_baseline_data(env).await
            && let Some(entry) = baseline.entries.get(store_hash)
        {
            return Ok(Some(entry.narinfo.clone()));
        }

        // 5. Miss: Debounced Read-Through to GHCR
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

        if should_check_ghcr && self.force_refresh(env).await.is_ok() {
            if let Some(run_id) = self.config.run_id {
                let tag = format!("run-{}", run_id);
                if let Ok(Some(session)) = self.get_session_data(env, &tag).await
                    && let Some(entry) = session.entries.get(store_hash)
                {
                    return Ok(Some(entry.narinfo.clone()));
                }
            }
            if let Ok(baseline) = self.get_baseline_data(env).await
                && let Some(entry) = baseline.entries.get(store_hash)
            {
                return Ok(Some(entry.narinfo.clone()));
            }
        }

        Ok(None)
    }

    /// 级联反向解析 NAR 文件名对应的 Blob Digest (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3 -> 智能防抖穿透)
    pub async fn lookup_nar_digest_cascading(
        &self,
        env: &Env,
        nar_basename: &str,
    ) -> Result<Option<String>, String> {
        // 1. Tier 0: In-Memory Hot Registry
        if let Ok(hot_lookup) = HOT_NAR_LOOKUP.lock()
            && let Some(digest) = hot_lookup.get(nar_basename)
        {
            return Ok(Some(digest.clone()));
        }

        // 2. Tier 1: Workflow Run Session (run-<run_id>)
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Ok(Some(session)) = self.get_session_data(env, &tag).await
                && let Some(digest) = session.nar_lookup.get(nar_basename)
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
            if let Ok(Some(branch_sess)) = self.get_session_data(env, &tag).await
                && let Some(digest) = branch_sess.nar_lookup.get(nar_basename)
            {
                return Ok(Some(digest.clone()));
            }
        }

        // 4. Tier 3: Baseline Global Index
        if let Ok(baseline) = self.get_baseline_data(env).await
            && let Some(digest) = baseline.nar_lookup.get(nar_basename)
        {
            return Ok(Some(digest.clone()));
        }

        // 5. Miss: Debounced Read-Through to GHCR
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

        if should_check_ghcr && self.force_refresh(env).await.is_ok() {
            if let Some(run_id) = self.config.run_id {
                let tag = format!("run-{}", run_id);
                if let Ok(Some(session)) = self.get_session_data(env, &tag).await
                    && let Some(digest) = session.nar_lookup.get(nar_basename)
                {
                    return Ok(Some(digest.clone()));
                }
            }
            if let Ok(baseline) = self.get_baseline_data(env).await
                && let Some(digest) = baseline.nar_lookup.get(nar_basename)
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
            if let Ok(Some(session)) = self.get_session_data(env, &tag).await
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
            if let Ok(Some(branch_sess)) = self.get_session_data(env, &tag).await
                && let Some(ref pk) = branch_sess.public_key
                && !pk.is_empty()
            {
                return Ok(Some(pk.clone()));
            }
        }

        // Tier 3
        let baseline = self.get_baseline_data(env).await?;
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
    ) -> Result<Option<RunSessionManifest>, String> {
        let now = Date::now();

        // 1. L1 Memory Cache
        {
            if let Ok(cache) = MEM_SESSION_CACHE.lock()
                && let Some((session, expiry)) = cache.get(tag)
                && now < *expiry
            {
                return Ok(Some(session.clone()));
            }
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
            let mut session = wrapper.data;
            session.rebuild_lookup_table();

            if let Ok(mut cache) = MEM_SESSION_CACHE.lock() {
                cache.insert(tag.to_string(), (session.clone(), now + L1_MEM_TTL_MS));
            }
            return Ok(Some(session));
        }

        // 3. L3 OCI GHCR
        match self.oci_client.get_session_manifest(tag).await {
            Ok(Some((mut session, manifest_digest))) => {
                session.rebuild_lookup_table();

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

                if let Ok(mut cache) = MEM_SESSION_CACHE.lock() {
                    cache.insert(tag.to_string(), (session.clone(), now + L1_MEM_TTL_MS));
                }

                Ok(Some(session))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                if let Ok(kv) = env.kv("NIXCACHE_KV")
                    && let Ok(Some(wrapper)) = kv
                        .get(&kv_key)
                        .json::<KVCacheWrapper<RunSessionManifest>>()
                        .await
                {
                    let mut session = wrapper.data;
                    session.rebuild_lookup_table();
                    return Ok(Some(session));
                }
                Err(e)
            }
        }
    }

    /// 获取生产基线全局索引 (L1 Memory -> L2 KV -> L3 GHCR)
    pub async fn get_baseline_data(&self, env: &Env) -> Result<CacheIndexData, String> {
        let now = Date::now();

        // 1. L1 Memory Cache
        {
            if let Ok(cache) = MEM_BASELINE_CACHE.lock()
                && let Some((ref data, expiry)) = *cache
                && now < expiry
            {
                return Ok(data.clone());
            }
        }

        // 2. L2 Cloudflare KV
        let kv = env.kv("NIXCACHE_KV").map_err(|e| e.to_string())?;
        if let Ok(Some(wrapper)) = kv
            .get("cache_index_wrapper")
            .json::<KVCacheWrapper<CacheIndexData>>()
            .await
            && now - wrapper.last_refresh < self.baseline_ttl_ms
        {
            let mut data = wrapper.data;
            data.manifest_digest = wrapper.manifest_digest;
            data.rebuild_lookup_table();

            if let Ok(mut cache) = MEM_BASELINE_CACHE.lock() {
                *cache = Some((data.clone(), now + L1_MEM_TTL_MS));
            }
            return Ok(data);
        }

        // 3. L3 OCI GHCR
        match self.refresh_baseline_from_ghcr(env).await {
            Ok(data) => Ok(data),
            Err(e) => {
                if let Ok(Some(wrapper)) = kv
                    .get("cache_index_wrapper")
                    .json::<KVCacheWrapper<CacheIndexData>>()
                    .await
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

    async fn refresh_baseline_from_ghcr(&self, env: &Env) -> Result<CacheIndexData, String> {
        let now = Date::now();
        let mut index_data = match self
            .oci_client
            .get_cache_index(&self.config.baseline_tag)
            .await?
        {
            Some((data, digest)) => {
                let mut d = data;
                d.manifest_digest = digest;
                d
            }
            None => CacheIndexData::default(),
        };
        index_data.rebuild_lookup_table();

        let kv = env.kv("NIXCACHE_KV").map_err(|e| e.to_string())?;
        let wrapper = KVCacheWrapper {
            data: index_data.clone(),
            last_refresh: now,
            manifest_digest: index_data.manifest_digest.clone(),
        };
        let _ = kv
            .put("cache_index_wrapper", &wrapper)
            .map_err(|e| e.to_string())?
            .execute()
            .await;

        if let Ok(mut cache) = MEM_BASELINE_CACHE.lock() {
            *cache = Some((index_data.clone(), now + L1_MEM_TTL_MS));
        }

        if let Ok(mut last_check) = LAST_GHCR_CHECK.lock() {
            *last_check = now;
        }

        Ok(index_data)
    }

    /// 强制刷新所有层级的索引 (Tier 1 -> Tier 2 -> Tier 3)
    pub async fn force_refresh(&self, env: &Env) -> Result<usize, String> {
        let mut errors = Vec::new();

        if let Ok(mut mem_sess) = MEM_SESSION_CACHE.lock() {
            mem_sess.clear();
        }
        if let Ok(mut mem_base) = MEM_BASELINE_CACHE.lock() {
            *mem_base = None;
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

        let baseline = match self.refresh_baseline_from_ghcr(env).await {
            Ok(b) => b,
            Err(e) => {
                errors.push(format!("Baseline: {}", e));
                CacheIndexData::default()
            }
        };

        let status = self.get_status(env).await;
        if errors.is_empty()
            || status.total_unique_entries > 0
            || !baseline.manifest_digest.is_empty()
        {
            Ok(status.total_unique_entries)
        } else {
            Err(errors.join("; "))
        }
    }

    /// 获取完整的状态元信息与各层级统计
    pub async fn get_status(&self, env: &Env) -> RemoteStatus {
        let hot_count = HOT_ENTRIES.lock().map(|h| h.len()).unwrap_or(0);

        let mut tier1_count = 0;
        let mut session_opt = None;
        if let Some(run_id) = self.config.run_id {
            let tag = format!("run-{}", run_id);
            if let Ok(Some(sess)) = self.get_session_data(env, &tag).await {
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
            if let Ok(Some(b_sess)) = self.get_session_data(env, &tag).await {
                tier2_count = b_sess.entries.len();
                branch_opt = Some(b_sess);
            }
        }

        let baseline_res = self.get_baseline_data(env).await;
        let (remote_connected, remote_error, tier3_count, manifest_digest, generated) =
            match baseline_res {
                Ok(ref b) => (
                    true,
                    None,
                    b.entries.len(),
                    b.manifest_digest.clone(),
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

        let mut unique_hashes = HashSet::new();
        if let Ok(hot) = HOT_ENTRIES.lock() {
            unique_hashes.extend(hot.keys().cloned());
        }
        if let Some(s) = session_opt {
            unique_hashes.extend(s.entries.keys().cloned());
        }
        if let Some(b) = branch_opt {
            unique_hashes.extend(b.entries.keys().cloned());
        }
        if let Ok(ref b) = baseline_res {
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
    use super::{
        CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, JobSummaryMetadata, RUN_SESSION_VERSION,
        RemoteStatus, RunSessionManifest, build_nar_lookup_map,
    };
    use std::collections::HashMap;

    #[test]
    fn test_build_nar_lookup_map() {
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
                origin_job: Some("job:build-x86".to_string()),
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
                origin_job: None,
            },
        );

        let lookup = build_nar_lookup_map(&entries);
        assert_eq!(
            lookup.get("pkg1.nar.xz"),
            Some(&"sha256:digest1".to_string())
        );
        assert_eq!(lookup.get("pkg2.nar"), Some(&"sha256:digest2".to_string()));
        assert_eq!(lookup.get("nonexistent.nar"), None);
    }

    #[test]
    fn test_schema_v3_data_structures_serialization() {
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
                origin_job: Some("job:nixos-vm-tests".to_string()),
            },
        );

        let data = CacheIndexData {
            version: CACHE_INDEX_VERSION,
            repo: "owner/repo".to_string(),
            registry: "ghcr.io".to_string(),
            image: "ghcr.io/owner/repo/nix-cache".to_string(),
            generated: "2026-08-28T00:00:00Z".to_string(),
            public_key: "key:pub".to_string(),
            entries: entries.clone(),
            gc_roots: HashMap::new(),
            last_promoted_run: Some(123456789),
            nar_lookup: HashMap::new(),
            manifest_digest: "sha256:manifest".to_string(),
        };

        let json = serde_json::to_string(&data).unwrap();
        let parsed: CacheIndexData = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.repo, "owner/repo");
        assert_eq!(parsed.last_promoted_run, Some(123456789));
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(
            parsed.entries.get("hash1").unwrap().origin_job,
            Some("job:nixos-vm-tests".to_string())
        );

        let session = RunSessionManifest {
            version: RUN_SESSION_VERSION,
            run_id: 123456789,
            head_sha: "headsha123".to_string(),
            ref_name: "refs/heads/main".to_string(),
            created_at: "2026-08-29T10:00:00Z".to_string(),
            updated_at: "2026-08-29T10:05:00Z".to_string(),
            public_key: Some("sess-key:pub".to_string()),
            entries,
            gc_roots: HashMap::new(),
            completed_jobs: vec![JobSummaryMetadata {
                job_id: "job:nixos-vm-tests".to_string(),
                system: "x86_64-linux".to_string(),
                uploaded_blobs: 1,
                uploaded_bytes: 1024,
                timestamp: "2026-08-29T10:05:00Z".to_string(),
            }],
            nar_lookup: HashMap::new(),
        };

        let sess_json = serde_json::to_string(&session).unwrap();
        let parsed_sess: RunSessionManifest = serde_json::from_str(&sess_json).unwrap();
        assert_eq!(parsed_sess.version, 3);
        assert_eq!(parsed_sess.run_id, 123456789);
        assert_eq!(parsed_sess.completed_jobs.len(), 1);
        assert_eq!(parsed_sess.completed_jobs[0].job_id, "job:nixos-vm-tests");
    }

    #[test]
    fn test_remote_status_serialization() {
        let status = RemoteStatus {
            remote_connected: true,
            remote_error: None,
            registry: "ghcr.io".to_string(),
            repo: "owner/repo".to_string(),
            run_id: Some(123456),
            branch_or_pr: Some("main".to_string()),
            tier0_hot_entries: 1,
            tier1_session_entries: 2,
            tier2_branch_entries: 0,
            tier3_baseline_entries: 5,
            total_unique_entries: 8,
            index_entries: 8,
            index_ttl: 300,
            session_ttl: 10,
            baseline_ttl: 300,
            upstream: vec!["https://cache.nixos.org".to_string()],
            manifest_digest: "sha256:digest".to_string(),
            generated: "2026-08-28T00:00:00Z".to_string(),
        };

        let json = serde_json::to_string(&status).unwrap();
        let parsed: RemoteStatus = serde_json::from_str(&json).unwrap();
        assert!(parsed.remote_connected);
        assert_eq!(parsed.run_id, Some(123456));
        assert_eq!(parsed.tier0_hot_entries, 1);
        assert_eq!(parsed.total_unique_entries, 8);
    }

    #[test]
    fn test_gc_roots_compatibility() {
        let v2_json = r#"{
            "version": 3,
            "repo": "test/repo",
            "registry": "ghcr.io",
            "image": "ghcr.io/test/repo/nix-cache",
            "generated": "2026-08-28T00:00:00Z",
            "public_key": "",
            "entries": {},
            "gc_roots": {
                "x86_64-linux": ["hash1", "hash2"],
                "aarch64-linux": ["hash3"]
            }
        }"#;
        let v2: CacheIndexData = serde_json::from_str(v2_json).unwrap();
        assert_eq!(v2.gc_roots.get("x86_64-linux").unwrap().len(), 2);
        assert_eq!(v2.gc_roots.get("aarch64-linux").unwrap().len(), 1);

        let v1_json = r#"{
            "version": 1,
            "repo": "test/repo",
            "registry": "ghcr.io",
            "image": "ghcr.io/test/repo/nix-cache",
            "generated": "2026-08-28T00:00:00Z",
            "public_key": "",
            "entries": {},
            "gc_roots": ["hash1", "hash2"]
        }"#;
        let v1: CacheIndexData = serde_json::from_str(v1_json).unwrap();
        assert_eq!(v1.gc_roots.get("default").unwrap().len(), 2);
    }

    #[test]
    fn test_hot_registration_and_lookup() {
        let mut entries = HashMap::new();
        entries.insert(
            "hot-hash-1".to_string(),
            IndexEntry {
                name: "hot-pkg-1".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/hot-hash-1-hot-pkg-1\nURL: nar/hot-pkg-1.nar.xz\n"
                    .to_string(),
                nar_digest: "sha256:hotdigest1".to_string(),
                nar_size: 42,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: Some("job:build-hot".to_string()),
            },
        );

        super::CacheStore::register_hot_entries(entries);

        let hot_entries = super::HOT_ENTRIES.lock().unwrap();
        assert!(hot_entries.contains_key("hot-hash-1"));
        let hot_lookup = super::HOT_NAR_LOOKUP.lock().unwrap();
        assert_eq!(
            hot_lookup.get("hot-pkg-1.nar.xz"),
            Some(&"sha256:hotdigest1".to_string())
        );
    }

    #[test]
    fn test_worker_proxy_config_default() {
        let config = super::WorkerProxyConfig::default();
        assert_eq!(config.registry, "ghcr.io");
        assert_eq!(config.baseline_tag, "cache-index");
        assert_eq!(config.session_ttl_secs, 10);
        assert_eq!(config.baseline_ttl_secs, 300);
        assert_eq!(config.upstream_caches, vec!["https://cache.nixos.org"]);
    }
}
