use crate::{
    env_injector::NixEnvInjector,
    error::BuilderError,
    nix::{self, BuildConfig, driver::get_own_public_key},
    session::init::find_proxy_binary,
    summary::write_worker_step_summary,
};
use chrono::Utc;
use nixcache_core::{
    BuildReceipt, BuildStats, CacheIndexData, IndexEntry, StoreHash, SystemArch,
};
use nixcache_oci::OciClient;
use nixcache_oci_backend::{ReqwestTransport, create_tokio_reqwest_client};
use std::{
    collections::{HashMap, HashSet},
    env,
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{fs, process::Child, time::sleep};
use tracing::{error, info, warn};

pub struct ProxyGuard {
    child: Option<Child>,
}

impl ProxyGuard {
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }
}

impl Drop for ProxyGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

pub async fn setup_self_substituter(
    repo: &str,
    registry: &str,
    github_token: &str,
    signing_key_file: Option<&str>,
    fail_fast: bool,
) -> Result<ProxyGuard, BuilderError> {
    let proxy_bin = find_proxy_binary();
    info!("Starting self-substituter proxy using {:?}", proxy_bin);

    let mut proxy_cmd = tokio::process::Command::new(&proxy_bin);
    proxy_cmd
        .env("NIXCACHE_REPO", repo)
        .env("NIXCACHE_REGISTRY", registry)
        .env("NIXCACHE_PORT", "37515")
        .env("NIXCACHE_LISTEN", "127.0.0.1")
        .env("NIXCACHE_UPSTREAM", "")
        .env("GITHUB_TOKEN", github_token)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

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

    if ready {
        info!("Self-substituter running on port 37515");
        let mut keys = Vec::new();
        let pub_key = get_own_public_key(signing_key_file).await;
        if let Some(ref k) = pub_key {
            keys.push(k.as_str());
            info!("Trusted own public key: {}", k);
        }
        let nix_config = NixEnvInjector::generate_nix_config(&["http://127.0.0.1:37515"], &keys);
        let _ = NixEnvInjector::export_to_github_env(&nix_config).await;
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

    Ok(ProxyGuard { child: proxy_child })
}

pub async fn fetch_remote_cache_index(
    oci: &OciClient<ReqwestTransport>,
) -> (CacheIndexData, HashSet<StoreHash>) {
    let mut remote_index = CacheIndexData::default();
    let mut own_hashes = HashSet::new();

    if let Ok(Some((data, _))) = oci.get_cache_index("cache-index").await {
        own_hashes = data.entries.keys().cloned().collect();
        remote_index = data;
    }

    (remote_index, own_hashes)
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
    export_concurrency: usize,
) -> Result<(), BuilderError> {
    let system = match &build_config.system {
        Some(s) if !s.trim().is_empty() => SystemArch::from(s.trim()),
        _ => SystemArch::from(nix::get_system().await?.as_str()),
    };

    info!(
        "Starting worker build for system: {} | Repo: {}/{}",
        system, registry, repo
    );

    // 1. 启动自替代代理并注入 NIX_CONFIG
    let mut proxy_guard =
        setup_self_substituter(repo, registry, github_token, signing_key_file, fail_fast).await?;

    // 2. 发现目标
    let discovered = nix::discover_outputs(build_config).await?;
    info!("Discovered {} output target(s)", discovered.len());

    // 3. 构建目标
    let output_paths = nix::build_outputs(&discovered).await?;
    info!("Built {} top-level output path(s)", output_paths.len());

    // 4. 关闭代理并恢复配置
    proxy_guard.stop().await;

    // 5. 获取已有远端 hashes
    let oci = create_tokio_reqwest_client(registry, repo, github_token, true);
    let (_remote_index, own_hashes) = fetch_remote_cache_index(&oci).await;
    info!(
        "GHCR index contains {} previously-cached entries",
        own_hashes.len()
    );

    let own_hashes_vec: Vec<String> = own_hashes.into_iter().map(|h| h.into_inner()).collect();

    // 6. 查找本地新构建的路径并执行端到端无盘流式并行 Export
    let upload_list = nix::find_locally_built_paths(&output_paths, &own_hashes_vec).await?;

    let mut new_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    let mut uploaded_count = 0;
    let mut total_bytes_uploaded = 0u64;

    if !upload_list.is_empty() {
        info!(
            "Locally-built paths to export & upload in parallel: {} (concurrency: {})",
            upload_list.len(),
            export_concurrency
        );
        let export_config = nix::ParallelExportConfig {
            concurrency: export_concurrency,
            signing_key_file: signing_key_file.map(|s| s.to_string()),
            fail_fast,
            upload_config: nixcache_oci::UploadConfig::default(),
            system,
            origin_job: env::var("GITHUB_JOB").ok().map(|j| format!("job:{}", j)),
        };

        let report =
            nix::ParallelExporter::export_and_upload_paths(&upload_list, &oci, &export_config)
                .await?;

        for exported in report.successful {
            new_entries.insert(exported.store_hash, exported.index_entry);
            uploaded_count += 1;
            total_bytes_uploaded += exported.file_size;
        }

        if !report.failed.is_empty() {
            for (path, err) in &report.failed {
                error!("Failed to export & upload {}: {}", path, err);
            }
            if fail_fast {
                return Err(BuilderError::Other(format!(
                    "Parallel export failed for {} path(s)",
                    report.failed.len()
                )));
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
        system,
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
