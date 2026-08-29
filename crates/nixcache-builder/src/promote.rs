use crate::{error::BuilderError, summary::write_promote_step_summary};
use bytes::Bytes;
use chrono::Utc;
use nixcache_core::{
    ArchCacheIndexData, BuildReceipt, CACHE_INDEX_VERSION, IndexEntry, StoreHash, SystemArch,
};
use nixcache_oci::{
    IndexCodec, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest, OciDescriptor,
    OciImageManifest, OciPlatform, build_arch_index_manifest, build_image_index,
};
use nixcache_oci_backend::create_tokio_reqwest_client;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use tokio::fs;
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

/// Promote: 汇聚多架构会话清单与 Receipts，分架构生成 Sub-Manifest，并原子发布顶层 OCI Image Index
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
    let mut session_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    let mut session_roots: HashMap<SystemArch, Vec<StoreHash>> = HashMap::new();
    let mut session_pub_key: Option<String> = None;
    let mut session_found = false;

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
                session_pub_key = Some(pk.clone());
            }
            session_entries.extend(session.entries);
            for (sys, roots) in session.gc_roots {
                session_roots.entry(sys).or_default().extend(roots);
            }
        }
    }

    let mut receipt_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    let mut receipt_roots: HashMap<SystemArch, Vec<StoreHash>> = HashMap::new();
    let mut receipt_pub_key: Option<String> = None;

    // 2. 从本地 Receipt 文件/目录加载
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
                                        receipt_pub_key = Some(pk.clone());
                                    }
                                    receipt_entries.extend(receipt.new_entries);
                                    receipt_roots
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
                            receipt_pub_key = Some(pk.clone());
                        }
                        receipt_entries.extend(receipt.new_entries);
                        receipt_roots
                            .entry(receipt.system)
                            .or_default()
                            .extend(receipt.active_gc_roots);
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

    let total_promoted_entries = session_entries.len() + receipt_entries.len();

    if !session_found && receipt_paths.is_empty() && total_promoted_entries == 0 {
        info!("No session manifest or receipts found to promote. Merging with existing baseline.");
    }

    // 3. 拉取现存 Baseline 数据并按 SystemArch 分桶
    let mut partitioned_entries: HashMap<SystemArch, HashMap<StoreHash, IndexEntry>> =
        HashMap::new();
    let mut partitioned_roots: HashMap<SystemArch, Vec<StoreHash>> = HashMap::new();
    let mut base_pub_key = session_pub_key.or(receipt_pub_key).unwrap_or_default();

    if let Ok(Some(artifact)) = oci.fetch_artifact(target_tag).await {
        match artifact.manifest {
            OciArtifactManifest::Index(index) => {
                for desc in index.manifests {
                    if let Ok(Some((sub_json, _))) =
                        oci.get_manifest_with_digest(&desc.digest).await
                        && let Ok(sub_manifest) =
                            serde_json::from_str::<OciImageManifest>(&sub_json)
                        && let Some(layer) = sub_manifest.layers.first()
                        && let Ok(blob_bytes) = oci.get_blob(&layer.digest).await
                        && let Ok(arch_data) = IndexCodec::decode_zstd::<ArchCacheIndexData>(
                            &blob_bytes,
                            &layer.media_type,
                        )
                    {
                        if base_pub_key.is_empty() && !arch_data.public_key.is_empty() {
                            base_pub_key = arch_data.public_key;
                        }
                        partitioned_entries
                            .entry(arch_data.system)
                            .or_default()
                            .extend(arch_data.entries);
                        partitioned_roots
                            .entry(arch_data.system)
                            .or_default()
                            .extend(arch_data.gc_roots);
                    }
                }
            }
            OciArtifactManifest::Manifest(sub_manifest) => {
                if let Some(layer) = sub_manifest.layers.first()
                    && let Ok(blob_bytes) = oci.get_blob(&layer.digest).await
                    && let Ok(arch_data) = IndexCodec::decode_zstd::<ArchCacheIndexData>(
                        &blob_bytes,
                        &layer.media_type,
                    )
                {
                    if base_pub_key.is_empty() && !arch_data.public_key.is_empty() {
                        base_pub_key = arch_data.public_key;
                    }
                    partitioned_entries
                        .entry(arch_data.system)
                        .or_default()
                        .extend(arch_data.entries);
                    partitioned_roots
                        .entry(arch_data.system)
                        .or_default()
                        .extend(arch_data.gc_roots);
                }
            }
        }
    }

    // 合并 session 条目到分桶
    for (hash, entry) in session_entries {
        let sys = entry.system.unwrap_or_default();
        partitioned_entries
            .entry(sys)
            .or_default()
            .insert(hash, entry);
    }
    for (sys, roots) in session_roots {
        let entry_roots = partitioned_roots.entry(sys).or_default();
        let mut set: HashSet<StoreHash> = entry_roots.iter().cloned().collect();
        set.extend(roots);
        let mut sorted: Vec<StoreHash> = set.into_iter().collect();
        sorted.sort();
        *entry_roots = sorted;
    }

    // 合并 receipt 条目到分桶
    for (hash, entry) in receipt_entries {
        let sys = entry.system.unwrap_or_default();
        partitioned_entries
            .entry(sys)
            .or_default()
            .insert(hash, entry);
    }
    for (sys, roots) in receipt_roots {
        let entry_roots = partitioned_roots.entry(sys).or_default();
        let mut set: HashSet<StoreHash> = entry_roots.iter().cloned().collect();
        set.extend(roots);
        let mut sorted: Vec<StoreHash> = set.into_iter().collect();
        sorted.sort();
        *entry_roots = sorted;
    }

    // 4. 为每个系统架构构建并推送 Sub-Manifest 与 Index Blob
    let empty_config = Bytes::from_static(b"{}");
    let config_digest = oci.push_blob_bytes(empty_config).await?;
    let config_size = 2u64;

    let mut manifest_descriptors: Vec<OciDescriptor> = Vec::new();
    let mut all_systems: HashSet<SystemArch> = partitioned_entries.keys().cloned().collect();
    all_systems.extend(partitioned_roots.keys().cloned());

    for sys in all_systems {
        let entries = partitioned_entries.remove(&sys).unwrap_or_default();
        let roots = partitioned_roots.remove(&sys).unwrap_or_default();

        let arch_data = ArchCacheIndexData {
            version: CACHE_INDEX_VERSION,
            system: sys,
            repo: repo.to_string(),
            registry: registry.to_string(),
            generated: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            public_key: base_pub_key.clone(),
            entries,
            gc_roots: roots,
            last_promoted_run: run_id,
        };

        // 推送单架构 Index Blob
        let (blob_digest, compressed_size, uncompressed_size) =
            oci.push_zstd_blob(&arch_data).await?;

        // 构造单架构 Sub-Manifest
        let sub_manifest = build_arch_index_manifest(
            &blob_digest,
            compressed_size,
            uncompressed_size,
            &config_digest,
            config_size,
            &sys,
        );
        let sub_manifest_json = sub_manifest.to_json_string()?;
        let sub_manifest_digest = compute_sha256_digest(sub_manifest_json.as_bytes());

        // 推送架构特定 Tag (如 cache-index-x86_64-linux)
        let arch_tag = format!("{}-{}", target_tag, sys.as_str());
        oci.push_manifest(&arch_tag, &sub_manifest_json).await?;

        info!(
            "Pushed Sub-Manifest for architecture: {} (tag: {})",
            sys, arch_tag
        );

        // 生成挂载到顶层 Image Index 的 Descriptor
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

    // 5. 组装并原子发布顶层 OCI Image Index (cache-index)
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

    // 6. 清理会话标签 (包括全局会话与各架构专属会话)
    if cleanup_session && let Some(rid) = run_id {
        let main_tag = format!("run-{}", rid);
        let _ = oci.delete_manifest(&main_tag).await;

        for sys in [
            SystemArch::X86_64Linux,
            SystemArch::Aarch64Linux,
            SystemArch::X86_64Darwin,
            SystemArch::Aarch64Darwin,
            SystemArch::I686Linux,
            SystemArch::Armv7lLinux,
            SystemArch::Riscv64Linux,
        ] {
            let arch_tag = format!("run-{}-{}", rid, sys.as_str());
            let _ = oci.delete_manifest(&arch_tag).await;
        }
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
    use nixcache_core::{BuildReceipt, BuildStats, StoreHash, SystemArch};
    use std::collections::{HashMap, HashSet};
    use tempfile::tempdir;
    use tokio::fs;

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
}
