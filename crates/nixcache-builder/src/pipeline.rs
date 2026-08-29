use crate::nix::{self, BuildConfig};
use chrono::{DateTime, Utc};
use nixcache_oci::{
    BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, OciClient,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    env,
    path::{Path, PathBuf},
    time::Duration,
};
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
) -> Result<Option<(PathBuf, String)>, String> {
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
                fs::write(&user_conf_path, user_new)
                    .await
                    .map_err(|err| format!("Failed to write user nix.conf: {}", err))?;
                info!("Added self-substituter to {:?}", user_conf_path);
                Ok(Some((user_conf_path, user_original)))
            } else {
                Err(format!(
                    "Permission denied for /etc/nix/nix.conf and HOME env is not set: {}",
                    e
                ))
            }
        }
        Err(e) => Err(format!("Failed to write nix.conf: {}", e)),
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

async fn setup_self_substituter(
    repo: &str,
    registry: &str,
    github_token: &str,
    signing_key_file: Option<&str>,
    fail_fast: bool,
) -> Result<ProxyGuard, String> {
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
                return Err(format!("Failed to spawn nixcache-proxy: {}", e));
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
            return Err(
                "Self-substituter proxy failed to become ready (timed out after 15s)".to_string(),
            );
        }
        info!("Self-substituter failed to respond, proceeding without it");
    }

    Ok(ProxyGuard {
        child: proxy_child,
        nix_conf_backup,
    })
}

async fn fetch_remote_cache_index(oci: &OciClient) -> (CacheIndexData, HashSet<String>) {
    let mut remote_index = CacheIndexData::default();
    let mut own_hashes = HashSet::new();

    if let Ok(Some(manifest_json)) = oci.get_manifest("cache-index").await
        && let Ok(manifest) = serde_json::from_str::<Value>(&manifest_json)
        && let Some(layers) = manifest.get("layers").and_then(|l| l.as_array())
        && !layers.is_empty()
        && let Some(digest) = layers[0].get("digest").and_then(|d| d.as_str())
        && let Ok(blob_bytes) = oci.get_blob(digest).await
        && let Ok(data) = serde_json::from_slice::<CacheIndexData>(&blob_bytes)
    {
        own_hashes = data.entries.keys().cloned().collect();
        remote_index = data;
    }

    (remote_index, own_hashes)
}

async fn push_cache_index_data(
    oci: &OciClient,
    index: &CacheIndexData,
    temp_dir: &Path,
    tag: &str,
) -> Result<(), String> {
    let index_json = serde_json::to_string_pretty(index)
        .map_err(|e| format!("Failed to serialize cache index: {}", e))?;

    let index_temp_path = temp_dir.join("cache-index.json");
    fs::write(&index_temp_path, &index_json)
        .await
        .map_err(|e| format!("Failed to write index temp: {}", e))?;

    let index_digest = oci
        .push_blob(&index_temp_path)
        .await
        .map_err(|e| e.to_string())?;
    let index_size = fs::metadata(&index_temp_path)
        .await
        .map_err(|e| format!("Failed to get index size: {}", e))?
        .len();

    let config_temp_path = temp_dir.join("config.json");
    fs::write(&config_temp_path, "{}")
        .await
        .map_err(|e| format!("Failed to write config temp: {}", e))?;
    let config_digest = oci
        .push_blob(&config_temp_path)
        .await
        .map_err(|e| e.to_string())?;
    let config_size = 2;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_size
        },
        "layers": [{
            "mediaType": "application/vnd.nix.cache.index.v1+json",
            "digest": index_digest,
            "size": index_size
        }]
    });

    oci.push_manifest(tag, &manifest.to_string())
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

async fn record_store_snapshot(snap_path: &Path) -> Result<(), String> {
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
    fs::write(snap_path, content)
        .await
        .map_err(|e| format!("Failed to write store snapshot to {:?}: {}", snap_path, e))?;
    info!(
        "Recorded store snapshot with {} paths to {:?}",
        paths.len(),
        snap_path
    );
    Ok(())
}

/// Session Init: 启动后台 Proxy 守护进程，配置 nix.conf，并记录 store 快照
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
) -> Result<(), String> {
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

    let _child = proxy_cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn nixcache-proxy: {}", e))?;

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
        return Err(format!(
            "Proxy failed to become ready on http://{}:{}",
            listen, port
        ));
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
) -> Result<(), String> {
    let system = match system_opt {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => nix::get_system().await?,
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
    let temp_dir = tempfile::tempdir().map_err(|e| format!("Failed to create tempdir: {}", e))?;

    let mut new_entries = HashMap::new();
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

            let metadata = fs::metadata(&nar_file_path)
                .await
                .map_err(|e| e.to_string())?;
            let size = metadata.len();

            info!("  Uploading NAR for {} ({} bytes)", hash, size);
            match oci.push_blob(&nar_file_path).await {
                Ok(nar_digest) => {
                    if let Ok(narinfo_content) = fs::read_to_string(&narinfo_path).await {
                        let name = Path::new(&store_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .and_then(|s| s.split_once('-'))
                            .map(|x| x.1.to_string())
                            .unwrap_or(hash.clone());

                        new_entries.insert(
                            hash.clone(),
                            IndexEntry {
                                name,
                                system: Some(system.clone()),
                                narinfo: narinfo_content,
                                nar_digest,
                                nar_size: size,
                                added: Utc::now()
                                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                                origin_job: Some(format!("job:{}", job_id)),
                            },
                        );
                        uploaded_count += 1;
                        total_bytes_uploaded += size;
                    }
                }
                Err(e) => {
                    error!("Failed to upload NAR for {}: {}", hash, e);
                }
            }
        }
    }

    let mut active_gc_roots = Vec::new();
    for p in &candidate_paths {
        if let Some(file_name) = Path::new(p).file_name().and_then(|n| n.to_str())
            && file_name.len() >= 32
        {
            active_gc_roots.push(file_name[..32].to_string());
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
            &system,
            job_id,
            head_sha.as_deref(),
            ref_name.as_deref(),
            pub_key.as_deref(),
            uploaded_count,
            total_bytes_uploaded,
            5,
        )
        .await
        .map_err(|e| format!("Failed to update run session with CAS: {}", e))?;
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

        let receipt_json = serde_json::to_string_pretty(&receipt).map_err(|e| e.to_string())?;
        fs::write(receipt_path, receipt_json)
            .await
            .map_err(|e| e.to_string())?;
        info!("Build receipt written to {:?}", receipt_path);
    }

    write_session_capture_summary(
        job_id,
        &system,
        candidate_paths.len(),
        uploaded_count,
        total_bytes_uploaded,
    )
    .await;
    Ok(())
}

/// Session Clean: 清理本地快照文件
pub async fn run_session_clean(snapshot_path: Option<&Path>) -> Result<(), String> {
    if let Some(path) = snapshot_path
        && path.exists()
    {
        let _ = fs::remove_file(path).await;
        info!("Cleaned up snapshot file at {:?}", path);
    }
    Ok(())
}

/// 阶段 1: 编译构建阶段 (Worker / Matrix 节点)
/// 负责编译 Nix 目标、上传 NAR Blobs，并输出 BuildReceipt 文件，不发布 cache-index
pub async fn run_build_worker(
    build_config: &BuildConfig,
    repo: &str,
    registry: &str,
    signing_key_file: Option<&str>,
    github_token: &str,
    output_receipt_path: &Path,
    fail_fast: bool,
) -> Result<(), String> {
    let system = match &build_config.system {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => nix::get_system().await?,
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

    let own_hashes_vec: Vec<String> = own_hashes.into_iter().collect();

    // 6. 查找本地新构建的路径
    let upload_list = nix::find_locally_built_paths(&output_paths, &own_hashes_vec).await?;

    let temp_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temporary directory: {}", e))?;

    let mut new_entries = HashMap::new();
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

            let metadata = fs::metadata(&nar_file_path)
                .await
                .map_err(|e| format!("Failed to read nar file metadata: {}", e))?;
            let size = metadata.len();

            info!("  Uploading NAR for {} ({} bytes)", hash, size);
            match oci.push_blob(&nar_file_path).await {
                Ok(nar_digest) => {
                    if let Ok(narinfo_content) = fs::read_to_string(&narinfo_path).await {
                        let name = Path::new(&store_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .and_then(|s| s.split_once('-'))
                            .map(|x| x.1.to_string())
                            .unwrap_or(hash.clone());

                        new_entries.insert(
                            hash.clone(),
                            IndexEntry {
                                name,
                                system: Some(system.clone()),
                                narinfo: narinfo_content,
                                nar_digest,
                                nar_size: size,
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
                Err(e) => {
                    error!("Failed to upload NAR for {}: {}", hash, e);
                }
            }
        }
    } else {
        info!("Nothing to upload — every path has an external or existing signature");
    }

    // 7. 提取 active_gc_roots
    let mut active_gc_roots = Vec::new();
    for p in &output_paths {
        if let Some(file_name) = Path::new(p).file_name().and_then(|n| n.to_str())
            && file_name.len() >= 32
        {
            active_gc_roots.push(file_name[..32].to_string());
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
        fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create receipt directory: {}", e))?;
    }

    let receipt_json = serde_json::to_string_pretty(&receipt)
        .map_err(|e| format!("Failed to serialize receipt: {}", e))?;
    fs::write(output_receipt_path, receipt_json)
        .await
        .map_err(|e| format!("Failed to write receipt file: {}", e))?;

    info!(
        "Build receipt written to {:?} (system: {}, uploaded: {} blobs)",
        output_receipt_path, system, uploaded_count
    );

    write_worker_step_summary(
        &system,
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
) -> Result<(), String> {
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
                let mut set: HashSet<String> = system_roots.iter().cloned().collect();
                set.extend(roots);
                let mut sorted: Vec<String> = set.into_iter().collect();
                sorted.sort();
                *system_roots = sorted;
            }
        }
    }

    // 2. 从本地 Receipt 文件合并 (兼容旧版及多架构 artifacts)
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
                                    let mut set: HashSet<String> =
                                        system_roots.iter().cloned().collect();
                                    set.extend(receipt.active_gc_roots);
                                    let mut sorted: Vec<String> = set.into_iter().collect();
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
                        let mut set: HashSet<String> = system_roots.iter().cloned().collect();
                        set.extend(receipt.active_gc_roots);
                        let mut sorted: Vec<String> = set.into_iter().collect();
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
    let temp_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temporary directory: {}", e))?;
    push_cache_index_data(&oci, &index, temp_dir.path(), target_tag).await?;

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

/// 兼容接口：run_merge_coordinator
pub async fn run_merge_coordinator(
    receipt_paths: &[PathBuf],
    repo: &str,
    registry: &str,
    github_token: &str,
) -> Result<(), String> {
    run_promote(
        None,
        receipt_paths,
        repo,
        registry,
        "cache-index",
        false,
        github_token,
    )
    .await
}

/// 阶段 3: 跨平台垃圾回收阶段
pub async fn run_gc(
    retention_days: u64,
    dry_run: bool,
    repo: &str,
    registry: &str,
    github_token: &str,
) -> Result<(), String> {
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

    info!(
        "Loaded index with {} total entries across systems: {:?}",
        index.entries.len(),
        index.gc_roots.keys().collect::<Vec<_>>()
    );

    // 聚合所有系统的 GC Roots
    let all_live_roots: HashSet<String> = index
        .gc_roots
        .values()
        .flat_map(|roots| roots.iter().cloned())
        .collect();

    info!(
        "Aggregated {} live GC root(s) across all systems",
        all_live_roots.len()
    );

    let now = Utc::now();
    let retention_duration = chrono::Duration::days(retention_days as i64);
    let cutoff = now - retention_duration;

    info!(
        "Cutoff date: {}",
        cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );

    let mut kept_entries = HashMap::new();
    let mut to_delete_count = 0;

    for (hash, entry) in index.entries {
        let is_live = all_live_roots.contains(&hash);
        let added_dt = DateTime::parse_from_rfc3339(&entry.added)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(now);

        let is_old = added_dt < cutoff;

        if !is_live && is_old {
            to_delete_count += 1;
            info!(
                "DELETE: {} ({}, system={:?}) added={}",
                hash, entry.name, entry.system, entry.added
            );
        } else {
            kept_entries.insert(hash.clone(), entry);
            let reason = if is_live { "live" } else { "recent" };
            info!("KEEP:   {} reason={}", hash, reason);
        }
    }

    info!(
        "\n>>> Multi-Arch GC Total: {} keep, {} delete",
        kept_entries.len(),
        to_delete_count
    );

    if dry_run {
        info!(">>> Dry run, no changes written");
        return Ok(());
    }

    if to_delete_count > 0 {
        index.entries = kept_entries;
        // 清理 gc_roots 中已不存在的 hash
        for roots in index.gc_roots.values_mut() {
            roots.retain(|h| index.entries.contains_key(h));
        }

        let temp_dir = tempfile::tempdir()
            .map_err(|e| format!("Failed to create temporary directory: {}", e))?;

        push_cache_index_data(&oci, &index, temp_dir.path(), "cache-index").await?;
        info!(">>> Multi-arch GC complete, index updated");
    }

    Ok(())
}

/// 阶段 4: 单机全流程快捷命令 (兼容本地/单一架构项目)
pub async fn run_all_in_one(
    build_config: &BuildConfig,
    repo: &str,
    registry: &str,
    signing_key_file: Option<&str>,
    github_token: &str,
    fail_fast: bool,
) -> Result<(), String> {
    let system = match &build_config.system {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => nix::get_system().await?,
    };

    info!(
        "Starting all-in-one build & publish for system: {} | Repo: {}/{}",
        system, registry, repo
    );

    let temp_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temporary directory: {}", e))?;
    let receipt_path = temp_dir.path().join(format!("receipt-{}.json", system));

    // 1. 执行 Worker 构建与上传
    run_build_worker(
        build_config,
        repo,
        registry,
        signing_key_file,
        github_token,
        &receipt_path,
        fail_fast,
    )
    .await?;

    // 2. 执行 Coordinator 汇聚与发布
    run_merge_coordinator(&[receipt_path], repo, registry, github_token).await?;

    info!("All-in-one pipeline complete!");
    Ok(())
}

async fn write_session_init_summary(
    repo: &str,
    run_id: Option<u64>,
    branch: Option<&str>,
    port: u16,
) {
    if let Ok(summary_path) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "## NixCache Session Init Summary\n\n| Property | Value |\n|---|---|\n| Repository | `{}` |\n| Run ID (Tier 1) | `{}` |\n| Branch/PR (Tier 2) | `{}` |\n| Proxy Port | `{}` |\n| Status | Ready (Cascading Resolver Active) |\n",
            repo,
            run_id
                .map(|r| r.to_string())
                .unwrap_or_else(|| "N/A".to_string()),
            branch.unwrap_or("N/A"),
            port
        );
        let _ = fs::write(&summary_path, content).await;
    }
}

async fn write_session_capture_summary(
    job_id: &str,
    system: &str,
    discovered: usize,
    uploaded: usize,
    bytes_uploaded: u64,
) {
    if let Ok(summary_path) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "## NixCache Session Capture Summary\n\n| Metric | Value |\n|---|---|\n| Job ID | `{}` |\n| System | `{}` |\n| Candidate Paths Scanned | {} |\n| Blobs Uploaded | {} |\n| Bytes Uploaded | {} bytes |\n",
            job_id, system, discovered, uploaded, bytes_uploaded
        );
        let _ = fs::write(&summary_path, content).await;
    }
}

async fn write_promote_step_summary(
    run_id: Option<u64>,
    target_tag: &str,
    total_entries: usize,
    promoted_entries: usize,
) {
    if let Ok(summary_path) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "## NixCache Promote Summary\n\n| Metric | Value |\n|---|---|\n| Run ID Promoted | `{}` |\n| Target Tag | `{}` |\n| Entries Promoted from Run | {} |\n| Total Global Index Entries | {} |\n",
            run_id
                .map(|r| r.to_string())
                .unwrap_or_else(|| "N/A".to_string()),
            target_tag,
            promoted_entries,
            total_entries
        );
        let _ = fs::write(&summary_path, content).await;
    }
}

async fn write_worker_step_summary(
    system: &str,
    discovered: usize,
    built: usize,
    upload_candidates: usize,
    uploaded: usize,
) {
    if let Ok(summary_path) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "## NixCache Build Summary ({})\n\n| Metric | Count |\n|---|---|\n| System Architecture | `{}` |\n| Outputs discovered | {} |\n| Output paths built | {} |\n| New paths to upload | {} |\n| Successfully uploaded | {} |\n",
            system, system, discovered, built, upload_candidates, uploaded
        );
        let _ = fs::write(&summary_path, content).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixcache_oci::{BuildReceipt, BuildStats, CacheIndexData, IndexEntry};

    #[test]
    fn test_merge_receipts_in_memory() {
        let mut index = CacheIndexData {
            version: 3,
            ..Default::default()
        };

        let entry1 = IndexEntry {
            name: "pkg-x86".to_string(),
            system: Some("x86_64-linux".to_string()),
            ..Default::default()
        };

        let receipt1 = BuildReceipt::new(
            "x86_64-linux".to_string(),
            "test/repo".to_string(),
            "2026-08-28T00:00:00Z".to_string(),
            Some("key-1".to_string()),
            HashMap::from([("hash-x86-1".to_string(), entry1)]),
            vec!["hash-x86-root-1".to_string()],
            BuildStats::default(),
        );

        let entry2 = IndexEntry {
            name: "pkg-arm".to_string(),
            system: Some("aarch64-linux".to_string()),
            ..Default::default()
        };

        let receipt2 = BuildReceipt::new(
            "aarch64-linux".to_string(),
            "test/repo".to_string(),
            "2026-08-28T00:00:00Z".to_string(),
            Some("key-1".to_string()),
            HashMap::from([("hash-arm-1".to_string(), entry2)]),
            vec!["hash-arm-root-1".to_string()],
            BuildStats::default(),
        );

        let receipts = vec![receipt1, receipt2];

        for r in &receipts {
            index.entries.extend(r.new_entries.clone());
            let roots = index.gc_roots.entry(r.system.clone()).or_default();
            roots.extend(r.active_gc_roots.clone());
        }

        assert_eq!(index.version, 3);
        assert_eq!(index.entries.len(), 2);
        assert!(index.entries.contains_key("hash-x86-1"));
        assert!(index.entries.contains_key("hash-arm-1"));
        assert_eq!(index.gc_roots.get("x86_64-linux").unwrap().len(), 1);
        assert_eq!(index.gc_roots.get("aarch64-linux").unwrap().len(), 1);
    }

    #[test]
    fn test_gc_multi_arch_aggregation() {
        let mut index = CacheIndexData::default();
        index.gc_roots.insert(
            "x86_64-linux".to_string(),
            vec!["hash-x86-live".to_string()],
        );
        index.gc_roots.insert(
            "aarch64-linux".to_string(),
            vec!["hash-arm-live".to_string()],
        );

        let entry_x86_live = IndexEntry {
            name: "live-x86".to_string(),
            added: "2020-01-01T00:00:00Z".to_string(),
            ..Default::default()
        };

        let entry_arm_live = IndexEntry {
            name: "live-arm".to_string(),
            added: "2020-01-01T00:00:00Z".to_string(),
            ..Default::default()
        };

        let entry_dead_old = IndexEntry {
            name: "dead-old".to_string(),
            added: "2020-01-01T00:00:00Z".to_string(),
            ..Default::default()
        };

        let entry_dead_recent = IndexEntry {
            name: "dead-recent".to_string(),
            added: Utc::now().to_rfc3339(),
            ..Default::default()
        };

        index
            .entries
            .insert("hash-x86-live".to_string(), entry_x86_live);
        index
            .entries
            .insert("hash-arm-live".to_string(), entry_arm_live);
        index
            .entries
            .insert("hash-dead-old".to_string(), entry_dead_old);
        index
            .entries
            .insert("hash-dead-recent".to_string(), entry_dead_recent);

        let all_live_roots: HashSet<String> = index
            .gc_roots
            .values()
            .flat_map(|roots| roots.iter().cloned())
            .collect();

        assert_eq!(all_live_roots.len(), 2);
        assert!(all_live_roots.contains("hash-x86-live"));
        assert!(all_live_roots.contains("hash-arm-live"));

        let cutoff = Utc::now() - chrono::Duration::days(30);

        let mut kept = HashMap::new();
        let mut deleted = Vec::new();

        for (hash, entry) in index.entries {
            let is_live = all_live_roots.contains(&hash);
            let added_dt = DateTime::parse_from_rfc3339(&entry.added)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let is_old = added_dt < cutoff;

            if !is_live && is_old {
                deleted.push(hash);
            } else {
                kept.insert(hash, entry);
            }
        }

        assert_eq!(deleted, vec!["hash-dead-old"]);
        assert_eq!(kept.len(), 3);
        assert!(kept.contains_key("hash-x86-live"));
        assert!(kept.contains_key("hash-arm-live"));
        assert!(kept.contains_key("hash-dead-recent"));
    }

    #[test]
    fn test_gc_reachability_graph_algorithm() {
        let mut index = CacheIndexData::default();
        // 构造三架构共享依赖图
        index.gc_roots.insert(
            "x86_64-linux".to_string(),
            vec![
                "hash-shared-libc".to_string(),
                "hash-x86-server".to_string(),
            ],
        );
        index.gc_roots.insert(
            "aarch64-linux".to_string(),
            vec![
                "hash-shared-libc".to_string(),
                "hash-arm-server".to_string(),
            ],
        );
        index.gc_roots.insert(
            "aarch64-darwin".to_string(),
            vec!["hash-darwin-client".to_string()],
        );

        let now = Utc::now();
        let ninety_days_ago = (now - chrono::Duration::days(90)).to_rfc3339();
        let one_hour_ago = (now - chrono::Duration::hours(1)).to_rfc3339();

        let entries_def = vec![
            ("hash-shared-libc", "glibc", ninety_days_ago.clone()),
            ("hash-x86-server", "server-x86", ninety_days_ago.clone()),
            ("hash-arm-server", "server-arm", ninety_days_ago.clone()),
            ("hash-darwin-client", "client-mac", ninety_days_ago.clone()),
            ("hash-orphan-ancient", "old-tool", ninety_days_ago.clone()),
            ("hash-orphan-recent", "ci-temp", one_hour_ago),
        ];

        for (h, name, added) in entries_def {
            index.entries.insert(
                h.to_string(),
                IndexEntry {
                    name: name.to_string(),
                    added,
                    ..Default::default()
                },
            );
        }

        let all_live_roots: HashSet<String> = index
            .gc_roots
            .values()
            .flat_map(|roots| roots.iter().cloned())
            .collect();

        let cutoff = now - chrono::Duration::days(30);
        let mut kept = HashMap::new();
        let mut deleted = Vec::new();

        for (hash, entry) in index.entries {
            let is_live = all_live_roots.contains(&hash);
            let added_dt = DateTime::parse_from_rfc3339(&entry.added)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(now);

            if !is_live && added_dt < cutoff {
                deleted.push(hash);
            } else {
                kept.insert(hash, entry);
            }
        }

        // 仅有孤立且超过 30 天的 hash-orphan-ancient 被删除
        assert_eq!(deleted, vec!["hash-orphan-ancient"]);
        assert_eq!(kept.len(), 5);
        assert!(kept.contains_key("hash-shared-libc"));
        assert!(kept.contains_key("hash-orphan-recent"));
    }

    #[tokio::test]
    async fn test_restore_nix_conf_logic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_conf = temp_dir.path().join("nix.conf");

        // 初始写入原有内容
        tokio::fs::write(&test_conf, "sandbox = true\n")
            .await
            .unwrap();

        let backup = Some((test_conf.clone(), "sandbox = true\n".to_string()));
        // 模拟被修改后的内容
        tokio::fs::write(
            &test_conf,
            "sandbox = true\nextra-substituters = http://localhost\n",
        )
        .await
        .unwrap();

        // 恢复
        restore_nix_conf(backup).await;
        let restored = tokio::fs::read_to_string(&test_conf).await.unwrap();
        assert_eq!(restored, "sandbox = true\n");

        // 测试原文件不存在时恢复为删除
        let new_file = temp_dir.path().join("new.conf");
        tokio::fs::write(&new_file, "extra-substituters = test\n")
            .await
            .unwrap();
        let empty_backup = Some((new_file.clone(), String::new()));
        restore_nix_conf(empty_backup).await;
        assert!(!new_file.exists());
    }

    #[tokio::test]
    async fn test_receipt_dir_parsing_in_merge() {
        let temp_dir = tempfile::tempdir().unwrap();
        let receipts_dir = temp_dir.path().join("receipts");
        tokio::fs::create_dir_all(&receipts_dir).await.unwrap();

        let receipt1 = BuildReceipt::new(
            "x86_64-linux".to_string(),
            "test/repo".to_string(),
            "2026-08-28T00:00:00Z".to_string(),
            Some("key:pub".to_string()),
            HashMap::new(),
            vec!["root1".to_string()],
            BuildStats::default(),
        );

        let receipt2 = BuildReceipt::new(
            "aarch64-linux".to_string(),
            "test/repo".to_string(),
            "2026-08-28T00:00:00Z".to_string(),
            Some("key:pub".to_string()),
            HashMap::new(),
            vec!["root2".to_string()],
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
        // 忽略非 json 文件
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
        let systems: HashSet<String> = loaded.into_iter().map(|r| r.system).collect();
        assert!(systems.contains("x86_64-linux"));
        assert!(systems.contains("aarch64-linux"));
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

        // 清理不存在的文件也不会报错
        let res2 = run_session_clean(Some(&snap_file)).await;
        assert!(res2.is_ok());
    }
}
