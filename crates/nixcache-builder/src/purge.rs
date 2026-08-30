use crate::{
    error::BuilderError,
    nix::{BuildConfig, BuildMode, BuildTarget, discovery::discover_outputs},
    summary::write_purge_step_summary,
};
use chrono::Utc;
use nixcache_cli::PurgeFilterArgs;
use nixcache_core::{
    CACHE_INDEX_VERSION, CacheIndexData, StoreHash, evaluate_cache_purge, extract_store_hash,
};
use nixcache_oci::{
    EMPTY_CONFIG_DIGEST, EMPTY_CONFIG_SIZE, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciDescriptor,
    OciPlatform, build_arch_index_manifest, build_image_index,
};
use nixcache_oci_backend::create_tokio_reqwest_client;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::process::Command;
use tracing::{info, warn};

fn compute_sha256_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    format!(
        "sha256:{}",
        hash.iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    )
}

async fn resolve_flake_output_hashes(
    flake_path: &str,
    attributes: &[String],
) -> Result<Vec<StoreHash>, BuilderError> {
    let mut hashes = Vec::new();
    let abs_flake_path = match tokio::fs::canonicalize(flake_path).await {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(e) => {
            warn!("Failed to canonicalize flake path {}: {}", flake_path, e);
            flake_path.to_string()
        }
    };

    if !attributes.is_empty() {
        for attr in attributes {
            let target = format!("path:{}#{}", abs_flake_path, attr);
            let output = Command::new("nix")
                .args(["path-info", "--accept-flake-config", "--json", &target])
                .output()
                .await;

            if let Ok(out) = output
                && out.status.success()
            {
                let stdout_str = String::from_utf8_lossy(&out.stdout);
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&stdout_str) {
                    if let Some(obj) = val.as_object() {
                        for k in obj.keys() {
                            if let Some(h) = extract_store_hash(k) {
                                hashes.push(h);
                            }
                        }
                    } else if let Some(arr) = val.as_array() {
                        for item in arr {
                            if let Some(path_str) = item.get("path").and_then(|p| p.as_str())
                                && let Some(h) = extract_store_hash(path_str)
                            {
                                hashes.push(h);
                            }
                        }
                    }
                }
            } else {
                // Fallback: try nix eval --raw <target>.outPath
                let eval_out = Command::new("nix")
                    .args([
                        "eval",
                        "--accept-flake-config",
                        "--raw",
                        &format!("{}.outPath", target),
                    ])
                    .output()
                    .await;

                if let Ok(e_out) = eval_out
                    && e_out.status.success()
                {
                    let p = String::from_utf8_lossy(&e_out.stdout).trim().to_string();
                    if let Some(h) = extract_store_hash(&p) {
                        hashes.push(h);
                    }
                }
            }
        }
    } else {
        // 发现当前 Flake 的所有输出
        let build_config = BuildConfig {
            system: None,
            mode: BuildMode::Flake,
            flake_path: abs_flake_path.clone(),
            file: "default.nix".to_string(),
            attributes: Vec::new(),
        };
        if let Ok(targets) = discover_outputs(&build_config).await {
            for target in targets {
                if let BuildTarget::Flake {
                    flake_ref,
                    attribute,
                } = target
                {
                    let full_target = format!("{}#{}", flake_ref, attribute);
                    let eval_out = Command::new("nix")
                        .args([
                            "eval",
                            "--accept-flake-config",
                            "--raw",
                            &format!("{}.outPath", full_target),
                        ])
                        .output()
                        .await;

                    if let Ok(e_out) = eval_out
                        && e_out.status.success()
                    {
                        let p = String::from_utf8_lossy(&e_out.stdout).trim().to_string();
                        if let Some(h) = extract_store_hash(&p) {
                            hashes.push(h);
                        }
                    }
                }
            }
        }
    }

    Ok(hashes)
}

/// 执行缓存主动清理与失效工作流
pub async fn run_purge(
    args: &PurgeFilterArgs,
    repo: &str,
    registry: &str,
    github_token: &str,
) -> Result<(), BuilderError> {
    let dry_run = args.resolve_dry_run();
    let delete_blobs = args.resolve_delete_blobs();

    info!(
        "Starting cache purge workflow for {}/{} (dry_run: {}, delete_blobs: {})",
        registry, repo, dry_run, delete_blobs
    );

    let oci = create_tokio_reqwest_client(registry, repo, github_token, true);

    let (index_data, _) = match oci.get_cache_index("cache-index").await? {
        Some(pair) => pair,
        None => {
            info!("No cache index found, nothing to purge.");
            return Ok(());
        }
    };

    if index_data.entries.is_empty() {
        info!("Cache index is empty, nothing to purge.");
        return Ok(());
    }

    // 若指定了 flake-path 或 attributes，解析 Flake 输出闭包对应的 StoreHash
    let mut extra_hashes = Vec::new();
    if let Some(ref flake_path) = args.resolve_flake_path() {
        let attrs = args.resolve_attributes();
        info!("Evaluating flake outputs for purging from: {}", flake_path);
        let flake_hashes = resolve_flake_output_hashes(flake_path, &attrs).await?;
        info!(
            "Resolved {} store hashes from flake outputs",
            flake_hashes.len()
        );
        extra_hashes.extend(flake_hashes);
    }

    let filter = args.to_purge_filter(&extra_hashes);
    let purge_result = evaluate_cache_purge(&index_data, &filter);

    info!(
        "Purge Evaluation: Total Before: {}, Purged: {}, Kept: {}, Estimated Space Freed: {} bytes",
        index_data.entries.len(),
        purge_result.purged_entries.len(),
        purge_result.kept_entries.len(),
        purge_result.estimated_freed_bytes
    );

    if purge_result.purged_entries.is_empty() {
        info!("No entries matched purge filter. Cache index remains untouched.");
        write_purge_step_summary(dry_run, 0, index_data.entries.len(), 0, 0).await;
        return Ok(());
    }

    if dry_run {
        info!(
            "Dry run mode active. Outputting preview report and exiting without remote modifications."
        );
        for (hash, reason) in &purge_result.reason_map {
            info!("  [Preview Purge] {} -> Reason: {}", hash, reason);
        }
        write_purge_step_summary(
            true,
            purge_result.purged_entries.len(),
            purge_result.kept_entries.len(),
            purge_result.estimated_freed_bytes,
            0,
        )
        .await;
        return Ok(());
    }

    // 将保留的 entries 和 roots 按架构拆分写回
    let kept_data = CacheIndexData {
        version: CACHE_INDEX_VERSION,
        repo: repo.to_string(),
        registry: registry.to_string(),
        image: format!("{}/{}/nix-cache", registry, repo),
        generated: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        public_key: index_data.public_key.clone(),
        entries: purge_result.kept_entries.clone(),
        gc_roots: purge_result.updated_gc_roots,
        last_promoted_run: index_data.last_promoted_run,
    };

    let partitioned = kept_data.into_arch_partitioned();
    let config_digest = EMPTY_CONFIG_DIGEST;
    let config_size = EMPTY_CONFIG_SIZE;

    let mut manifest_descriptors: Vec<OciDescriptor> = Vec::new();

    for (sys, arch_data) in partitioned {
        let (blob_digest, compressed_size, uncompressed_size) =
            oci.push_zstd_blob(&arch_data).await?;
        let sub_manifest = build_arch_index_manifest(
            &blob_digest,
            compressed_size,
            uncompressed_size,
            config_digest,
            config_size,
            &sys,
        );
        let sub_manifest_json = sub_manifest.to_json_string()?;
        let sub_manifest_digest = compute_sha256_digest(sub_manifest_json.as_bytes());

        let arch_tag = format!("cache-index-{}", sys.as_str());
        oci.push_manifest(&arch_tag, &sub_manifest_json).await?;

        let mut desc_annotations = HashMap::new();
        desc_annotations.insert("org.nixos.nixcache.system".to_string(), sys.to_string());

        manifest_descriptors.push(OciDescriptor {
            media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
            digest: sub_manifest_digest,
            size: sub_manifest_json.len() as u64,
            platform: Some(OciPlatform::from_system(&sys)),
            annotations: Some(desc_annotations),
        });
    }

    let final_descriptors = manifest_descriptors;
    oci.update_image_index_cas("cache-index", 5, |_existing| {
        let mut index = build_image_index(
            final_descriptors.clone(),
            "NixCache Multi-Architecture Global Index",
        );
        index.schema_version = 2;
        Ok(index)
    })
    .await?;

    info!("Successfully updated multi-arch cache-index after purge.");

    let mut deleted_blobs = 0;
    if delete_blobs && !purge_result.purged_nar_digests.is_empty() {
        info!(
            "Attempting physical deletion of {} OCI NAR blobs...",
            purge_result.purged_nar_digests.len()
        );
        let (del, skipped) = oci
            .batch_delete_blobs(&purge_result.purged_nar_digests, 8)
            .await?;
        info!(
            "Blob deletion complete: {} physically deleted, {} skipped / unsupported.",
            del, skipped
        );
        deleted_blobs = del;
    }

    write_purge_step_summary(
        false,
        purge_result.purged_entries.len(),
        purge_result.kept_entries.len(),
        purge_result.estimated_freed_bytes,
        deleted_blobs,
    )
    .await;

    info!("Cache purge workflow finished successfully.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixcache_core::{IndexEntry, NarDigest, NarInfoMeta, SystemArch};

    #[test]
    fn test_purge_sha256_digest() {
        let digest = compute_sha256_digest(b"hello world");
        assert_eq!(
            digest,
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_purge_filter_integration_with_builder() {
        let hash1 = StoreHash::new_unchecked("0000000000000000000000000000pkg1");
        let hash2 = StoreHash::new_unchecked("0000000000000000000000000000pkg2");

        let mut index = CacheIndexData::default();
        index.entries.insert(
            hash1.clone(),
            IndexEntry {
                name: "pkg-x86".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-pkg-x86", hash1),
                    nar_basename: "pkg-x86.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob1"),
                nar_size: 1024,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );
        index.entries.insert(
            hash2.clone(),
            IndexEntry {
                name: "pkg-arm".to_string(),
                system: Some(SystemArch::Aarch64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-pkg-arm", hash2),
                    nar_basename: "pkg-arm.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob2"),
                nar_size: 2048,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        let args = PurgeFilterArgs {
            system: vec!["x86_64-linux".to_string()],
            ..Default::default()
        };

        let filter = args.to_purge_filter(&[]);
        let result = evaluate_cache_purge(&index, &filter);
        assert_eq!(result.purged_hashes, vec![hash1]);
        assert_eq!(result.kept_entries.len(), 1);
        assert!(result.kept_entries.contains_key(&hash2));
    }
}
