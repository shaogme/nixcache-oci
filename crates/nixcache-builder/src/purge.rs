use crate::{
    error::BuilderError, nix::resolve_flake_output_hashes, summary::write_purge_step_summary,
};
use chrono::Utc;
use futures_util::future::try_join_all;
use nixcache_cli::PurgeArgs;
use nixcache_core::{
    FastBlockedBloomFilter, IndexEntry, NUM_SHARDS, SCHEMA_VERSION_V5, ShardDataPayload,
    ShardDescriptor, ShardedArchCacheIndexData, StoreHash, SystemArch, evaluate_cache_purge,
    partition_entries_by_shard,
};
use nixcache_oci::{
    OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest, OciDescriptor, OciPlatform,
    build_image_index,
};
use nixcache_oci_backend::create_tokio_reqwest_client;
use std::collections::{HashMap, HashSet};
use tracing::info;

/// 执行缓存主动清理与失效工作流 (Schema v5 分片梅克尔基数索引)
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

    // 2. 探查多架构并加载现存基线索引数据
    let mut target_systems: HashSet<SystemArch> = HashSet::new();
    if let Ok(Some(artifact)) = oci.fetch_artifact("cache-index").await {
        match artifact.manifest {
            OciArtifactManifest::Index(index) => {
                for desc in index.manifests {
                    if let Some(ref plat) = desc.platform {
                        let sys = SystemArch::from_oci(
                            &plat.os,
                            &plat.architecture,
                            plat.variant.as_deref(),
                        );
                        if sys.is_known() {
                            target_systems.insert(sys);
                        }
                    }
                }
            }
            OciArtifactManifest::Manifest(_) => {
                let detected = SystemArch::detect_current();
                if detected.is_known() {
                    target_systems.insert(detected);
                }
            }
        }
    }

    if target_systems.is_empty() {
        for sys in SystemArch::all() {
            target_systems.insert(sys);
        }
    }

    let mut arch_roots_data: HashMap<
        SystemArch,
        (ShardedArchCacheIndexData, HashMap<StoreHash, IndexEntry>),
    > = HashMap::new();
    let mut all_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    let mut all_gc_roots: HashMap<SystemArch, Vec<StoreHash>> = HashMap::new();

    let root_futures = target_systems.into_iter().map(|sys| {
        let oci = oci.clone();
        async move {
            if let Some((root_data, _)) = oci.get_sharded_root_index("cache-index", &sys).await? {
                let non_empty_shards: Vec<_> = root_data
                    .shards
                    .iter()
                    .filter(|s| s.entry_count > 0 && !s.blob_digest.is_empty())
                    .map(|s| s.blob_digest.clone())
                    .collect();

                let shard_futures = non_empty_shards.into_iter().map(|digest| {
                    let oci = oci.clone();
                    async move { oci.get_shard_data(&digest).await }
                });
                let payloads = try_join_all(shard_futures).await?;
                let mut entries = HashMap::new();
                for p in payloads {
                    entries.extend(p.entries);
                }
                Ok::<_, BuilderError>(Some((sys, root_data, entries)))
            } else {
                Ok(None)
            }
        }
    });

    let loaded_archs = try_join_all(root_futures).await?;
    for (sys, root_data, entries) in loaded_archs.into_iter().flatten() {
        all_gc_roots.insert(sys, root_data.gc_roots.clone());
        all_entries.extend(entries.clone());
        arch_roots_data.insert(sys, (root_data, entries));
    }

    if all_entries.is_empty() {
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
    let purge_result = evaluate_cache_purge(&all_entries, &all_gc_roots, &selector);

    info!(
        "Purge Evaluation: Total Before: {}, Purged: {}, Kept: {}, Estimated Space Freed: {} bytes",
        all_entries.len(),
        purge_result.purged_entries.len(),
        purge_result.kept_entries.len(),
        purge_result.estimated_freed_bytes
    );

    if purge_result.purged_entries.is_empty() {
        info!("No entries matched purge filter. Cache index remains untouched.");
        write_purge_step_summary(dry_run, 0, all_entries.len(), 0, 0).await;
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

    // 3. 按架构对发生变化的分片执行局部压实与推送
    let purged_hashes_set: HashSet<StoreHash> =
        purge_result.purged_hashes.iter().cloned().collect();
    let push_futures = arch_roots_data.into_iter().map(
        |(sys, (mut root_index, original_entries))| {
            let oci = oci.clone();
            let purged_hashes_set = purged_hashes_set.clone();
            let updated_roots = purge_result
                .updated_gc_roots
                .get(&sys)
                .cloned()
                .unwrap_or_default();

            async move {
                let kept_entries_for_sys: HashMap<StoreHash, IndexEntry> = original_entries
                    .into_iter()
                    .filter(|(h, _)| !purged_hashes_set.contains(h))
                    .collect();

                let mut partitioned_kept = partition_entries_by_shard(kept_entries_for_sys.clone());

                for shard_id in 0..NUM_SHARDS as u16 {
                    let kept_for_shard = partitioned_kept.remove(&shard_id).unwrap_or_default();
                    let desc = &mut root_index.shards[shard_id as usize];

                    if kept_for_shard.is_empty() {
                        *desc = ShardDescriptor::empty(shard_id);
                    } else if desc.entry_count != kept_for_shard.len() {
                        // 该分片有部分条目被清除，重新序列化并推送
                        let mut payload = ShardDataPayload::new(shard_id);
                        payload.entries = kept_for_shard;
                        let (blob_digest, comp_size, uncomp_size) =
                            oci.push_shard_data(&payload).await?;

                        desc.blob_digest = blob_digest;
                        desc.compressed_size = comp_size;
                        desc.uncompressed_size = uncomp_size;
                        desc.entry_count = payload.len();
                        desc.merkle_hash = payload.compute_merkle_hash();
                    }
                    // 未发生变更的分片：完全复用原描述符
                }

                root_index.gc_roots = updated_roots;
                root_index.recalculate_merkle_root();

                // 重建 Bloom Filter
                let total_entries = root_index.total_entries();
                let mut bloom_filter =
                    FastBlockedBloomFilter::new_with_defaults(total_entries.max(100));
                for hash in kept_entries_for_sys.keys() {
                    bloom_filter.insert(hash);
                }

                let bf_manifest = oci.push_bloom_filter(&bloom_filter).await?;
                root_index.bloom_filter = bf_manifest.clone();
                root_index.version = SCHEMA_VERSION_V5;
                root_index.generated =
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

                let arch_tag = format!("cache-index-{}", sys.as_str());
                let sub_manifest_digest = oci
                    .push_sharded_root_index(
                        &arch_tag,
                        &root_index,
                        &bf_manifest.blob_digest,
                        bf_manifest.compressed_size,
                        None,
                    )
                    .await?;

                info!(
                    "Pushed updated Sharded Sub-Manifest for {} after purge: digest {} (tag: {})",
                    sys, sub_manifest_digest, arch_tag
                );

                let mut desc_annotations = HashMap::new();
                desc_annotations.insert("org.nixos.nixcache.system".to_string(), sys.to_string());
                desc_annotations.insert(
                    "org.nixos.nixcache.merkle_root".to_string(),
                    root_index.merkle_root.clone(),
                );

                let descriptor = OciDescriptor {
                    media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
                    digest: sub_manifest_digest,
                    size: 0,
                    platform: Some(OciPlatform::from_system(&sys)),
                    annotations: Some(desc_annotations),
                };

                Ok::<OciDescriptor, BuilderError>(descriptor)
            }
        },
    );

    let manifest_descriptors: Vec<OciDescriptor> = try_join_all(push_futures).await?;

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
                        operation: "delete_blob",
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
    fn test_purge_filter_integration_with_builder() {
        let hash1 = StoreHash::new_unchecked("0000000000000000000000000000pkg1");
        let hash2 = StoreHash::new_unchecked("0000000000000000000000000000pkg2");

        let mut entries = HashMap::new();
        entries.insert(
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
        entries.insert(
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

        let mut gc_roots = HashMap::new();
        gc_roots.insert(SystemArch::X86_64Linux, vec![hash1.clone()]);
        gc_roots.insert(SystemArch::Aarch64Linux, vec![hash2.clone()]);

        let args = PurgeArgs {
            selector: CacheSelectorArgs {
                system: vec!["x86_64-linux".to_string()],
                ..Default::default()
            },
            ..Default::default()
        };

        let selector = args.to_purge_filter(&[]);
        let result = evaluate_cache_purge(&entries, &gc_roots, &selector);
        assert_eq!(result.purged_hashes, vec![hash1]);
        assert_eq!(result.kept_entries.len(), 1);
        assert!(result.kept_entries.contains_key(&hash2));
    }
}
