use crate::error::BuilderError;
use bytes::Bytes;
use chrono::Utc;
use nixcache_core::{CACHE_INDEX_VERSION, CacheIndexData, evaluate_multi_arch_gc};
use nixcache_oci::{
    OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciDescriptor, OciPlatform, build_arch_index_manifest,
    build_image_index,
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

/// 阶段 3: 跨平台垃圾回收阶段 (适配 OCI Image Index 与单架构 Sub-Manifest)
pub async fn run_gc(
    retention_days: u64,
    dry_run: bool,
    repo: &str,
    registry: &str,
    github_token: &str,
) -> Result<(), BuilderError> {
    info!(
        "Running multi-arch garbage collection for {}/{}",
        registry, repo
    );
    let oci = create_tokio_reqwest_client(registry, repo, github_token, true);

    let (index_data, _) = match oci.get_cache_index("cache-index").await? {
        Some(pair) => pair,
        None => {
            info!("No cache index found or index is empty, nothing to GC");
            return Ok(());
        }
    };

    if index_data.entries.is_empty() {
        info!("Cache index is empty, nothing to GC");
        return Ok(());
    }

    let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
    let gc_result = evaluate_multi_arch_gc(&index_data, &cutoff);

    info!(
        "GC Evaluation: Total: {}, Live Roots: {}, Kept: {}, To Delete: {}",
        index_data.entries.len(),
        gc_result.reachable_roots.len(),
        gc_result.kept_entries.len(),
        gc_result.deleted_hashes.len()
    );

    if dry_run {
        info!("Dry run complete. No modifications pushed.");
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
        entries: gc_result.kept_entries,
        gc_roots: index_data.gc_roots.clone(),
        last_promoted_run: index_data.last_promoted_run,
    };

    let partitioned = kept_data.into_arch_partitioned();
    let empty_config = Bytes::from_static(b"{}");
    let config_digest = oci.push_blob_bytes(empty_config).await?;
    let config_size = 2u64;

    let mut manifest_descriptors: Vec<OciDescriptor> = Vec::new();

    for (sys, arch_data) in partitioned {
        let (blob_digest, compressed_size, uncompressed_size) =
            oci.push_zstd_blob(&arch_data).await?;
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

    info!("Successfully updated multi-arch cache-index after GC.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use nixcache_core::{
        CacheIndexData, IndexEntry, NarDigest, NarInfoMeta, StoreHash, SystemArch,
        evaluate_multi_arch_gc,
    };

    #[test]
    fn test_gc_multi_arch_aggregation() {
        let hash_x86_live = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let hash_arm_live = StoreHash::parse("00000000000000000000000000000002").unwrap();
        let hash_dead_old = StoreHash::parse("00000000000000000000000000000003").unwrap();
        let hash_dead_recent = StoreHash::parse("00000000000000000000000000000004").unwrap();

        let mut index = CacheIndexData::default();
        index
            .gc_roots
            .insert(SystemArch::X86_64Linux, vec![hash_x86_live.clone()]);
        index
            .gc_roots
            .insert(SystemArch::Aarch64Linux, vec![hash_arm_live.clone()]);

        let now = chrono::Utc::now();
        let sixty_days_ago = (now - chrono::Duration::days(60)).to_rfc3339();
        let five_days_ago = (now - chrono::Duration::days(5)).to_rfc3339();

        let entry_x86_live = IndexEntry {
            name: "pkg-x86".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_x86_live),
                nar_basename: "pkg-x86.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 100,
            added: sixty_days_ago.clone(),
            origin_job: None,
        };
        let entry_arm_live = IndexEntry {
            name: "pkg-arm".to_string(),
            system: Some(SystemArch::Aarch64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_arm_live),
                nar_basename: "pkg-arm.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 100,
            added: sixty_days_ago.clone(),
            origin_job: None,
        };
        let entry_dead_old = IndexEntry {
            name: "pkg-dead-old".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_dead_old),
                nar_basename: "pkg-dead-old.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 100,
            added: sixty_days_ago.clone(),
            origin_job: None,
        };
        let entry_dead_recent = IndexEntry {
            name: "pkg-dead-recent".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash_dead_recent),
                nar_basename: "pkg-dead-recent.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 100,
            added: five_days_ago.clone(),
            origin_job: None,
        };

        index.entries.insert(hash_x86_live.clone(), entry_x86_live);
        index.entries.insert(hash_arm_live.clone(), entry_arm_live);
        index.entries.insert(hash_dead_old.clone(), entry_dead_old);
        index
            .entries
            .insert(hash_dead_recent.clone(), entry_dead_recent);

        let cutoff = now - chrono::Duration::days(30);
        let result = evaluate_multi_arch_gc(&index, &cutoff);

        assert_eq!(result.deleted_hashes, vec![hash_dead_old]);
        assert_eq!(result.kept_entries.len(), 3);
        assert!(result.kept_entries.contains_key(&hash_x86_live));
        assert!(result.kept_entries.contains_key(&hash_arm_live));
        assert!(result.kept_entries.contains_key(&hash_dead_recent));
    }

    #[test]
    fn test_gc_reachability_graph_algorithm() {
        let hash_shared_libc = StoreHash::parse("00000000000000000000000000000010").unwrap();
        let hash_x86_server = StoreHash::parse("00000000000000000000000000000011").unwrap();
        let hash_arm_server = StoreHash::parse("00000000000000000000000000000012").unwrap();
        let hash_darwin_client = StoreHash::parse("00000000000000000000000000000013").unwrap();
        let hash_orphan_ancient = StoreHash::parse("00000000000000000000000000000014").unwrap();
        let hash_orphan_recent = StoreHash::parse("00000000000000000000000000000015").unwrap();

        let mut index = CacheIndexData::default();
        index.gc_roots.insert(
            SystemArch::X86_64Linux,
            vec![hash_shared_libc.clone(), hash_x86_server.clone()],
        );
        index.gc_roots.insert(
            SystemArch::Aarch64Linux,
            vec![hash_shared_libc.clone(), hash_arm_server.clone()],
        );
        index
            .gc_roots
            .insert(SystemArch::Aarch64Darwin, vec![hash_darwin_client.clone()]);

        let now = chrono::Utc::now();
        let ninety_days_ago = (now - chrono::Duration::days(90)).to_rfc3339();
        let one_hour_ago = (now - chrono::Duration::hours(1)).to_rfc3339();

        let entries_def = vec![
            (hash_shared_libc.clone(), "glibc", ninety_days_ago.clone()),
            (
                hash_x86_server.clone(),
                "server-x86",
                ninety_days_ago.clone(),
            ),
            (
                hash_arm_server.clone(),
                "server-arm",
                ninety_days_ago.clone(),
            ),
            (
                hash_darwin_client.clone(),
                "client-mac",
                ninety_days_ago.clone(),
            ),
            (
                hash_orphan_ancient.clone(),
                "old-tool",
                ninety_days_ago.clone(),
            ),
            (hash_orphan_recent.clone(), "ci-temp", one_hour_ago),
        ];

        for (h, name, added) in entries_def {
            index.entries.insert(
                h.clone(),
                IndexEntry {
                    name: name.to_string(),
                    system: Some(SystemArch::X86_64Linux),
                    narinfo_meta: NarInfoMeta {
                        store_path: format!("/nix/store/{}-{}", h, name),
                        nar_basename: format!("{}.nar.xz", name),
                        nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0".to_string(),
                        ..Default::default()
                    },
                    nar_digest: NarDigest::new_sha256("0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0").unwrap(),
                    nar_size: 100,
                    added,
                    origin_job: None,
                },
            );
        }

        let cutoff = now - chrono::Duration::days(30);
        let result = evaluate_multi_arch_gc(&index, &cutoff);

        assert_eq!(result.deleted_hashes, vec![hash_orphan_ancient]);
        assert_eq!(result.kept_entries.len(), 5);
        assert!(result.kept_entries.contains_key(&hash_shared_libc));
        assert!(result.kept_entries.contains_key(&hash_orphan_recent));
    }
}
