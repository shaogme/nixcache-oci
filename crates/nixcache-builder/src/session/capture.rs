use crate::{
    error::BuilderError,
    nix::{self, driver::get_own_public_key},
    summary::write_session_capture_summary,
};
use chrono::Utc;
use nixcache_core::{
    BuildReceipt, BuildStats, IndexEntry, NarDigest, NarInfo, StoreHash, SystemArch,
};
use nixcache_oci::SessionMutationRequest;
use nixcache_oci_backend::{OciClientExt, create_tokio_reqwest_client};
use std::{
    collections::{HashMap, HashSet},
    env,
    path::Path,
};
use tempfile::tempdir;
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
    pub explicit_paths: &'a [String],
}

/// Session Capture: 捕获本 Job 新构建产物，导出并上传 NAR Blobs，CAS 更新 run-<run_id>，热注册到 Proxy
pub async fn run_session_capture(opts: &SessionCaptureOptions<'_>) -> Result<(), BuilderError> {
    let system = match opts.system_opt {
        Some(s) if !s.trim().is_empty() => SystemArch::from(s.trim()),
        _ => SystemArch::from(nix::get_system().await?.as_str()),
    };

    info!(
        "Capturing session for Job: {} | Run ID: {} | System: {} | Repo: {}/{}",
        opts.job_id, opts.run_id, system, opts.registry, opts.repo
    );

    let candidate_paths: Vec<String> = if !opts.explicit_paths.is_empty() {
        opts.explicit_paths.to_vec()
    } else if let Some(snap_file) = opts.snapshot_before
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

    let oci = create_tokio_reqwest_client(opts.registry, opts.repo, opts.github_token, true);
    let temp_dir = tempdir()?;

    let mut new_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    let mut uploaded_count = 0;
    let mut total_bytes_uploaded = 0u64;

    if !candidate_paths.is_empty() {
        let exported =
            nix::export_paths_directly(&candidate_paths, opts.signing_key_file, temp_dir.path())
                .await?;
        for (hash, store_path) in exported {
            let narinfo_path = temp_dir.path().join(format!("{}.narinfo", hash));
            let nar_file_path = temp_dir.path().join("nar").join(format!("{}.nar.xz", hash));

            if !nar_file_path.exists() {
                continue;
            }

            let metadata = fs::metadata(&nar_file_path).await?;
            let size = metadata.len();

            info!("  Uploading NAR for {} ({} bytes)", hash, size);
            match oci.push_blob_file(&nar_file_path).await {
                Ok(nar_digest_str) => {
                    if let Ok(narinfo_content) = fs::read_to_string(&narinfo_path).await {
                        let name = Path::new(&store_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .and_then(|s| s.split_once('-'))
                            .map(|x| x.1.to_string())
                            .unwrap_or_else(|| hash.clone());

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
                                    system: Some(system),
                                    narinfo_meta,
                                    nar_digest,
                                    nar_size: size.max(nar_size),
                                    added: Utc::now()
                                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                                    origin_job: Some(format!("job:{}", opts.job_id)),
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

    let pub_key = get_own_public_key(opts.signing_key_file).await;
    let head_sha = env::var("GITHUB_SHA").ok();
    let ref_name = env::var("GITHUB_REF_NAME").ok();

    // 执行单架构无锁乐观并发 CAS 更新写入 run-<run_id>-<system>
    if !new_entries.is_empty() || !active_gc_roots.is_empty() {
        let request = SessionMutationRequest::new(opts.run_id, opts.job_id, system)
            .with_entries(new_entries.clone())
            .with_roots(active_gc_roots.clone())
            .with_git_info(head_sha, ref_name)
            .with_public_key(pub_key.clone())
            .with_upload_stats(uploaded_count, total_bytes_uploaded)
            .with_max_retries(5);

        oci.update_arch_session_with_cas(request).await?;
    }

    // 热注册到本机 Proxy
    if let Some(purl) = opts.proxy_url
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

    // 写入 Schema v4 的 BuildReceipt
    if let Some(receipt_path) = opts.output_receipt_path {
        let stats = BuildStats {
            discovered_outputs: candidate_paths.len(),
            built_paths: candidate_paths.len(),
            uploaded_blobs: uploaded_count,
            total_bytes_uploaded,
            substituted_paths: 0,
        };

        let receipt = BuildReceipt::new(
            system,
            opts.repo.to_string(),
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            pub_key,
            new_entries,
            active_gc_roots,
            stats,
        )
        .with_run_info(Some(opts.run_id), Some(opts.job_id.to_string()));

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
        opts.job_id,
        system.as_str(),
        candidate_paths.len(),
        uploaded_count,
        total_bytes_uploaded,
    )
    .await;
    Ok(())
}
