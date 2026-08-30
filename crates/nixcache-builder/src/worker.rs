use crate::{
    env_injector::NixEnvInjector,
    error::BuilderError,
    nix::{self, BuildConfig, driver::get_own_public_key},
    session::init::find_proxy_binary,
    summary::write_worker_step_summary,
};
use chrono::Utc;
use nixcache_core::{BuildReceipt, BuildStats, IndexEntry, StoreHash, SystemArch};
use nixcache_oci::{OciClient, OciTransport};
use nixcache_oci_backend::create_tokio_reqwest_client;
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
    strict: bool,
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
            if strict {
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
        if strict {
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

pub async fn fetch_remote_arch_hashes<T: OciTransport + Clone>(
    oci: &OciClient<T>,
    system: &SystemArch,
) -> HashSet<StoreHash> {
    let mut own_hashes = HashSet::new();

    if let Ok(Some((root_data, _))) = oci.get_sharded_root_index("cache-index", system).await {
        let non_empty_shards: Vec<_> = root_data
            .shards
            .iter()
            .filter(|s| s.entry_count > 0 && !s.blob_digest.is_empty())
            .map(|s| s.blob_digest.clone())
            .collect();

        let futures = non_empty_shards.into_iter().map(|digest| {
            let oci = oci.clone();
            async move { oci.get_shard_data(&digest).await.ok() }
        });
        let payloads = futures_util::future::join_all(futures).await;
        for payload in payloads.into_iter().flatten() {
            own_hashes.extend(payload.entries.into_keys());
        }
    }

    own_hashes
}

#[allow(dead_code)]
pub async fn fetch_remote_cache_hashes<T: OciTransport + Clone>(
    oci: &OciClient<T>,
) -> HashSet<StoreHash> {
    let mut own_hashes = HashSet::new();
    for sys in SystemArch::all() {
        let hashes = fetch_remote_arch_hashes(oci, &sys).await;
        own_hashes.extend(hashes);
    }
    own_hashes
}

#[derive(Debug, Clone)]
pub struct BuildWorkerOptions<'a> {
    pub build_config: &'a BuildConfig,
    pub repo: &'a str,
    pub registry: &'a str,
    pub signing_key_file: Option<&'a str>,
    pub github_token: &'a str,
    pub output_receipt_path: &'a Path,
    pub strict: bool,
    pub export_concurrency: usize,
}

/// 阶段 1: 编译构建阶段 (Worker / Matrix 节点)
pub async fn run_build_worker(opts: &BuildWorkerOptions<'_>) -> Result<(), BuilderError> {
    let system = match &opts.build_config.system {
        Some(s) if !s.trim().is_empty() => SystemArch::from(s.trim()),
        _ => SystemArch::from(nix::get_system().await?.as_str()),
    };

    info!(
        "Starting worker build for system: {} | Repo: {}/{}",
        system, opts.registry, opts.repo
    );

    // 1. 启动自替代代理并注入 NIX_CONFIG
    let mut proxy_guard = setup_self_substituter(
        opts.repo,
        opts.registry,
        opts.github_token,
        opts.signing_key_file,
        opts.strict,
    )
    .await?;

    // 2. 发现目标
    let discovered = nix::discover_outputs(opts.build_config).await?;
    info!("Discovered {} output target(s)", discovered.len());

    // 3. 构建目标
    let output_paths = nix::build_outputs(&discovered).await?;
    info!("Built {} top-level output path(s)", output_paths.len());

    // 4. 关闭代理并恢复配置
    proxy_guard.stop().await;

    // 5. 获取已有远端 hashes
    let oci = create_tokio_reqwest_client(opts.registry, opts.repo, opts.github_token, true);
    let own_hashes = fetch_remote_arch_hashes(&oci, &system).await;
    info!(
        "Remote index contains {} previously-cached entries for {}",
        own_hashes.len(),
        system
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
            opts.export_concurrency
        );
        let export_config = nix::ParallelExportConfig {
            concurrency: opts.export_concurrency,
            signing_key_file: opts.signing_key_file.map(|s| s.to_string()),
            strict: opts.strict,
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
            if opts.strict {
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

    let pub_key = get_own_public_key(opts.signing_key_file).await;

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
        opts.repo.to_string(),
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        pub_key,
        new_entries,
        active_gc_roots,
        stats,
    )
    .with_run_info(run_id, job_id);

    if let Some(parent) = opts.output_receipt_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).await?;
    }

    let receipt_json = serde_json::to_string(&receipt)?;
    fs::write(opts.output_receipt_path, receipt_json).await?;

    info!(
        "Build receipt written to {:?} (system: {}, uploaded: {} blobs)",
        opts.output_receipt_path, system, uploaded_count
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

#[cfg(test)]
mod tests {
    use super::*;
    use nixcache_core::{
        FastBlockedBloomFilter, IndexEntry, ShardDataPayload, ShardedArchCacheIndexData, StoreHash,
        SystemArch,
    };
    use nixcache_oci::MockRouterTransport;

    #[tokio::test]
    async fn test_fetch_remote_arch_hashes_empty() {
        let transport = MockRouterTransport::default();
        let client = OciClient::with_transport("example.com", "test/repo", "", false, transport);
        let hashes = fetch_remote_arch_hashes(&client, &SystemArch::X86_64Linux).await;
        assert!(hashes.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_remote_arch_hashes_populated() {
        let transport = MockRouterTransport::default();
        let client = OciClient::with_transport("example.com", "test/repo", "", true, transport);

        let h1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let mut payload = ShardDataPayload::new(h1.shard_id());
        payload.entries.insert(h1.clone(), IndexEntry::default());

        let (shard_digest, comp_size, uncomp_size) =
            client.push_shard_data(&payload).await.unwrap();

        let mut root_data =
            ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "example.com");
        let sid = h1.shard_id() as usize;
        root_data.shards[sid].blob_digest = shard_digest;
        root_data.shards[sid].compressed_size = comp_size;
        root_data.shards[sid].uncompressed_size = uncomp_size;
        root_data.shards[sid].entry_count = 1;
        root_data.shards[sid].merkle_hash = payload.compute_merkle_hash();
        root_data.recalculate_merkle_root();

        let bloom = FastBlockedBloomFilter::new_with_defaults(10);
        let bf_manifest = client.push_bloom_filter(&bloom).await.unwrap();

        client
            .push_sharded_root_index(
                "cache-index-x86_64-linux",
                &root_data,
                &bf_manifest.blob_digest,
                bf_manifest.compressed_size,
                None,
            )
            .await
            .unwrap();

        let hashes = fetch_remote_arch_hashes(&client, &SystemArch::X86_64Linux).await;
        assert_eq!(hashes.len(), 1);
        assert!(hashes.contains(&h1));

        let all_hashes = fetch_remote_cache_hashes(&client).await;
        assert_eq!(all_hashes.len(), 1);
        assert!(all_hashes.contains(&h1));
    }
}
