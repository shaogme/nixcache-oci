use crate::{
    error::BuilderError,
    nix::{
        self, CaptureMode, ClosureEngine, TargetResolver,
        driver::get_own_public_key,
        exporter::{ParallelExportConfig, ParallelExporter},
        filter::{NixArtifactFilter, NixArtifactFilterContext},
    },
    session::init::StoreSnapshotLoader,
    summary::write_session_capture_summary,
    worker,
};
use chrono::Utc;
use nixcache_core::{BuildReceipt, BuildStats, IndexEntry, StoreHash, SystemArch};
use nixcache_oci::{SessionMutationRequest, UploadConfig};
use nixcache_oci_backend::create_tokio_reqwest_client;
use std::{collections::HashMap, env, path::Path, time::Duration};
use tokio::fs;
use tracing::{error, info};

#[derive(Debug, Clone)]
pub struct SessionCaptureOptions<'a> {
    pub repo: &'a str,
    pub registry: &'a str,
    pub run_id: u64,
    pub job_id: &'a str,
    pub system_opt: Option<&'a str>,
    pub signing_key_file: Option<&'a str>,
    pub github_token: &'a str,
    pub output_receipt_path: Option<&'a Path>,
    pub proxy_url: Option<&'a str>,
    pub snapshot_before: Option<&'a Path>,
    pub export_concurrency: usize,
    pub explicit_paths: &'a [String],
    pub out_link_pattern: Option<&'a str>,
    pub targets_expr: Option<&'a str>,
    pub capture_mode: CaptureMode,
    pub strict_closure: bool,
    pub workspace_root: &'a Path,
}

/// Session Capture: 并发目标解析 -> 内存快照差集闭包计算 -> 先验过滤 -> 内存签名注入 -> 无盘流式上传 -> CAS 提交
pub async fn run_session_capture(opts: &SessionCaptureOptions<'_>) -> Result<(), BuilderError> {
    let system = match opts.system_opt {
        Some(s) if !s.trim().is_empty() => SystemArch::from(s.trim()),
        _ => {
            let detected = SystemArch::detect_current();
            if detected.is_known() {
                detected
            } else {
                SystemArch::from(nix::get_system().await?.as_str())
            }
        }
    };

    info!(
        "Capturing session for Job: {} | Run ID: {} | System: {} | Repo: {}/{} | Mode: {:?}",
        opts.job_id, opts.run_id, system, opts.registry, opts.repo, opts.capture_mode
    );

    // 1. 并发目标根路径解析 (显式路径 + targets 表达式 + out-link 软链接)
    let effective_out_link = opts.out_link_pattern.unwrap_or("./result*");
    let target_roots = TargetResolver::resolve_target_roots(
        opts.explicit_paths,
        opts.targets_expr,
        Some(effective_out_link),
        opts.workspace_root,
    )
    .await?;

    info!(
        "Resolved {} target root(s) to capture: {:?}",
        target_roots.len(),
        target_roots
    );

    // 2. 纯内存加载构建前快照 (耗时 < 5ms，零磁盘扫描)
    let snap_set = if let Some(snap_file) = opts.snapshot_before {
        Some(StoreSnapshotLoader::load_snapshot_set(snap_file).await?)
    } else {
        None
    };

    if let Some(ref set) = snap_set {
        info!("Loaded {} baseline snapshot paths into memory", set.len());
    }

    // 3. 单次强类型闭包计算与内存差集过滤 (仅在此调用 1 次 nix path-info --recursive --json)
    let closure_res = ClosureEngine::compute_candidate_closure(
        &target_roots,
        snap_set.as_ref(),
        opts.capture_mode,
        opts.strict_closure,
    )
    .await?;

    info!(
        "Closure calculation: {} candidate item(s) to evaluate, {} active GC root(s)",
        closure_res.items.len(),
        closure_res.active_gc_roots.len()
    );

    let oci = create_tokio_reqwest_client(opts.registry, opts.repo, opts.github_token, true);

    // 4. 并行获取远端已缓存 StoreHash 集合 (cache-index + 当前 run-id session)
    let own_cached_hashes = worker::fetch_remote_arch_hashes(&oci, &system).await;
    let mut all_known_hashes = own_cached_hashes;
    if let Ok(Some((delta_data, _))) = oci
        .get_delta_patch_manifest(&format!("run-{}-{}", opts.run_id, system.as_str()))
        .await
    {
        all_known_hashes.extend(delta_data.new_entries.into_keys());
    }

    // 5. 先验过滤分类 (直接消费 closure_res.items，零二次 path-info 查询)
    let own_pub_key = get_own_public_key(opts.signing_key_file).await;
    let filter_ctx = NixArtifactFilterContext {
        own_public_key: own_pub_key.as_deref(),
        remote_cached_hashes: &all_known_hashes,
        trusted_upstream_prefixes: &["cache.nixos.org-1".to_string()],
    };

    let mut decision_report =
        NixArtifactFilter::classify_and_filter(closure_res.items, &filter_ctx);

    info!(
        "Pre-filtering result: {} locally-built, {} substituted (upstream), {} already-cached, {} ignored",
        decision_report.to_export.len(),
        decision_report.substituted_count,
        decision_report.already_cached_count,
        decision_report.ignored_count
    );

    // 6. 内存级确定性签名注入
    if let Some(key_file) = opts.signing_key_file
        && !decision_report.to_export.is_empty()
    {
        info!(
            "Signing and injecting signatures for {} export items",
            decision_report.to_export.len()
        );
        ParallelExporter::batch_sign_and_inject_signatures(
            &mut decision_report.to_export,
            key_file,
        )
        .await?;
    }

    // 7. 自适应并发无盘流式上传
    let mut new_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    let mut uploaded_count = 0;
    let mut total_bytes_uploaded = 0u64;

    if !decision_report.to_export.is_empty() {
        info!(
            "Capturing {} locally-built path(s) via ParallelExporter (Diskless, concurrency: {})",
            decision_report.to_export.len(),
            opts.export_concurrency
        );

        let export_config = ParallelExportConfig {
            concurrency: opts.export_concurrency,
            signing_key_file: None, // 前序已在内存中完成签名注入
            strict: false,
            upload_config: UploadConfig::default(),
            system,
            origin_job: Some(format!("job:{}", opts.job_id)),
        };

        let report = ParallelExporter::export_and_upload_paths_with_preinfo(
            &decision_report.to_export,
            &oci,
            &export_config,
        )
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
        }
    }

    let head_sha = env::var("GITHUB_SHA").ok();
    let ref_name = env::var("GITHUB_REF_NAME").ok();

    // 8. CAS 原子提交 Session Manifest (严格仅登记 closure_res.active_gc_roots)
    if !new_entries.is_empty() || !closure_res.active_gc_roots.is_empty() {
        let request = SessionMutationRequest::new(opts.run_id, opts.job_id, system)
            .with_entries(new_entries.clone())
            .with_roots(closure_res.active_gc_roots.clone())
            .with_git_info(head_sha, ref_name)
            .with_public_key(own_pub_key.clone())
            .with_upload_stats(uploaded_count, total_bytes_uploaded)
            .with_max_retries(5);

        oci.update_arch_session_with_cas(request).await?;
    }

    // 9. 代理热注册与 BuildReceipt 写入
    if let Some(purl) = opts.proxy_url
        && !new_entries.is_empty()
    {
        let register_endpoint = format!("{}/_session/register", purl.trim_end_matches('/'));
        if let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_millis(300))
            .connect_timeout(Duration::from_millis(100))
            .build()
        {
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
    }

    let candidate_count = decision_report.to_export.len()
        + decision_report.substituted_count
        + decision_report.already_cached_count
        + decision_report.ignored_count;

    if let Some(receipt_path) = opts.output_receipt_path {
        let stats = BuildStats {
            discovered_outputs: candidate_count,
            built_paths: decision_report.to_export.len(),
            uploaded_blobs: uploaded_count,
            total_bytes_uploaded,
            substituted_paths: decision_report.substituted_count,
        };

        let receipt = BuildReceipt::new(
            system,
            opts.repo.to_string(),
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            own_pub_key,
            new_entries,
            closure_res.active_gc_roots,
            stats,
        )
        .with_run_info(Some(opts.run_id), Some(opts.job_id.to_string()));

        if let Some(parent) = receipt_path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = fs::create_dir_all(parent).await;
        }

        let receipt_json = serde_json::to_string(&receipt)?;
        fs::write(receipt_path, receipt_json).await?;
        info!("Build receipt written to {:?}", receipt_path);
    }

    write_session_capture_summary(
        opts.job_id,
        system.as_str(),
        candidate_count,
        uploaded_count,
        total_bytes_uploaded,
    )
    .await;

    Ok(())
}
