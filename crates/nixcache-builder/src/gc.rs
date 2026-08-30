use crate::{cli::GcArgs, error::BuilderError, purge::run_purge};
use tracing::info;

/// 阶段 3: 跨平台垃圾回收阶段 (统一复用 purge 执行引擎)
pub async fn run_gc(
    args: &GcArgs,
    repo: &str,
    registry: &str,
    github_token: &str,
) -> Result<(), BuilderError> {
    let filter_args = args.to_purge_filter_args();
    info!(
        "Running multi-arch garbage collection via purge engine for {}/{} (retention: {} days, dry_run: {}, delete_blobs: {})",
        registry,
        repo,
        args.resolve_retention_days(),
        filter_args.dry_run,
        filter_args.delete_blobs
    );

    run_purge(&filter_args, repo, registry, github_token).await
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
