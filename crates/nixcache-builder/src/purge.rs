use crate::{
    error::BuilderError, nix::resolve_flake_output_hashes, summary::write_purge_step_summary,
};
use chrono::Utc;
use nixcache_cli::PurgeArgs;
use nixcache_core::{CACHE_INDEX_VERSION, CacheIndexData, evaluate_cache_purge};
use nixcache_oci::{
    EMPTY_CONFIG_DIGEST, EMPTY_CONFIG_SIZE, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciDescriptor,
    OciPlatform, build_arch_index_manifest, build_image_index,
};
use nixcache_oci_backend::create_tokio_reqwest_client;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tracing::info;

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

/// 执行缓存主动清理与失效工作流
pub async fn run_purge(
    args: &PurgeArgs,
    repo: &str,
    registry: &str,
    github_token: &str,
) -> Result<(), BuilderError> {
    let dry_run = args.resolve_dry_run();
    let delete_blobs = args.resolve_delete_blobs();
    let strict_mode = args.resolve_strict();
    let is_all = args.selector.resolve_all();

    info!(
        "Starting cache purge workflow for {}/{} (dry_run: {}, delete_blobs: {}, strict: {}, is_all: {})",
        registry, repo, dry_run, delete_blobs, strict_mode, is_all
    );

    let oci = create_tokio_reqwest_client(registry, repo, github_token, true);

    // 1. 若指定了 --all 且后端支持原生包删除 (如 GHCR)，直接执行彻底重置/删除
    if is_all && oci.capabilities().supports_package_deletion {
        if dry_run {
            info!(
                "Dry run mode active: would delete entire remote package {}/{} on backend '{}'",
                registry,
                repo,
                oci.kind()
            );
            write_purge_step_summary(true, 0, 0, 0, 0).await;
            return Ok(());
        }

        info!(
            "Executing complete package deletion for {}/{} on backend '{}'...",
            registry,
            repo,
            oci.kind()
        );
        oci.delete_entire_package_strict().await?;
        info!(
            "Successfully deleted entire remote package {}/{}",
            registry, repo
        );
        write_purge_step_summary(false, 0, 0, 0, 0).await;
        return Ok(());
    }

    // 2. 拉取现存 cache-index
    let index_data_opt = oci.get_cache_index("cache-index").await?;
    let (index_data, _) = match index_data_opt {
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
    if let Some(ref flake_path) = args.selector.resolve_flake_path() {
        let attrs = args.selector.resolve_attributes();
        info!("Evaluating flake outputs for purging from: {}", flake_path);
        let flake_hashes = resolve_flake_output_hashes(flake_path, &attrs).await?;
        info!(
            "Resolved {} store hashes from flake outputs",
            flake_hashes.len()
        );
        extra_hashes.extend(flake_hashes);
    }

    let selector = args.to_purge_filter(&extra_hashes);
    let purge_result = evaluate_cache_purge(&index_data, &selector);

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

    // 3. 将保留的 entries 和 roots 按架构拆分写回
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

    // 4. 处理 Blobs 物理删除
    let mut deleted_blobs = 0;
    if delete_blobs && !purge_result.purged_nar_digests.is_empty() {
        if !oci.capabilities().supports_blob_physical_deletion {
            if strict_mode {
                return Err(BuilderError::Oci(
                    nixcache_oci::OciError::OperationNotSupported {
                        backend: oci.kind(),
                        reason: format!(
                            "Backend '{}' does not support standalone OCI blob deletion. Blobs are managed via package versions. To delete unused data on GHCR, use tag deletion or 'purge --all'.",
                            oci.kind()
                        ),
                    },
                ));
            } else {
                info!(
                    "Notice: backend '{}' does not support standalone blob deletion; skipping blob physical deletion stage.",
                    oci.kind()
                );
            }
        } else {
            info!(
                "Attempting physical deletion of {} OCI NAR blobs...",
                purge_result.purged_nar_digests.len()
            );
            let summary = oci
                .batch_delete_blobs_strict(&purge_result.purged_nar_digests, 8, strict_mode)
                .await?;
            info!(
                "Blob deletion complete: {} physically deleted, {} failed/skipped.",
                summary.deleted_count, summary.failed_count
            );
            deleted_blobs = summary.deleted_count;
        }
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
    use nixcache_cli::CacheSelectorArgs;
    use nixcache_core::{IndexEntry, NarDigest, NarInfoMeta, StoreHash, SystemArch};

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

        let args = PurgeArgs {
            selector: CacheSelectorArgs {
                system: vec!["x86_64-linux".to_string()],
                ..Default::default()
            },
            ..Default::default()
        };

        let selector = args.to_purge_filter(&[]);
        let result = evaluate_cache_purge(&index, &selector);
        assert_eq!(result.purged_hashes, vec![hash1]);
        assert_eq!(result.kept_entries.len(), 1);
        assert!(result.kept_entries.contains_key(&hash2));
    }
}
