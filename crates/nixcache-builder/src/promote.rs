use crate::{error::BuilderError, summary::write_promote_step_summary};
use chrono::Utc;
use futures_util::future::{join_all, try_join_all};
use nixcache_core::{
    BuildReceipt, FastBlockedBloomFilter, IndexEntry, NUM_SHARDS, SCHEMA_VERSION_V5,
    ShardDataPayload, ShardDescriptor, ShardedArchCacheIndexData, StoreHash, SystemArch,
    partition_entries_by_shard,
};
use nixcache_oci::{
    OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest, OciDescriptor, OciPlatform,
    build_image_index,
};
use nixcache_oci_backend::create_tokio_reqwest_client;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use tokio::fs;
use tracing::{info, warn};

async fn collect_receipt_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs_to_visit = Vec::new();

    for p in paths {
        if p.is_dir() {
            dirs_to_visit.push(p.clone());
        } else if p.is_file() && p.extension().and_then(|ext| ext.to_str()) == Some("json") {
            files.push(p.clone());
        }
    }

    while let Some(current_dir) = dirs_to_visit.pop() {
        if let Ok(mut dir_entries) = fs::read_dir(&current_dir).await {
            while let Ok(Some(entry)) = dir_entries.next_entry().await {
                let file_path = entry.path();
                if let Ok(file_type) = entry.file_type().await {
                    if file_type.is_dir() {
                        dirs_to_visit.push(file_path);
                    } else if file_type.is_file()
                        && file_path.extension().and_then(|ext| ext.to_str()) == Some("json")
                    {
                        files.push(file_path);
                    }
                }
            }
        }
    }

    files.sort();
    files.dedup();
    files
}

/// Promote: 汇聚多架构会话清单与 Receipts，分架构局部压实 (Partial Shard Compaction)，并原子发布顶层 OCI Image Index
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
        "Promoting multi-arch cache to tag '{}' for repo: {}/{} (Run ID: {:?})",
        target_tag, registry, repo, run_id
    );

    let oci = create_tokio_reqwest_client(registry, repo, github_token, true);

    // 1. 准备待合并的数据集 (按系统架构分桶)
    let mut incoming_entries_by_sys: HashMap<SystemArch, HashMap<StoreHash, IndexEntry>> =
        HashMap::new();
    let mut incoming_roots_by_sys: HashMap<SystemArch, Vec<StoreHash>> = HashMap::new();
    let mut base_pub_key = String::new();
    let mut session_found = false;

    // 1.1 远端 Session Delta Manifests 收集
    if let Some(rid) = run_id {
        for sys in SystemArch::all() {
            let arch_tag = format!("run-{}-{}", rid, sys.as_str());
            if let Ok(Some((delta, _))) = oci.get_delta_patch_manifest(&arch_tag).await {
                info!(
                    "Found remote DeltaPatchData for tag {} with {} new entries",
                    arch_tag,
                    delta.new_entries.len()
                );
                session_found = true;
                incoming_entries_by_sys
                    .entry(sys)
                    .or_default()
                    .extend(delta.new_entries);
                incoming_roots_by_sys
                    .entry(sys)
                    .or_default()
                    .extend(delta.active_gc_roots);
            }
        }

        let main_tag = format!("run-{}", rid);
        if let Ok(Some((main_delta, _))) = oci.get_delta_patch_manifest(&main_tag).await {
            info!(
                "Found remote DeltaPatchData for tag {} with {} entries",
                main_tag,
                main_delta.new_entries.len()
            );
            session_found = true;
            for (hash, entry) in main_delta.new_entries {
                let sys = entry.system.unwrap_or(main_delta.system);
                incoming_entries_by_sys
                    .entry(sys)
                    .or_default()
                    .insert(hash, entry);
            }
            incoming_roots_by_sys
                .entry(main_delta.system)
                .or_default()
                .extend(main_delta.active_gc_roots);
        }
    }

    // 1.2 本地 Receipt 文件/目录加载（支持单文件、目录及多级子目录递归扫描）
    let receipt_files = collect_receipt_files(receipt_paths).await;
    for file_path in receipt_files {
        match fs::read_to_string(&file_path).await {
            Ok(content) => match serde_json::from_str::<BuildReceipt>(&content) {
                Ok(receipt) => {
                    info!(
                        "Loaded receipt from {:?} (system: {}, entries: {})",
                        file_path,
                        receipt.system,
                        receipt.new_entries.len()
                    );
                    if base_pub_key.is_empty()
                        && let Some(ref pk) = receipt.public_key
                        && !pk.is_empty()
                    {
                        base_pub_key = pk.clone();
                    }
                    incoming_entries_by_sys
                        .entry(receipt.system)
                        .or_default()
                        .extend(receipt.new_entries);
                    incoming_roots_by_sys
                        .entry(receipt.system)
                        .or_default()
                        .extend(receipt.active_gc_roots);
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

    let total_promoted_entries: usize = incoming_entries_by_sys.values().map(|e| e.len()).sum();

    if !session_found && receipt_paths.is_empty() && total_promoted_entries == 0 {
        info!("No session manifest or receipts found to promote. Merging with existing baseline.");
    }

    // 2. 探查现存 Baseline 数据涉及的所有架构
    let mut target_systems: HashSet<SystemArch> = incoming_entries_by_sys.keys().copied().collect();
    target_systems.extend(incoming_roots_by_sys.keys().copied());

    if let Ok(Some(artifact)) = oci.fetch_artifact(target_tag).await {
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
        let detected = SystemArch::detect_current();
        if detected.is_known() {
            target_systems.insert(detected);
        } else {
            target_systems.insert(SystemArch::X86_64Linux);
        }
    }

    let mut target_systems_vec: Vec<SystemArch> = target_systems.into_iter().collect();
    target_systems_vec.sort();

    // 3. 为每个系统架构并发执行分片局部压实 (Partial Compaction) 并推送 Sub-Manifest
    let push_futures = target_systems_vec.into_iter().map(|sys| {
        let oci = oci.clone();
        let new_entries = incoming_entries_by_sys.remove(&sys).unwrap_or_default();
        let new_roots = incoming_roots_by_sys.remove(&sys).unwrap_or_default();
        let repo = repo.to_string();
        let registry = registry.to_string();
        let base_pub_key = base_pub_key.clone();
        let target_tag = target_tag.to_string();

        async move {
            // 3.1 获取现存该架构的 Root Index 或新建空白结构
            let (mut root_index, _prev_digest) =
                match oci.get_sharded_root_index(&target_tag, &sys).await? {
                    Some((data, digest)) => (data, Some(digest)),
                    None => (ShardedArchCacheIndexData::new(sys, &repo, &registry), None),
                };

            if root_index.shards.len() != NUM_SHARDS {
                root_index.shards = (0..NUM_SHARDS as u16).map(ShardDescriptor::empty).collect();
            }

            if !base_pub_key.is_empty() && root_index.public_key.is_empty() {
                root_index.public_key = base_pub_key.clone();
            }

            // 3.2 将新增条目按 1024 分片桶分区
            let mut partitioned_incoming = partition_entries_by_shard(new_entries);

            // 3.3 遍历 1024 个分片执行局部压实
            for shard_id in 0..NUM_SHARDS as u16 {
                if let Some(incoming_shard_entries) = partitioned_incoming.remove(&shard_id)
                    && !incoming_shard_entries.is_empty()
                {
                    let existing_desc = &root_index.shards[shard_id as usize];
                    let mut shard_payload =
                        if existing_desc.entry_count > 0 && !existing_desc.blob_digest.is_empty() {
                            match oci.get_shard_data(&existing_desc.blob_digest).await {
                                Ok(payload) => payload,
                                Err(_) => ShardDataPayload::new(shard_id),
                            }
                        } else {
                            ShardDataPayload::new(shard_id)
                        };

                    shard_payload.entries.extend(incoming_shard_entries);

                    let (new_blob_digest, comp_size, uncomp_size) =
                        oci.push_shard_data(&shard_payload).await?;

                    let desc = &mut root_index.shards[shard_id as usize];
                    desc.blob_digest = new_blob_digest;
                    desc.compressed_size = comp_size;
                    desc.uncompressed_size = uncomp_size;
                    desc.entry_count = shard_payload.len();
                    desc.merkle_hash = shard_payload.compute_merkle_hash();
                }
                // 未发生变更的分片：完全复用原有 blob_digest、compressed_size 与 merkle_hash (零开销)
            }

            // 3.4 合并 GC Roots 并重算全局 Merkle Root
            root_index.gc_roots.extend(new_roots);
            root_index.gc_roots.sort_unstable();
            root_index.gc_roots.dedup();
            root_index.recalculate_merkle_root();

            // 3.5 构建/更新紧凑布隆过滤器
            let total_entries = root_index.total_entries();
            let mut bloom_filter =
                FastBlockedBloomFilter::new_with_defaults(total_entries.max(100));

            // 从各个非空分片并发收集全量 hashes 以生成精准 Bloom Filter
            let non_empty_shards: Vec<_> = root_index
                .shards
                .iter()
                .filter(|s| s.entry_count > 0 && !s.blob_digest.is_empty())
                .map(|s| s.blob_digest.clone())
                .collect();

            let shard_futures = non_empty_shards.into_iter().map(|digest| {
                let oci = oci.clone();
                async move { oci.get_shard_data(&digest).await.ok() }
            });
            let payloads = join_all(shard_futures).await;
            for payload in payloads.into_iter().flatten() {
                for hash in payload.entries.keys() {
                    bloom_filter.insert(hash);
                }
            }

            let bf_manifest = oci.push_bloom_filter(&bloom_filter).await?;
            root_index.bloom_filter = bf_manifest.clone();
            root_index.version = SCHEMA_VERSION_V5;
            root_index.generated = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            root_index.last_promoted_run = run_id;

            // 3.6 推送架构专属 Sub-Manifest (如 cache-index-x86_64-linux)
            let arch_tag = format!("{}-{}", target_tag, sys.as_str());
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
                "Pushed Sharded Sub-Manifest for {}: digest {} (tag: {})",
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
    });

    let manifest_descriptors: Vec<OciDescriptor> = try_join_all(push_futures).await?;

    // 4. 组装并原子发布顶层 OCI Image Index (cache-index)
    let final_descriptors = manifest_descriptors;
    oci.update_image_index_cas(target_tag, 5, |_existing| {
        let mut index = build_image_index(
            final_descriptors.clone(),
            "NixCache Multi-Architecture Global Index",
        );
        index.schema_version = 2;
        Ok(index)
    })
    .await?;

    info!(
        "Promote complete! Multi-Architecture OCI Image Index published for tag '{}'.",
        target_tag
    );

    // 5. 清理会话标签 (包括全局会话与各架构专属会话)
    if cleanup_session && let Some(rid) = run_id {
        let main_tag = format!("run-{}", rid);
        let mut delete_tags = vec![main_tag];

        for sys in SystemArch::all() {
            delete_tags.push(format!("run-{}-{}", rid, sys.as_str()));
        }

        let delete_futures = delete_tags.into_iter().map(|tag| {
            let oci = oci.clone();
            async move { oci.delete_tag_strict(&tag).await }
        });
        try_join_all(delete_futures).await?;
        info!("Cleaned up session tags for run-{}", rid);
    }

    write_promote_step_summary(
        run_id,
        target_tag,
        total_promoted_entries,
        total_promoted_entries,
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixcache_core::{BuildReceipt, BuildStats, StoreHash, SystemArch};
    use std::collections::{HashMap, HashSet};
    use tempfile::tempdir;
    use tokio::fs;

    #[test]
    fn test_promote_gc_roots_in_place_merge() {
        let root1 = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let root2 = StoreHash::parse("00000000000000000000000000000002").unwrap();
        let root3 = StoreHash::parse("00000000000000000000000000000003").unwrap();

        let mut roots = vec![root3.clone(), root1.clone()];
        let incoming_roots = vec![root2.clone(), root1.clone()];
        roots.extend(incoming_roots);
        roots.sort_unstable();
        roots.dedup();

        assert_eq!(roots, vec![root1, root2, root3]);
    }

    #[tokio::test]
    async fn test_receipt_dir_parsing_in_promote() {
        let temp_dir = tempdir().unwrap();
        let receipts_dir = temp_dir.path().join("receipts");
        fs::create_dir_all(&receipts_dir).await.unwrap();

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

        fs::write(receipts_dir.join("receipt-x86.json"), r1_json)
            .await
            .unwrap();
        fs::write(receipts_dir.join("receipt-arm.json"), r2_json)
            .await
            .unwrap();
        fs::write(receipts_dir.join("README.txt"), "some notes")
            .await
            .unwrap();

        let mut loaded = Vec::new();
        let mut dir_entries = fs::read_dir(&receipts_dir).await.unwrap();
        while let Ok(Some(entry)) = dir_entries.next_entry().await {
            let p = entry.path();
            if p.is_file() && p.extension().and_then(|ext| ext.to_str()) == Some("json") {
                let content = fs::read_to_string(&p).await.unwrap();
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
    async fn test_collect_receipt_files_recursive() {
        let temp_dir = tempdir().unwrap();
        let base_dir = temp_dir.path().join("downloaded-receipts");
        let sub_dir1 = base_dir.join("nixcache-receipt-x86_64-linux");
        let sub_dir2 = base_dir.join("nixcache-receipt-aarch64-linux");
        fs::create_dir_all(&sub_dir1).await.unwrap();
        fs::create_dir_all(&sub_dir2).await.unwrap();

        let file1 = sub_dir1.join("receipt.json");
        let file2 = sub_dir2.join("receipt.json");
        let non_json = sub_dir1.join("log.txt");

        fs::write(&file1, "{}").await.unwrap();
        fs::write(&file2, "{}").await.unwrap();
        fs::write(&non_json, "log").await.unwrap();

        let files = collect_receipt_files(&[base_dir]).await;
        assert_eq!(files.len(), 2);
        assert!(files.contains(&file1));
        assert!(files.contains(&file2));
    }

    #[test]
    fn test_partial_shard_compaction_logic() {
        let h1 = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let h2 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();

        let mut root =
            ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        assert_eq!(root.shards.len(), NUM_SHARDS);
        let initial_root_hash = root.merkle_root.clone();

        // 模拟 shard 0 写入条目
        let sid1 = h1.shard_id() as usize;
        root.shards[sid1].entry_count = 1;
        root.shards[sid1].blob_digest = "sha256:blob_shard_0".to_string();
        root.shards[sid1].merkle_hash = "sha256:merkle_shard_0".to_string();
        root.recalculate_merkle_root();

        let intermediate_root_hash = root.merkle_root.clone();
        assert_ne!(initial_root_hash, intermediate_root_hash);

        // 模拟 shard s6 写入条目，shard 0 保持不变（未被修改）
        let sid2 = h2.shard_id() as usize;
        let shard0_desc_before = root.shards[sid1].clone();

        root.shards[sid2].entry_count = 1;
        root.shards[sid2].blob_digest = "sha256:blob_shard_s6".to_string();
        root.shards[sid2].merkle_hash = "sha256:merkle_shard_s6".to_string();
        root.recalculate_merkle_root();

        // shard 0 完全保持一致（零开销复用）
        assert_eq!(root.shards[sid1], shard0_desc_before);
        assert_ne!(intermediate_root_hash, root.merkle_root);
    }
}
