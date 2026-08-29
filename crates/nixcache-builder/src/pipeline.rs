use crate::{
    error::BuilderError,
    nix::{self, BuildConfig},
};
use chrono::Utc;
use nixcache_core::{
    BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, NarDigest, NarInfo,
    StoreHash, SystemArch, evaluate_multi_arch_gc,
};
use nixcache_oci::{OciClient, build_index_manifest};
use std::{
    collections::{HashMap, HashSet},
    env,
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::{
    fs,
    process::{Child, Command},
    time::sleep,
};
use tracing::{error, info, warn};

fn find_proxy_binary() -> PathBuf {
    if let Ok(current_exe) = env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        let local_proxy = parent.join("nixcache-proxy");
        if local_proxy.exists() {
            return local_proxy;
        }
        let local_proxy_exe = parent.join("nixcache-proxy.exe");
        if local_proxy_exe.exists() {
            return local_proxy_exe;
        }
    }
    PathBuf::from("nixcache-proxy")
}

async fn write_nix_conf(
    substituters: &[&str],
    trusted_keys: &[&str],
) -> Result<Option<(PathBuf, String)>, BuilderError> {
    let nix_conf_path = PathBuf::from("/etc/nix/nix.conf");

    let original_content = if nix_conf_path.exists() {
        fs::read_to_string(&nix_conf_path).await.unwrap_or_default()
    } else {
        String::new()
    };

    let mut new_content = original_content.clone();
    if !new_content.ends_with('\n') && !new_content.is_empty() {
        new_content.push('\n');
    }

    for sub in substituters {
        new_content.push_str(&format!("extra-substituters = {}\n", sub));
        new_content.push_str(&format!("extra-trusted-substituters = {}\n", sub));
    }
    for key in trusted_keys {
        new_content.push_str(&format!("extra-trusted-public-keys = {}\n", key));
    }

    match fs::write(&nix_conf_path, &new_content).await {
        Ok(_) => {
            info!("Added self-substituter to {:?}", nix_conf_path);
            Ok(Some((nix_conf_path, original_content)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if let Ok(home) = env::var("HOME") {
                let user_conf_path = PathBuf::from(home).join(".config/nix/nix.conf");
                if let Some(parent) = user_conf_path.parent() {
                    let _ = fs::create_dir_all(parent).await;
                }
                let user_original = if user_conf_path.exists() {
                    fs::read_to_string(&user_conf_path)
                        .await
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let mut user_new = user_original.clone();
                if !user_new.ends_with('\n') && !user_new.is_empty() {
                    user_new.push('\n');
                }
                for sub in substituters {
                    user_new.push_str(&format!("extra-substituters = {}\n", sub));
                    user_new.push_str(&format!("extra-trusted-substituters = {}\n", sub));
                }
                for key in trusted_keys {
                    user_new.push_str(&format!("extra-trusted-public-keys = {}\n", key));
                }
                fs::write(&user_conf_path, user_new).await?;
                info!("Added self-substituter to {:?}", user_conf_path);
                Ok(Some((user_conf_path, user_original)))
            } else {
                Err(BuilderError::Config(format!(
                    "Permission denied for /etc/nix/nix.conf and HOME env is not set: {}",
                    e
                )))
            }
        }
        Err(e) => Err(BuilderError::Io(e)),
    }
}

async fn restore_nix_conf(backup: Option<(PathBuf, String)>) {
    if let Some((path, content)) = backup {
        if content.is_empty() {
            let _ = fs::remove_file(&path).await;
        } else {
            let _ = fs::write(&path, content).await;
        }
        info!("Restored nix.conf at {:?}", path);
    }
}

async fn get_own_public_key(signing_key_file: Option<&str>) -> Option<String> {
    let key_file = signing_key_file?;
    let pub_file = format!("{}.pub", key_file);
    if Path::new(&pub_file).exists() {
        fs::read_to_string(&pub_file)
            .await
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        let mut output = Command::new("nix")
            .args(["key", "convert-secret-to-public"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?;

        use tokio::io::AsyncWriteExt;
        let secret = fs::read_to_string(key_file).await.ok()?;
        let mut stdin = output.stdin.take()?;
        let _ = stdin.write_all(secret.as_bytes()).await;
        let _ = stdin.flush().await;
        drop(stdin);

        let res = output.wait_with_output().await.ok()?;
        if res.status.success() {
            Some(String::from_utf8_lossy(&res.stdout).trim().to_string())
        } else {
            None
        }
    }
}

struct ProxyGuard {
    child: Option<Child>,
    nix_conf_backup: Option<(PathBuf, String)>,
}

impl ProxyGuard {
    async fn stop(&mut self) {
        restore_nix_conf(self.nix_conf_backup.take()).await;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }
}

impl Drop for ProxyGuard {
    fn drop(&mut self) {
        if let Some((path, content)) = self.nix_conf_backup.take() {
            if content.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                let _ = std::fs::write(&path, content);
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

async fn setup_self_substituter(
    repo: &str,
    registry: &str,
    github_token: &str,
    signing_key_file: Option<&str>,
    fail_fast: bool,
) -> Result<ProxyGuard, BuilderError> {
    let proxy_bin = find_proxy_binary();
    info!("Starting self-substituter proxy using {:?}", proxy_bin);

    let mut proxy_cmd = Command::new(&proxy_bin);
    proxy_cmd
        .env("NIXCACHE_REPO", repo)
        .env("NIXCACHE_REGISTRY", registry)
        .env("NIXCACHE_PORT", "37515")
        .env("NIXCACHE_LISTEN", "127.0.0.1")
        .env("NIXCACHE_UPSTREAM", "")
        .env("GITHUB_TOKEN", github_token)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let (proxy_child, ready) = match proxy_cmd.spawn() {
        Ok(child) => {
            let mut ready = false;
            let client = reqwest::Client::new();
            for _ in 1..=15 {
                if let Ok(res) = client
                    .get("http://127.0.0.1:37515/nix-cache-info")
                    .send()
                    .await
                    && res.status().is_success()
                {
                    ready = true;
                    break;
                }
                sleep(Duration::from_secs(1)).await;
            }
            (Some(child), ready)
        }
        Err(e) => {
            if fail_fast {
                return Err(BuilderError::Proxy(format!(
                    "Failed to spawn nixcache-proxy: {}",
                    e
                )));
            }
            warn!(
                "Could not spawn nixcache-proxy ({}). Proceeding without self-substituter.",
                e
            );
            (None, false)
        }
    };

    let mut nix_conf_backup = None;
    if ready {
        info!("Self-substituter running on port 37515");
        let mut keys = Vec::new();
        let pub_key = get_own_public_key(signing_key_file).await;
        if let Some(ref k) = pub_key {
            keys.push(k.as_str());
            info!("Trusted own public key: {}", k);
        }
        nix_conf_backup = write_nix_conf(&["http://127.0.0.1:37515"], &keys).await?;
    } else if proxy_child.is_some() {
        if fail_fast {
            if let Some(mut child) = proxy_child {
                let _ = child.kill().await;
            }
            return Err(BuilderError::Proxy(
                "Self-substituter proxy failed to become ready (timed out after 15s)".to_string(),
            ));
        }
        info!("Self-substituter failed to respond, proceeding without it");
    }

    Ok(ProxyGuard {
        child: proxy_child,
        nix_conf_backup,
    })
}

async fn fetch_remote_cache_index(oci: &OciClient) -> (CacheIndexData, HashSet<StoreHash>) {
    let mut remote_index = CacheIndexData::default();
    let mut own_hashes = HashSet::new();

    if let Ok(Some((data, _))) = oci.get_cache_index("cache-index").await {
        own_hashes = data.entries.keys().cloned().collect();
        remote_index = data;
    }

    (remote_index, own_hashes)
}

async fn push_cache_index_data(
    oci: &OciClient,
    index: &CacheIndexData,
    tag: &str,
) -> Result<(), BuilderError> {
    let (index_digest, index_size) = oci.push_json_blob(index).await?;

    let empty_config = "{}";
    let temp_cfg = NamedTempFile::new()?;
    tokio::fs::write(temp_cfg.path(), empty_config.as_bytes()).await?;
    let config_digest = oci.push_blob(temp_cfg.path()).await?;
    let config_size = 2u64;

    let manifest = build_index_manifest(&index_digest, index_size, &config_digest, config_size);
    let manifest_str = manifest.to_json_string()?;

    oci.push_manifest(tag, &manifest_str).await?;
    Ok(())
}

async fn record_store_snapshot(snap_path: &Path) -> Result<(), BuilderError> {
    let mut paths = Vec::new();
    if let Ok(mut entries) = fs::read_dir("/nix/store").await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                paths.push(name.to_string());
            }
        }
    }
    paths.sort();
    if let Some(parent) = snap_path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let content = paths.join("\n");
    fs::write(snap_path, content).await?;
    info!(
        "Recorded store snapshot with {} paths to {:?}",
        paths.len(),
        snap_path
    );
    Ok(())
}

/// Session Init: 启动后台 Proxy 守护进程，配置 nix substituters，并记录 store 快照
#[allow(clippy::too_many_arguments)]
pub async fn run_session_init(
    repo: &str,
    registry: &str,
    run_id: Option<u64>,
    branch: Option<String>,
    port: u16,
    listen: &str,
    upstream: &str,
    session_ttl: u64,
    baseline_ttl: u64,
    baseline_tag: &str,
    github_token: &str,
    signing_key_file: Option<&str>,
    snapshot_path: Option<&Path>,
) -> Result<(), BuilderError> {
    info!(
        "Initializing NixCache Session: Run ID: {:?}, Branch: {:?}, Repo: {}/{}",
        run_id, branch, registry, repo
    );

    let proxy_bin = find_proxy_binary();
    info!("Starting background proxy daemon using {:?}", proxy_bin);

    let mut proxy_cmd = Command::new(&proxy_bin);
    proxy_cmd
        .env("NIXCACHE_REPO", repo)
        .env("NIXCACHE_REGISTRY", registry)
        .env("NIXCACHE_PORT", port.to_string())
        .env("NIXCACHE_LISTEN", listen)
        .env("NIXCACHE_UPSTREAM", upstream)
        .env("NIXCACHE_SESSION_TTL", session_ttl.to_string())
        .env("NIXCACHE_INDEX_TTL", baseline_ttl.to_string())
        .env("NIXCACHE_BASELINE_TAG", baseline_tag)
        .env("GITHUB_TOKEN", github_token)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    if let Some(rid) = run_id {
        proxy_cmd.env("NIXCACHE_RUN_ID", rid.to_string());
    }
    if let Some(ref br) = branch {
        proxy_cmd.env("NIXCACHE_BRANCH", br);
    }

    let _child = proxy_cmd.spawn()?;

    let client = reqwest::Client::new();
    let probe_url = format!("http://{}:{}/nix-cache-info", listen, port);
    let mut ready = false;
    for _ in 1..=20 {
        if let Ok(res) = client.get(&probe_url).send().await
            && res.status().is_success()
        {
            ready = true;
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }

    if !ready {
        return Err(BuilderError::Proxy(format!(
            "Proxy failed to become ready on http://{}:{}",
            listen, port
        )));
    }
    info!("Proxy is running and ready on http://{}:{}", listen, port);

    let proxy_substituter = format!("http://{}:{}", listen, port);
    let mut keys = Vec::new();
    let pub_key = get_own_public_key(signing_key_file).await;
    if let Some(ref k) = pub_key {
        keys.push(k.as_str());
        info!("Trusted own public key: {}", k);
    }
    let _ = write_nix_conf(&[&proxy_substituter], &keys).await?;

    if let Some(snap) = snapshot_path {
        record_store_snapshot(snap).await?;
    }

    write_session_init_summary(repo, run_id, branch.as_deref(), port).await;
    Ok(())
}

/// Session Capture: 捕获本 Job 新构建产物，导出并上传 NAR Blobs，CAS 更新 run-<run_id>，热注册到 Proxy
#[allow(clippy::too_many_arguments)]
pub async fn run_session_capture(
    repo: &str,
    registry: &str,
    run_id: u64,
    job_id: &str,
    system_opt: Option<&str>,
    signing_key_file: Option<&str>,
    github_token: &str,
    output_receipt_path: Option<&Path>,
    proxy_url: Option<&str>,
    snapshot_before: Option<&Path>,
    explicit_paths: &[String],
) -> Result<(), BuilderError> {
    let system = match system_opt {
        Some(s) if !s.trim().is_empty() => SystemArch::from(s.trim()),
        _ => SystemArch::from(nix::get_system().await?.as_str()),
    };

    info!(
        "Capturing session for Job: {} | Run ID: {} | System: {} | Repo: {}/{}",
        job_id, run_id, system, registry, repo
    );

    let candidate_paths: Vec<String> = if !explicit_paths.is_empty() {
        explicit_paths.to_vec()
    } else if let Some(snap_file) = snapshot_before
        && snap_file.exists()
    {
        let before_content = fs::read_to_string(snap_file).await.unwrap_or_default();
        let before_set: HashSet<&str> = before_content
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();

        let mut diff_paths = Vec::new();
        if let Ok(mut entries) = fs::read_dir("/nix/store").await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str()
                    && !before_set.contains(name)
                    && !name.ends_with(".lock")
                    && !name.ends_with(".drv")
                {
                    diff_paths.push(format!("/nix/store/{}", name));
                }
            }
        }
        diff_paths
    } else {
        Vec::new()
    };

    info!(
        "Found {} candidate path(s) to capture",
        candidate_paths.len()
    );

    let oci = OciClient::new(registry, repo, github_token, true);
    let temp_dir = tempfile::tempdir()?;

    let mut new_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    let mut uploaded_count = 0;
    let mut total_bytes_uploaded = 0u64;

    if !candidate_paths.is_empty() {
        let exported =
            nix::export_paths_directly(&candidate_paths, signing_key_file, temp_dir.path()).await?;
        for (hash, store_path) in exported {
            let narinfo_path = temp_dir.path().join(format!("{}.narinfo", hash));
            let nar_file_path = temp_dir.path().join("nar").join(format!("{}.nar.xz", hash));

            if !nar_file_path.exists() {
                continue;
            }

            let metadata = fs::metadata(&nar_file_path).await?;
            let size = metadata.len();

            info!("  Uploading NAR for {} ({} bytes)", hash, size);
            match oci.push_blob(&nar_file_path).await {
                Ok(nar_digest_str) => {
                    if let Ok(narinfo_content) = fs::read_to_string(&narinfo_path).await {
                        let name = Path::new(&store_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .and_then(|s| s.split_once('-'))
                            .map(|x| x.1.to_string())
                            .unwrap_or(hash.clone());

                        if let Ok(narinfo) = NarInfo::parse(&narinfo_content) {
                            let (narinfo_meta, nar_size) = narinfo.into_meta();
                            let store_hash = narinfo_meta
                                .store_hash()
                                .unwrap_or_else(|| StoreHash::new_unchecked(&hash));
                            let nar_digest = NarDigest::parse(&nar_digest_str)
                                .unwrap_or_else(|_| NarDigest::new_unchecked(&nar_digest_str));

                            new_entries.insert(
                                store_hash,
                                IndexEntry {
                                    name,
                                    system: Some(system.clone()),
                                    narinfo_meta,
                                    nar_digest,
                                    nar_size: size.max(nar_size),
                                    added: Utc::now()
                                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                                    origin_job: Some(format!("job:{}", job_id)),
                                },
                            );
                            uploaded_count += 1;
                            total_bytes_uploaded += size;
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to upload NAR for {}: {}", hash, e);
                }
            }
        }
    }

    let mut active_gc_roots: Vec<StoreHash> = Vec::new();
    for p in &candidate_paths {
        if let Some(file_name) = Path::new(p).file_name().and_then(|n| n.to_str())
            && file_name.len() >= 32
            && let Ok(sh) = StoreHash::parse(&file_name[..32])
        {
            active_gc_roots.push(sh);
        }
    }
    active_gc_roots.sort();
    active_gc_roots.dedup();

    let pub_key = get_own_public_key(signing_key_file).await;
    let head_sha = env::var("GITHUB_SHA").ok();
    let ref_name = env::var("GITHUB_REF_NAME").ok();

    // 执行乐观并发 CAS 更新写入 run-<run_id>
    if !new_entries.is_empty() || !active_gc_roots.is_empty() {
        oci.update_run_session_with_cas(
            run_id,
            new_entries.clone(),
            active_gc_roots.clone(),
            system.clone(),
            job_id,
            head_sha.as_deref(),
            ref_name.as_deref(),
            pub_key.as_deref(),
            uploaded_count,
            total_bytes_uploaded,
            5,
        )
        .await?;
    }

    // 热注册到本机 Proxy
    if let Some(purl) = proxy_url
        && !new_entries.is_empty()
    {
        let register_endpoint = format!("{}/_session/register", purl.trim_end_matches('/'));
        let client = reqwest::Client::new();
        let _ = client
            .post(&register_endpoint)
            .json(&new_entries)
            .send()
            .await;
        info!(
            "Hot-registered {} entries to proxy at {}",
            new_entries.len(),
            register_endpoint
        );
    }

    // 写入 Schema v3 的 BuildReceipt
    if let Some(receipt_path) = output_receipt_path {
        let stats = BuildStats {
            discovered_outputs: candidate_paths.len(),
            built_paths: candidate_paths.len(),
            uploaded_blobs: uploaded_count,
            total_bytes_uploaded,
            substituted_paths: 0,
        };

        let receipt = BuildReceipt::new(
            system.clone(),
            repo.to_string(),
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            pub_key,
            new_entries,
            active_gc_roots,
            stats,
        )
        .with_run_info(Some(run_id), Some(job_id.to_string()));

        if let Some(parent) = receipt_path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = fs::create_dir_all(parent).await;
        }

        let receipt_json = serde_json::to_string_pretty(&receipt)?;
        fs::write(receipt_path, receipt_json).await?;
        info!("Build receipt written to {:?}", receipt_path);
    }

    write_session_capture_summary(
        job_id,
        system.as_str(),
        candidate_paths.len(),
        uploaded_count,
        total_bytes_uploaded,
    )
    .await;
    Ok(())
}

/// Session Clean: 清理本地快照文件
pub async fn run_session_clean(snapshot_path: Option<&Path>) -> Result<(), BuilderError> {
    if let Some(path) = snapshot_path
        && path.exists()
    {
        let _ = fs::remove_file(path).await;
        info!("Cleaned up snapshot file at {:?}", path);
    }
    Ok(())
}

/// 阶段 1: 编译构建阶段 (Worker / Matrix 节点)
pub async fn run_build_worker(
    build_config: &BuildConfig,
    repo: &str,
    registry: &str,
    signing_key_file: Option<&str>,
    github_token: &str,
    output_receipt_path: &Path,
    fail_fast: bool,
) -> Result<(), BuilderError> {
    let system = match &build_config.system {
        Some(s) if !s.trim().is_empty() => SystemArch::from(s.trim()),
        _ => SystemArch::from(nix::get_system().await?.as_str()),
    };

    info!(
        "Starting worker build for system: {} | Repo: {}/{}",
        system, registry, repo
    );

    // 1. 启动自替代代理
    let mut proxy_guard =
        setup_self_substituter(repo, registry, github_token, signing_key_file, fail_fast).await?;

    // 2. 发现目标
    let discovered = nix::discover_outputs(build_config).await?;
    info!("Discovered {} output target(s)", discovered.len());

    // 3. 构建目标
    let output_paths = nix::build_outputs(&discovered).await?;
    info!("Built {} top-level output path(s)", output_paths.len());

    // 4. 关闭代理
    proxy_guard.stop().await;

    // 5. 获取已有远端 hashes
    let oci = OciClient::new(registry, repo, github_token, true);
    let (_remote_index, own_hashes) = fetch_remote_cache_index(&oci).await;
    info!(
        "GHCR index contains {} previously-cached entries",
        own_hashes.len()
    );

    let own_hashes_vec: Vec<String> = own_hashes.into_iter().map(|h| h.into_inner()).collect();

    // 6. 查找本地新构建的路径
    let upload_list = nix::find_locally_built_paths(&output_paths, &own_hashes_vec).await?;

    let temp_dir = tempfile::tempdir()?;

    let mut new_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    let mut uploaded_count = 0;
    let mut total_bytes_uploaded = 0u64;

    if !upload_list.is_empty() {
        info!("Locally-built paths to upload: {}", upload_list.len());
        let exported =
            nix::export_paths_directly(&upload_list, signing_key_file, temp_dir.path()).await?;

        for (hash, store_path) in exported {
            let narinfo_path = temp_dir.path().join(format!("{}.narinfo", hash));
            let nar_file_path = temp_dir.path().join("nar").join(format!("{}.nar.xz", hash));

            if !nar_file_path.exists() {
                error!("NAR file not found for {}", hash);
                continue;
            }

            let metadata = fs::metadata(&nar_file_path).await?;
            let size = metadata.len();

            info!("  Uploading NAR for {} ({} bytes)", hash, size);
            match oci.push_blob(&nar_file_path).await {
                Ok(nar_digest_str) => {
                    if let Ok(narinfo_content) = fs::read_to_string(&narinfo_path).await {
                        let name = Path::new(&store_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .and_then(|s| s.split_once('-'))
                            .map(|x| x.1.to_string())
                            .unwrap_or(hash.clone());

                        if let Ok(narinfo) = NarInfo::parse(&narinfo_content) {
                            let (narinfo_meta, nar_size) = narinfo.into_meta();
                            let store_hash = narinfo_meta
                                .store_hash()
                                .unwrap_or_else(|| StoreHash::new_unchecked(&hash));
                            let nar_digest = NarDigest::parse(&nar_digest_str)
                                .unwrap_or_else(|_| NarDigest::new_unchecked(&nar_digest_str));

                            new_entries.insert(
                                store_hash,
                                IndexEntry {
                                    name,
                                    system: Some(system.clone()),
                                    narinfo_meta,
                                    nar_digest,
                                    nar_size: size.max(nar_size),
                                    added: Utc::now()
                                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                                    origin_job: env::var("GITHUB_JOB")
                                        .ok()
                                        .map(|j| format!("job:{}", j)),
                                },
                            );
                            uploaded_count += 1;
                            total_bytes_uploaded += size;
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to upload NAR for {}: {}", hash, e);
                }
            }
        }
    } else {
        info!("Nothing to upload — every path has an external or existing signature");
    }

    // 7. 提取 active_gc_roots
    let mut active_gc_roots: Vec<StoreHash> = Vec::new();
    for p in &output_paths {
        if let Some(file_name) = Path::new(p).file_name().and_then(|n| n.to_str())
            && file_name.len() >= 32
            && let Ok(sh) = StoreHash::parse(&file_name[..32])
        {
            active_gc_roots.push(sh);
        }
    }
    active_gc_roots.sort();
    active_gc_roots.dedup();

    let pub_key = get_own_public_key(signing_key_file).await;

    let stats = BuildStats {
        discovered_outputs: discovered.len(),
        built_paths: output_paths.len(),
        uploaded_blobs: uploaded_count,
        total_bytes_uploaded,
        substituted_paths: 0,
    };

    let run_id = env::var("GITHUB_RUN_ID")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let job_id = env::var("GITHUB_JOB").ok();

    let receipt = BuildReceipt::new(
        system.clone(),
        repo.to_string(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        pub_key,
        new_entries,
        active_gc_roots,
        stats,
    )
    .with_run_info(run_id, job_id);

    if let Some(parent) = output_receipt_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).await?;
    }

    let receipt_json = serde_json::to_string_pretty(&receipt)?;
    fs::write(output_receipt_path, receipt_json).await?;

    info!(
        "Build receipt written to {:?} (system: {}, uploaded: {} blobs)",
        output_receipt_path, system, uploaded_count
    );

    write_worker_step_summary(
        system.as_str(),
        discovered.len(),
        output_paths.len(),
        upload_list.len(),
        uploaded_count,
    )
    .await;
    Ok(())
}

/// Promote: 汇聚会话清单 (run-<run_id>) 与 Receipt，发布生产 cache-index
pub async fn run_promote(
    run_id: Option<u64>,
    receipt_paths: &[PathBuf],
    repo: &str,
    registry: &str,
    target_tag: &str,
    cleanup_session: bool,
    github_token: &str,
) -> Result<(), BuilderError> {
    info!(
        "Promoting cache to tag '{}' for repo: {}/{} (Run ID: {:?})",
        target_tag, registry, repo, run_id
    );

    let oci = OciClient::new(registry, repo, github_token, true);

    let mut index = match oci.get_cache_index(target_tag).await {
        Ok(Some((idx, _))) => idx,
        _ => CacheIndexData::default(),
    };

    index.version = CACHE_INDEX_VERSION;
    index.repo = repo.to_string();
    index.registry = registry.to_string();
    index.image = format!("{}/{}/nix-cache", registry, repo);
    index.generated = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    index.last_promoted_run = run_id;

    let mut total_promoted_entries = 0;
    let mut session_found = false;

    // 1. 从 OCI 会话清单 (run-<run_id>) 合并
    if let Some(rid) = run_id {
        let tag = format!("run-{}", rid);
        if let Ok(Some((session, _))) = oci.get_session_manifest(&tag).await {
            info!(
                "Found remote RunSessionManifest for tag {} with {} entries",
                tag,
                session.entries.len()
            );
            session_found = true;
            if let Some(ref pk) = session.public_key
                && !pk.is_empty()
            {
                index.public_key = pk.clone();
            }
            total_promoted_entries += session.entries.len();
            index.entries.extend(session.entries);

            for (sys, roots) in session.gc_roots {
                let system_roots = index.gc_roots.entry(sys).or_default();
                let mut set: HashSet<StoreHash> = system_roots.iter().cloned().collect();
                set.extend(roots);
                let mut sorted: Vec<StoreHash> = set.into_iter().collect();
                sorted.sort();
                *system_roots = sorted;
            }
        }
    }

    // 2. 从本地 Receipt 文件合并 (兼容多架构 artifacts)
    for p in receipt_paths {
        if p.is_dir() {
            if let Ok(mut dir_entries) = fs::read_dir(p).await {
                while let Ok(Some(entry)) = dir_entries.next_entry().await {
                    let file_path = entry.path();
                    if file_path.is_file()
                        && file_path.extension().and_then(|ext| ext.to_str()) == Some("json")
                    {
                        match fs::read_to_string(&file_path).await {
                            Ok(content) => match serde_json::from_str::<BuildReceipt>(&content) {
                                Ok(receipt) => {
                                    info!(
                                        "Loaded receipt from {:?} (system: {}, entries: {})",
                                        file_path,
                                        receipt.system,
                                        receipt.new_entries.len()
                                    );
                                    if let Some(ref pk) = receipt.public_key
                                        && !pk.is_empty()
                                    {
                                        index.public_key = pk.clone();
                                    }
                                    total_promoted_entries += receipt.new_entries.len();
                                    index.entries.extend(receipt.new_entries);
                                    let system_roots =
                                        index.gc_roots.entry(receipt.system.clone()).or_default();
                                    let mut set: HashSet<StoreHash> =
                                        system_roots.iter().cloned().collect();
                                    set.extend(receipt.active_gc_roots);
                                    let mut sorted: Vec<StoreHash> = set.into_iter().collect();
                                    sorted.sort();
                                    *system_roots = sorted;
                                }
                                Err(e) => {
                                    warn!("Could not parse receipt JSON at {:?}: {}", file_path, e);
                                }
                            },
                            Err(e) => {
                                warn!("Could not read file {:?}: {}", file_path, e);
                            }
                        }
                    }
                }
            }
        } else if p.is_file() {
            match fs::read_to_string(p).await {
                Ok(content) => match serde_json::from_str::<BuildReceipt>(&content) {
                    Ok(receipt) => {
                        info!(
                            "Loaded receipt from {:?} (system: {}, entries: {})",
                            p,
                            receipt.system,
                            receipt.new_entries.len()
                        );
                        if let Some(ref pk) = receipt.public_key
                            && !pk.is_empty()
                        {
                            index.public_key = pk.clone();
                        }
                        total_promoted_entries += receipt.new_entries.len();
                        index.entries.extend(receipt.new_entries);
                        let system_roots =
                            index.gc_roots.entry(receipt.system.clone()).or_default();
                        let mut set: HashSet<StoreHash> = system_roots.iter().cloned().collect();
                        set.extend(receipt.active_gc_roots);
                        let mut sorted: Vec<StoreHash> = set.into_iter().collect();
                        sorted.sort();
                        *system_roots = sorted;
                    }
                    Err(e) => {
                        warn!("Could not parse receipt JSON at {:?}: {}", p, e);
                    }
                },
                Err(e) => {
                    warn!("Could not read file {:?}: {}", p, e);
                }
            }
        }
    }

    if !session_found && receipt_paths.is_empty() && total_promoted_entries == 0 {
        info!("No session manifest or receipts found to promote. Pushing current baseline state.");
    }

    // 3. 发布更新后的全局 cache-index
    push_cache_index_data(&oci, &index, target_tag).await?;

    info!(
        "Promote complete! Global cache-index has {} total entries across {} systems.",
        index.entries.len(),
        index.gc_roots.len()
    );

    // 4. 清理会话标签
    if cleanup_session && let Some(rid) = run_id {
        let tag = format!("run-{}", rid);
        if let Ok(deleted) = oci.delete_manifest(&tag).await
            && deleted
        {
            info!("Cleaned up session tag {}", tag);
        }
    }

    write_promote_step_summary(
        run_id,
        target_tag,
        index.entries.len(),
        total_promoted_entries,
    )
    .await;
    Ok(())
}

/// 阶段 3: 跨平台垃圾回收阶段 (调用 nixcache_core 纯函数)
pub async fn run_gc(
    retention_days: u64,
    dry_run: bool,
    repo: &str,
    registry: &str,
    github_token: &str,
) -> Result<(), BuilderError> {
    info!(
        "Running multi-arch garbage collection for {}/{}",
        registry, repo
    );
    let oci = OciClient::new(registry, repo, github_token, true);

    let (mut index, _) = fetch_remote_cache_index(&oci).await;
    if index.entries.is_empty() {
        info!("No cache index found or index is empty, nothing to GC");
        return Ok(());
    }

    let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
    let gc_result = evaluate_multi_arch_gc(&index, &cutoff);

    info!(
        "GC Evaluation: Total: {}, Live Roots: {}, Kept: {}, To Delete: {}",
        index.entries.len(),
        gc_result.reachable_roots.len(),
        gc_result.kept_entries.len(),
        gc_result.deleted_hashes.len()
    );

    if dry_run {
        info!("Dry run complete. No modifications pushed.");
        return Ok(());
    }

    index.entries = gc_result.kept_entries;
    index.generated = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    push_cache_index_data(&oci, &index, "cache-index").await?;
    info!("Successfully updated cache-index after GC.");
    Ok(())
}

// GitHub Actions Summary 报告生成辅助函数
async fn write_session_init_summary(
    repo: &str,
    run_id: Option<u64>,
    branch: Option<&str>,
    port: u16,
) {
    if let Ok(summary_file) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "### 🚀 NixCache Session Initialized\n\n- **Repository:** `{}`\n- **Run ID:** `{:?}`\n- **Branch/PR:** `{:?}`\n- **Proxy Daemon Port:** `{}`\n",
            repo, run_id, branch, port
        );
        let _ = fs::write(&summary_file, content).await;
    }
}

async fn write_session_capture_summary(
    job_id: &str,
    system: &str,
    candidate_paths: usize,
    uploaded_blobs: usize,
    total_bytes: u64,
) {
    if let Ok(summary_file) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "### 📦 NixCache Session Captured\n\n- **Job:** `{}`\n- **System:** `{}`\n- **Candidate Paths:** `{}`\n- **Uploaded Blobs:** `{}`\n- **Uploaded Bytes:** `{}` bytes\n",
            job_id, system, candidate_paths, uploaded_blobs, total_bytes
        );
        let _ = fs::write(&summary_file, content).await;
    }
}

async fn write_worker_step_summary(
    system: &str,
    discovered: usize,
    built: usize,
    to_upload: usize,
    uploaded: usize,
) {
    if let Ok(summary_file) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "### 🔨 NixCache Worker Build\n\n- **System:** `{}`\n- **Discovered Outputs:** `{}`\n- **Built Outputs:** `{}`\n- **New Paths to Upload:** `{}`\n- **Uploaded Blobs:** `{}`\n",
            system, discovered, built, to_upload, uploaded
        );
        let _ = fs::write(&summary_file, content).await;
    }
}

async fn write_promote_step_summary(
    run_id: Option<u64>,
    target_tag: &str,
    total_entries: usize,
    promoted_entries: usize,
) {
    if let Ok(summary_file) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "### 🌟 NixCache Promotion Complete\n\n- **Run ID:** `{:?}`\n- **Target Tag:** `{}`\n- **Total Index Entries:** `{}`\n- **Promoted New Entries:** `{}`\n",
            run_id, target_tag, total_entries, promoted_entries
        );
        let _ = fs::write(&summary_file, content).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{restore_nix_conf, run_session_clean};
    use nixcache_core::{
        BuildReceipt, BuildStats, CacheIndexData, IndexEntry, NarDigest, NarInfoMeta, StoreHash,
        SystemArch, evaluate_multi_arch_gc,
    };
    use std::collections::{HashMap, HashSet};

    #[test]
    fn test_gc_multi_arch_aggregation() {
        let hash_x86_live = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let hash_arm_live = StoreHash::parse("00000000000000000000000000000002").unwrap();
        let hash_dead_old = StoreHash::parse("00000000000000000000000000000003").unwrap();
        let hash_dead_recent = StoreHash::parse("00000000000000000000000000000004").unwrap();

        let mut index = CacheIndexData::default();
        index.gc_roots.insert(
            SystemArch::X86_64Linux,
            vec![hash_x86_live.clone()],
        );
        index.gc_roots.insert(
            SystemArch::Aarch64Linux,
            vec![hash_arm_live.clone()],
        );

        let now = chrono::Utc::now();
        let sixty_days_ago = (now - chrono::Duration::days(60)).to_rfc3339();
        let five_days_ago = (now - chrono::Duration::days(5)).to_rfc3339();

        let entry_x86_live = IndexEntry {
            name: "pkg-x86".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_x86_live),
                nar_basename: "pkg-x86.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0".to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256("0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0").unwrap(),
            nar_size: 100,
            added: sixty_days_ago.clone(),
            origin_job: None,
        };
        let entry_arm_live = IndexEntry {
            name: "pkg-arm".to_string(),
            system: Some(SystemArch::Aarch64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_arm_live),
                nar_basename: "pkg-arm.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0".to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256("0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0").unwrap(),
            nar_size: 100,
            added: sixty_days_ago.clone(),
            origin_job: None,
        };
        let entry_dead_old = IndexEntry {
            name: "pkg-dead-old".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_dead_old),
                nar_basename: "pkg-dead-old.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0".to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256("0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0").unwrap(),
            nar_size: 100,
            added: sixty_days_ago.clone(),
            origin_job: None,
        };
        let entry_dead_recent = IndexEntry {
            name: "pkg-dead-recent".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_dead_recent),
                nar_basename: "pkg-dead-recent.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0".to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256("0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0").unwrap(),
            nar_size: 100,
            added: five_days_ago.clone(),
            origin_job: None,
        };

        index
            .entries
            .insert(hash_x86_live.clone(), entry_x86_live);
        index
            .entries
            .insert(hash_arm_live.clone(), entry_arm_live);
        index
            .entries
            .insert(hash_dead_old.clone(), entry_dead_old);
        index
            .entries
            .insert(hash_dead_recent.clone(), entry_dead_recent);

        let cutoff = now - chrono::Duration::days(30);
        let result = evaluate_multi_arch_gc(&index, &cutoff);

        assert_eq!(result.deleted_hashes, vec![hash_dead_old]);
        assert_eq!(result.kept_entries.len(), 3);
        assert!(result.kept_entries.contains_key(&hash_x86_live));
        assert!(result.kept_entries.contains_key(&hash_arm_live));
        assert!(result.kept_entries.contains_key(&hash_dead_recent));
    }

    #[test]
    fn test_gc_reachability_graph_algorithm() {
        let hash_shared_libc = StoreHash::parse("00000000000000000000000000000010").unwrap();
        let hash_x86_server = StoreHash::parse("00000000000000000000000000000011").unwrap();
        let hash_arm_server = StoreHash::parse("00000000000000000000000000000012").unwrap();
        let hash_darwin_client = StoreHash::parse("00000000000000000000000000000013").unwrap();
        let hash_orphan_ancient = StoreHash::parse("00000000000000000000000000000014").unwrap();
        let hash_orphan_recent = StoreHash::parse("00000000000000000000000000000015").unwrap();

        let mut index = CacheIndexData::default();
        index.gc_roots.insert(
            SystemArch::X86_64Linux,
            vec![
                hash_shared_libc.clone(),
                hash_x86_server.clone(),
            ],
        );
        index.gc_roots.insert(
            SystemArch::Aarch64Linux,
            vec![
                hash_shared_libc.clone(),
                hash_arm_server.clone(),
            ],
        );
        index.gc_roots.insert(
            SystemArch::Aarch64Darwin,
            vec![hash_darwin_client.clone()],
        );

        let now = chrono::Utc::now();
        let ninety_days_ago = (now - chrono::Duration::days(90)).to_rfc3339();
        let one_hour_ago = (now - chrono::Duration::hours(1)).to_rfc3339();

        let entries_def = vec![
            (hash_shared_libc.clone(), "glibc", ninety_days_ago.clone()),
            (hash_x86_server.clone(), "server-x86", ninety_days_ago.clone()),
            (hash_arm_server.clone(), "server-arm", ninety_days_ago.clone()),
            (hash_darwin_client.clone(), "client-mac", ninety_days_ago.clone()),
            (hash_orphan_ancient.clone(), "old-tool", ninety_days_ago.clone()),
            (hash_orphan_recent.clone(), "ci-temp", one_hour_ago),
        ];

        for (h, name, added) in entries_def {
            index.entries.insert(
                h.clone(),
                IndexEntry {
                    name: name.to_string(),
                    system: Some(SystemArch::X86_64Linux),
                    narinfo_meta: NarInfoMeta {
                        store_path: format!("/nix/store/{}-{}", h, name),
                        nar_basename: format!("{}.nar.xz", name),
                        nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0".to_string(),
                        ..Default::default()
                    },
                    nar_digest: NarDigest::new_sha256("0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0").unwrap(),
                    nar_size: 100,
                    added,
                    origin_job: None,
                },
            );
        }

        let cutoff = now - chrono::Duration::days(30);
        let result = evaluate_multi_arch_gc(&index, &cutoff);

        assert_eq!(result.deleted_hashes, vec![hash_orphan_ancient]);
        assert_eq!(result.kept_entries.len(), 5);
        assert!(result.kept_entries.contains_key(&hash_shared_libc));
        assert!(result.kept_entries.contains_key(&hash_orphan_recent));
    }

    #[tokio::test]
    async fn test_restore_nix_conf_logic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_conf = temp_dir.path().join("nix.conf");

        tokio::fs::write(&test_conf, "sandbox = true\n")
            .await
            .unwrap();

        let backup = Some((test_conf.clone(), "sandbox = true\n".to_string()));
        tokio::fs::write(
            &test_conf,
            "sandbox = true\nextra-substituters = http://localhost\n",
        )
        .await
        .unwrap();

        restore_nix_conf(backup).await;
        let restored = tokio::fs::read_to_string(&test_conf).await.unwrap();
        assert_eq!(restored, "sandbox = true\n");

        let new_file = temp_dir.path().join("new.conf");
        tokio::fs::write(&new_file, "extra-substituters = test\n")
            .await
            .unwrap();
        let empty_backup = Some((new_file.clone(), String::new()));
        restore_nix_conf(empty_backup).await;
        assert!(!new_file.exists());
    }

    #[tokio::test]
    async fn test_receipt_dir_parsing_in_promote() {
        let temp_dir = tempfile::tempdir().unwrap();
        let receipts_dir = temp_dir.path().join("receipts");
        tokio::fs::create_dir_all(&receipts_dir).await.unwrap();

        let root1 = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let root2 = StoreHash::parse("00000000000000000000000000000002").unwrap();

        let receipt1 = BuildReceipt::new(
            SystemArch::X86_64Linux,
            "test/repo".to_string(),
            "2026-08-28T00:00:00Z".to_string(),
            Some("key:pub".to_string()),
            HashMap::new(),
            vec![root1],
            BuildStats::default(),
        );

        let receipt2 = BuildReceipt::new(
            SystemArch::Aarch64Linux,
            "test/repo".to_string(),
            "2026-08-28T00:00:00Z".to_string(),
            Some("key:pub".to_string()),
            HashMap::new(),
            vec![root2],
            BuildStats::default(),
        );

        let r1_json = serde_json::to_string(&receipt1).unwrap();
        let r2_json = serde_json::to_string(&receipt2).unwrap();

        tokio::fs::write(receipts_dir.join("receipt-x86.json"), r1_json)
            .await
            .unwrap();
        tokio::fs::write(receipts_dir.join("receipt-arm.json"), r2_json)
            .await
            .unwrap();
        tokio::fs::write(receipts_dir.join("README.txt"), "some notes")
            .await
            .unwrap();

        let mut loaded = Vec::new();
        let mut dir_entries = tokio::fs::read_dir(&receipts_dir).await.unwrap();
        while let Ok(Some(entry)) = dir_entries.next_entry().await {
            let p = entry.path();
            if p.is_file() && p.extension().and_then(|ext| ext.to_str()) == Some("json") {
                let content = tokio::fs::read_to_string(&p).await.unwrap();
                let receipt: BuildReceipt = serde_json::from_str(&content).unwrap();
                loaded.push(receipt);
            }
        }

        assert_eq!(loaded.len(), 2);
        let systems: HashSet<SystemArch> = loaded.into_iter().map(|r| r.system).collect();
        assert!(systems.contains(&SystemArch::X86_64Linux));
        assert!(systems.contains(&SystemArch::Aarch64Linux));
    }

    #[tokio::test]
    async fn test_session_clean_logic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let snap_file = temp_dir.path().join("snapshot.txt");
        tokio::fs::write(&snap_file, "storepath1\nstorepath2\n")
            .await
            .unwrap();
        assert!(snap_file.exists());

        let res = run_session_clean(Some(&snap_file)).await;
        assert!(res.is_ok());
        assert!(!snap_file.exists());

        let res2 = run_session_clean(Some(&snap_file)).await;
        assert!(res2.is_ok());
    }
}
