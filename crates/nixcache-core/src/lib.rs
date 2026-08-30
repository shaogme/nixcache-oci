pub mod error;
pub mod gc;
pub mod lookup;
pub mod narinfo;
pub mod purge;
pub mod types;

pub use error::{CoreError, GcError, NarInfoParseError, TypeError};
pub use gc::{GcEvaluationResult, evaluate_multi_arch_gc};
pub use lookup::{
    build_nar_lookup_map, extract_nar_basename, extract_store_hash, extract_store_hash_str,
};
pub use narinfo::NarInfo;
pub use purge::{
    CachePurgeFilter, CascadeMode, PurgeEvaluationResult, SizeFilter, TimeFilter,
    evaluate_cache_purge, matches_pattern,
};
pub use types::{
    ArchCacheIndexData, ArchRunSessionManifest, BuildReceipt, BuildStats, CACHE_INDEX_VERSION,
    CacheIndexData, IndexEntry, JobSummaryMetadata, NarDigest, NarInfoMeta, RECEIPT_VERSION,
    RUN_SESSION_VERSION, RunSessionManifest, SCHEMA_VERSION, StoreHash, SystemArch,
};

#[cfg(test)]
mod tests {
    use super::{
        ArchCacheIndexData, ArchRunSessionManifest, BuildReceipt, BuildStats, CACHE_INDEX_VERSION,
        CacheIndexData, CachePurgeFilter, CascadeMode, IndexEntry, JobSummaryMetadata, NarDigest,
        NarInfo, NarInfoMeta, RECEIPT_VERSION, RUN_SESSION_VERSION, RunSessionManifest, SizeFilter,
        StoreHash, SystemArch, TimeFilter, TypeError, build_nar_lookup_map, evaluate_cache_purge,
        evaluate_multi_arch_gc, extract_nar_basename, extract_store_hash, extract_store_hash_str,
        matches_pattern,
    };
    use chrono::{DateTime, Duration, Utc};
    use std::collections::{HashMap, HashSet};

    #[test]
    fn test_strong_types_validation() {
        // Valid StoreHash (32 chars in nix base32)
        let hash_str = "s66mzxpvicwk07gjbjfw9izjfa797vsw";
        let store_hash = StoreHash::parse(hash_str).expect("Valid store hash");
        assert_eq!(store_hash.as_str(), hash_str);
        assert_eq!(format!("{}", store_hash), hash_str);

        // Invalid StoreHash (invalid char 'e' or wrong length)
        assert!(matches!(
            StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vse"),
            Err(TypeError::InvalidStoreHash(_))
        ));
        assert!(matches!(
            StoreHash::parse("short"),
            Err(TypeError::InvalidStoreHash(_))
        ));

        // Valid NarDigest
        let hex = "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0";
        let digest = NarDigest::new_sha256(hex).expect("Valid sha256 digest");
        assert_eq!(digest.as_str(), format!("sha256:{}", hex));

        let parsed_digest =
            NarDigest::parse(&format!("sha256:{}", hex)).expect("Valid digest parse");
        assert_eq!(digest, parsed_digest);

        // SystemArch display and parsing
        let arch = SystemArch::X86_64Linux;
        let arch_copy = arch; // Test Copy semantics
        assert_eq!(arch, arch_copy);
        assert_eq!(arch.as_str(), "x86_64-linux");
        assert_eq!(format!("{}", arch), "x86_64-linux");
        assert_eq!(SystemArch::from("x86_64-linux"), SystemArch::X86_64Linux);
        assert_eq!(SystemArch::from("custom-arch"), SystemArch::Unknown);

        // SystemArch VARIANTS and iteration
        assert!(SystemArch::VARIANTS.contains(&SystemArch::X86_64Linux));
        assert!(SystemArch::VARIANTS.contains(&SystemArch::Aarch64Linux));
        let all_systems: Vec<SystemArch> = SystemArch::all().collect();
        assert!(all_systems.contains(&SystemArch::X86_64Linux));
        assert!(!all_systems.contains(&SystemArch::Unknown));

        // SystemArch OCI platform mappings
        assert_eq!(arch.to_oci_platform_tuple(), ("linux", "amd64", None));
        assert_eq!(
            SystemArch::Aarch64Linux.to_oci_platform_tuple(),
            ("linux", "arm64", None)
        );
        assert_eq!(
            SystemArch::Aarch64Darwin.to_oci_platform_tuple(),
            ("darwin", "arm64", None)
        );
        assert_eq!(
            SystemArch::Armv7lLinux.to_oci_platform_tuple(),
            ("linux", "arm", Some("v7"))
        );
        assert_eq!(
            SystemArch::from_oci("linux", "amd64", None),
            SystemArch::X86_64Linux
        );
        assert_eq!(
            SystemArch::from_oci("linux", "arm64", None),
            SystemArch::Aarch64Linux
        );
        assert_eq!(
            SystemArch::from_oci("darwin", "arm64", None),
            SystemArch::Aarch64Darwin
        );
        assert_eq!(
            SystemArch::from_oci("linux", "arm", Some("v7")),
            SystemArch::Armv7lLinux
        );
        assert_eq!(
            SystemArch::from_oci("linux", "invalid_arch", None),
            SystemArch::Unknown
        );

        // SystemArch detect_current returns a known arch on supported platforms
        let detected = SystemArch::detect_current();
        assert!(detected.is_known());
    }

    #[test]
    fn test_narinfo_parse_and_serialize() {
        let content = r#"StorePath: /nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-hello-2.12.1
URL: nar/14j8s5vg8w80z5k86k6r00000000000000000000000.nar.xz
Compression: xz
FileHash: sha256:14j8s5vg8w80z5k86k6r5h3w08g214v0z6v26767v8v3wz7y30x9
FileSize: 51200
NarHash: sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0
NarSize: 204800
References: s66mzxpvicwk07gjbjfw9izjfa797vsw-hello-2.12.1 00000000000000000000000000000000-glibc-2.38
Deriver: a0000000000000000000000000000000-hello-2.12.1.drv
Sig: cache.nixos.org-1:abcd1234efgh5678
Sig: custom-cache:ijkl9012mnop3456
CA: fixed:sha256:0000000000000000000000000000000000000000000000000000000000000000
"#;

        let parsed = NarInfo::parse(content).expect("Failed to parse valid narinfo");
        assert_eq!(
            parsed.store_path,
            "/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-hello-2.12.1"
        );
        assert_eq!(
            parsed.nar_basename(),
            "14j8s5vg8w80z5k86k6r00000000000000000000000.nar.xz"
        );
        assert_eq!(parsed.compression, Some("xz".to_string()));
        assert_eq!(parsed.file_size, Some(51200));
        assert_eq!(parsed.nar_size, 204800);
        assert_eq!(parsed.references.len(), 2);
        assert_eq!(
            parsed.references[0],
            "s66mzxpvicwk07gjbjfw9izjfa797vsw-hello-2.12.1"
        );
        assert_eq!(
            parsed.references[1],
            "00000000000000000000000000000000-glibc-2.38"
        );
        assert_eq!(parsed.signatures.len(), 2);
        assert_eq!(
            parsed.deriver,
            Some("a0000000000000000000000000000000-hello-2.12.1.drv".to_string())
        );

        assert_eq!(
            parsed.store_hash(),
            Some(StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap())
        );

        let output_str = parsed.to_narinfo_string();
        let reparsed = NarInfo::parse(&output_str).expect("Failed to re-parse serialized narinfo");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn test_lookup_utilities() {
        let store_path = "/nix/store/3x796g9h5m792k762f036n93w156n96q-my-app-1.0";
        assert_eq!(
            extract_store_hash(store_path),
            Some(StoreHash::parse("3x796g9h5m792k762f036n93w156n96q").unwrap())
        );
        assert_eq!(
            extract_store_hash_str(store_path),
            Some("3x796g9h5m792k762f036n93w156n96q")
        );

        assert_eq!(
            extract_nar_basename("URL: nar/my-package.nar.xz"),
            "my-package.nar.xz"
        );
        assert_eq!(
            extract_nar_basename("URL: https://example.com/nar/direct.nar.xz"),
            "direct.nar.xz"
        );
        assert_eq!(extract_nar_basename("nar/test.nar.xz"), "test.nar.xz");

        let hash1 = StoreHash::new_unchecked("hash1111111111111111111111111111");
        let hash2 = StoreHash::new_unchecked("hash2222222222222222222222222222");

        let mut entries = HashMap::new();
        entries.insert(
            hash1,
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: "/nix/store/hash1-pkg1".to_string(),
                    nar_basename: "pkg1.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob1"),
                nar_size: 100,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );
        entries.insert(
            hash2,
            IndexEntry {
                name: "pkg2".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: "/nix/store/hash2-pkg2".to_string(),
                    nar_basename: "pkg2.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob2"),
                nar_size: 200,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        let lookup = build_nar_lookup_map(&entries);
        assert_eq!(
            lookup.get("pkg1.nar.xz"),
            Some(&NarDigest::new_unchecked("sha256:blob1"))
        );
        assert_eq!(
            lookup.get("pkg2.nar.xz"),
            Some(&NarDigest::new_unchecked("sha256:blob2"))
        );
        assert_eq!(lookup.get("nonexistent.nar.xz"), None);
    }

    #[test]
    fn test_multi_arch_gc_evaluation() {
        let root_app = StoreHash::new_unchecked("hash0000000000000000000000000app");
        let shared_dep = StoreHash::new_unchecked("hash0000000000000000000000000dep");
        let sub_dep = StoreHash::new_unchecked("hash0000000000000000000000000sub");
        let orphan_old = StoreHash::new_unchecked("hash0000000000000000000000000old");
        let orphan_new = StoreHash::new_unchecked("hash0000000000000000000000000new");

        let mut index = CacheIndexData::default();
        // 只有 root_app 被列为 gc_root，shared_dep 与 sub_dep 是其传递依赖
        index
            .gc_roots
            .insert(SystemArch::X86_64Linux, vec![root_app.clone()]);

        let now = Utc::now();
        let sixty_days_ago = (now - Duration::days(60)).to_rfc3339();
        let five_days_ago = (now - Duration::days(5)).to_rfc3339();

        // 1. Root app 引用 shared_dep
        index.entries.insert(
            root_app.clone(),
            IndexEntry {
                name: "root-app".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-root-app", root_app),
                    nar_basename: "root-app.nar.xz".to_string(),
                    references: vec![format!("{}-shared-dep", shared_dep)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:root-blob"),
                nar_size: 100,
                added: sixty_days_ago.clone(),
                origin_job: None,
            },
        );

        // 2. Shared dep 引用 sub_dep (深层传递依赖，虽然生成于60天前且未在root中列出，但闭包可达必须保留)
        index.entries.insert(
            shared_dep.clone(),
            IndexEntry {
                name: "shared-dep".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-shared-dep", shared_dep),
                    nar_basename: "shared-dep.nar.xz".to_string(),
                    references: vec![format!("{}-sub-dep", sub_dep)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:dep-blob"),
                nar_size: 100,
                added: sixty_days_ago.clone(),
                origin_job: None,
            },
        );

        // 3. Sub dep (叶子依赖，60天前生成，闭包可达必须保留)
        index.entries.insert(
            sub_dep.clone(),
            IndexEntry {
                name: "sub-dep".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-sub-dep", sub_dep),
                    nar_basename: "sub-dep.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:sub-blob"),
                nar_size: 100,
                added: sixty_days_ago.clone(),
                origin_job: None,
            },
        );

        // 4. 孤立过期条目 (不可达且过期，应该被删除)
        index.entries.insert(
            orphan_old.clone(),
            IndexEntry {
                name: "orphan-old".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-orphan-old", orphan_old),
                    nar_basename: "orphan-old.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:old-blob"),
                nar_size: 100,
                added: sixty_days_ago,
                origin_job: None,
            },
        );

        // 5. 孤立新条目 (不可达但未过期，在宽限期内保留)
        index.entries.insert(
            orphan_new.clone(),
            IndexEntry {
                name: "orphan-new".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-orphan-new", orphan_new),
                    nar_basename: "orphan-new.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:new-blob"),
                nar_size: 100,
                added: five_days_ago,
                origin_job: None,
            },
        );

        let cutoff = now - Duration::days(30);
        let result = evaluate_multi_arch_gc(&index, &cutoff);

        assert_eq!(result.deleted_hashes, vec![orphan_old]);
        assert_eq!(result.kept_entries.len(), 4);
        assert!(result.kept_entries.contains_key(&root_app));
        assert!(
            result.kept_entries.contains_key(&shared_dep),
            "Closure reachable shared_dep must be kept!"
        );
        assert!(
            result.kept_entries.contains_key(&sub_dep),
            "Closure reachable sub_dep must be kept!"
        );
        assert!(result.kept_entries.contains_key(&orphan_new));
        assert_eq!(result.reachable_roots.len(), 3);
    }

    #[test]
    fn test_types_serialization() {
        let hash1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let mut index = CacheIndexData {
            repo: "owner/repo".to_string(),
            ..Default::default()
        };
        index.entries.insert(
            hash1.clone(),
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-pkg1", hash1),
                    nar_basename: "pkg1.nar.xz".to_string(),
                    nar_hash:
                        "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                            .to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_sha256(
                    "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
                )
                .unwrap(),
                nar_size: 1024,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: Some("job:ci".to_string()),
            },
        );
        index
            .gc_roots
            .insert(SystemArch::X86_64Linux, vec![hash1.clone()]);

        let json_str = serde_json::to_string(&index).expect("Failed to serialize index");
        let deserialized: CacheIndexData =
            serde_json::from_str(&json_str).expect("Failed to deserialize index");

        assert_eq!(deserialized.version, CACHE_INDEX_VERSION);
        assert_eq!(deserialized.repo, "owner/repo");
        assert_eq!(deserialized.entries.len(), 1);
        assert_eq!(
            deserialized.gc_roots.get(&SystemArch::X86_64Linux).unwrap(),
            &vec![hash1]
        );
    }

    #[test]
    fn test_session_and_receipt_structures() {
        let mut session = RunSessionManifest {
            run_id: 12345,
            head_sha: "abcdef".to_string(),
            ref_name: "refs/heads/main".to_string(),
            ..Default::default()
        };
        session.completed_jobs.push(JobSummaryMetadata {
            job_id: "build-x86".to_string(),
            system: SystemArch::X86_64Linux,
            uploaded_blobs: 5,
            uploaded_bytes: 10240,
            timestamp: "2026-08-29T10:00:00Z".to_string(),
        });
        let session_json = serde_json::to_string(&session).unwrap();
        let loaded_session: RunSessionManifest = serde_json::from_str(&session_json).unwrap();
        assert_eq!(loaded_session.version, RUN_SESSION_VERSION);
        assert_eq!(loaded_session.run_id, 12345);
        assert_eq!(loaded_session.completed_jobs.len(), 1);

        // Arch-scoped session
        let mut arch_session = ArchRunSessionManifest::new(12345, SystemArch::X86_64Linux);
        arch_session.completed_jobs.push(JobSummaryMetadata {
            job_id: "build-x86".to_string(),
            system: SystemArch::X86_64Linux,
            uploaded_blobs: 5,
            uploaded_bytes: 10240,
            timestamp: "2026-08-29T10:00:00Z".to_string(),
        });
        let arch_session_json = serde_json::to_string(&arch_session).unwrap();
        let loaded_arch_session: ArchRunSessionManifest =
            serde_json::from_str(&arch_session_json).unwrap();
        assert_eq!(loaded_arch_session.version, RUN_SESSION_VERSION);
        assert_eq!(loaded_arch_session.system, SystemArch::X86_64Linux);

        // Arch-scoped cache index
        let arch_index = ArchCacheIndexData::new(SystemArch::Aarch64Linux, "owner/repo", "ghcr.io");
        let arch_index_json = serde_json::to_string(&arch_index).unwrap();
        let loaded_arch_index: ArchCacheIndexData = serde_json::from_str(&arch_index_json).unwrap();
        assert_eq!(loaded_arch_index.version, CACHE_INDEX_VERSION);
        assert_eq!(loaded_arch_index.system, SystemArch::Aarch64Linux);

        let root1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let receipt = BuildReceipt::new(
            SystemArch::X86_64Linux,
            "owner/repo".to_string(),
            "2026-08-29T10:00:00Z".to_string(),
            Some("key:pub".to_string()),
            HashMap::new(),
            vec![root1],
            BuildStats {
                discovered_outputs: 2,
                built_paths: 2,
                substituted_paths: 0,
                uploaded_blobs: 2,
                total_bytes_uploaded: 5000,
            },
        )
        .with_run_info(Some(12345), Some("job1".to_string()));
        assert_eq!(receipt.version, RECEIPT_VERSION);
        let receipt_json = serde_json::to_string(&receipt).unwrap();
        let loaded_receipt: BuildReceipt = serde_json::from_str(&receipt_json).unwrap();
        assert_eq!(loaded_receipt.job_id, Some("job1".to_string()));
        assert_eq!(loaded_receipt.stats.uploaded_blobs, 2);
    }

    #[test]
    fn test_pattern_wildcard_matching() {
        assert!(matches_pattern("*chromium*", "chromium-120.0"));
        assert!(matches_pattern(
            "*chromium*",
            "/nix/store/hash-chromium-120"
        ));
        assert!(matches_pattern("*-debug", "app-debug"));
        assert!(!matches_pattern("*-debug", "app-release"));
        assert!(matches_pattern("linux-6.1.*", "linux-6.1.100"));
        assert!(matches_pattern("libA", "libA.so"));
        assert!(matches_pattern("?", "a"));
        assert!(!matches_pattern("?", "ab"));
    }

    #[test]
    fn test_purge_all_clears_everything() {
        let hash1 = StoreHash::new_unchecked("hash1111111111111111111111111111");
        let hash2 = StoreHash::new_unchecked("hash2222222222222222222222222222");

        let mut index = CacheIndexData::default();
        index.entries.insert(
            hash1.clone(),
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-pkg1", hash1),
                    nar_basename: "pkg1.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob1"),
                nar_size: 1000,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );
        index.entries.insert(
            hash2.clone(),
            IndexEntry {
                name: "pkg2".to_string(),
                system: Some(SystemArch::Aarch64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-pkg2", hash2),
                    nar_basename: "pkg2.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob2"),
                nar_size: 2000,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );
        index
            .gc_roots
            .insert(SystemArch::X86_64Linux, vec![hash1.clone()]);
        index
            .gc_roots
            .insert(SystemArch::Aarch64Linux, vec![hash2.clone()]);

        let filter = CachePurgeFilter {
            purge_all: true,
            ..Default::default()
        };

        let result = evaluate_cache_purge(&index, &filter);
        assert!(result.kept_entries.is_empty());
        assert_eq!(result.purged_entries.len(), 2);
        assert_eq!(result.purged_hashes.len(), 2);
        assert_eq!(result.purged_nar_digests.len(), 2);
        assert_eq!(result.estimated_freed_bytes, 3000);
        assert!(result.updated_gc_roots.is_empty());
    }

    #[test]
    fn test_purge_exact_preserves_dependents() {
        let app = StoreHash::new_unchecked("hash0000000000000000000000000app");
        let lib_a = StoreHash::new_unchecked("hash000000000000000000000000liba");
        let core = StoreHash::new_unchecked("hash000000000000000000000000core");

        let mut index = CacheIndexData::default();
        index
            .gc_roots
            .insert(SystemArch::X86_64Linux, vec![app.clone()]);

        index.entries.insert(
            app.clone(),
            IndexEntry {
                name: "app".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-app", app),
                    nar_basename: "app.nar.xz".to_string(),
                    references: vec![format!("{}-liba", lib_a)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-app"),
                nar_size: 500,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        index.entries.insert(
            lib_a.clone(),
            IndexEntry {
                name: "liba".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-liba", lib_a),
                    nar_basename: "liba.nar.xz".to_string(),
                    references: vec![format!("{}-core", core)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-liba"),
                nar_size: 300,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        index.entries.insert(
            core.clone(),
            IndexEntry {
                name: "core".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-core", core),
                    nar_basename: "core.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-core"),
                nar_size: 200,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        let mut hashes = HashSet::new();
        hashes.insert(lib_a.clone());

        let filter = CachePurgeFilter {
            store_hashes: hashes,
            cascade_mode: CascadeMode::Exact,
            ..Default::default()
        };

        let result = evaluate_cache_purge(&index, &filter);
        assert_eq!(result.purged_hashes, vec![lib_a.clone()]);
        assert_eq!(result.estimated_freed_bytes, 300);
        assert!(result.kept_entries.contains_key(&app));
        assert!(result.kept_entries.contains_key(&core));

        // GC Roots 同步修剪：因为 app 依赖的 lib_a 被删除，app 发生断链，app 被从 roots 剔除
        assert!(result.updated_gc_roots.is_empty());
    }

    #[test]
    fn test_purge_cascade_dependents_invalidates_closure() {
        let app = StoreHash::new_unchecked("hash0000000000000000000000000app");
        let lib_a = StoreHash::new_unchecked("hash000000000000000000000000liba");
        let core = StoreHash::new_unchecked("hash000000000000000000000000core");

        let mut index = CacheIndexData::default();
        index
            .gc_roots
            .insert(SystemArch::X86_64Linux, vec![app.clone()]);

        index.entries.insert(
            app.clone(),
            IndexEntry {
                name: "app".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-app", app),
                    nar_basename: "app.nar.xz".to_string(),
                    references: vec![format!("{}-liba", lib_a)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-app"),
                nar_size: 500,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        index.entries.insert(
            lib_a.clone(),
            IndexEntry {
                name: "liba".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-liba", lib_a),
                    nar_basename: "liba.nar.xz".to_string(),
                    references: vec![format!("{}-core", core)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-liba"),
                nar_size: 300,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        index.entries.insert(
            core.clone(),
            IndexEntry {
                name: "core".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-core", core),
                    nar_basename: "core.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-core"),
                nar_size: 200,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        let mut hashes = HashSet::new();
        hashes.insert(lib_a.clone());

        let filter = CachePurgeFilter {
            store_hashes: hashes,
            cascade_mode: CascadeMode::Dependents,
            ..Default::default()
        };

        let result = evaluate_cache_purge(&index, &filter);
        assert_eq!(result.purged_entries.len(), 2);
        assert!(result.purged_entries.contains_key(&lib_a));
        assert!(result.purged_entries.contains_key(&app));
        assert!(result.kept_entries.contains_key(&core));
        assert_eq!(result.estimated_freed_bytes, 800);
        assert!(result.updated_gc_roots.is_empty());
    }

    #[test]
    fn test_purge_cascade_transitive_and_full_tree() {
        let app = StoreHash::new_unchecked("hash0000000000000000000000000app");
        let lib_a = StoreHash::new_unchecked("hash000000000000000000000000liba");
        let core = StoreHash::new_unchecked("hash000000000000000000000000core");

        let mut index = CacheIndexData::default();
        index.entries.insert(
            app.clone(),
            IndexEntry {
                name: "app".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-app", app),
                    nar_basename: "app.nar.xz".to_string(),
                    references: vec![format!("{}-liba", lib_a)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-app"),
                nar_size: 500,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );
        index.entries.insert(
            lib_a.clone(),
            IndexEntry {
                name: "liba".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-liba", lib_a),
                    nar_basename: "liba.nar.xz".to_string(),
                    references: vec![format!("{}-core", core)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-liba"),
                nar_size: 300,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );
        index.entries.insert(
            core.clone(),
            IndexEntry {
                name: "core".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-core", core),
                    nar_basename: "core.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-core"),
                nar_size: 200,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        let mut hashes = HashSet::new();
        hashes.insert(lib_a.clone());

        // Transitive: lib_a + core
        let filter_transitive = CachePurgeFilter {
            store_hashes: hashes.clone(),
            cascade_mode: CascadeMode::Transitive,
            ..Default::default()
        };
        let res_transitive = evaluate_cache_purge(&index, &filter_transitive);
        assert_eq!(res_transitive.purged_entries.len(), 2);
        assert!(res_transitive.purged_entries.contains_key(&lib_a));
        assert!(res_transitive.purged_entries.contains_key(&core));
        assert!(res_transitive.kept_entries.contains_key(&app));

        // FullTree: app + lib_a + core
        let filter_full = CachePurgeFilter {
            store_hashes: hashes,
            cascade_mode: CascadeMode::FullTree,
            ..Default::default()
        };
        let res_full = evaluate_cache_purge(&index, &filter_full);
        assert_eq!(res_full.purged_entries.len(), 3);
        assert!(res_full.purged_entries.contains_key(&app));
        assert!(res_full.purged_entries.contains_key(&lib_a));
        assert!(res_full.purged_entries.contains_key(&core));
        assert!(res_full.kept_entries.is_empty());
    }

    #[test]
    fn test_purge_pattern_and_time_and_size_filter() {
        let hash_chromium = StoreHash::new_unchecked("hash00000000000000000000chromium");
        let hash_small = StoreHash::new_unchecked("hash00000000000000000000000small");
        let hash_old = StoreHash::new_unchecked("hash0000000000000000000000000old");

        let mut index = CacheIndexData::default();
        index.entries.insert(
            hash_chromium.clone(),
            IndexEntry {
                name: "chromium-120.0".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-chromium-120.0", hash_chromium),
                    nar_basename: "chromium-120.0.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-chromium"),
                nar_size: 500_000_000,
                added: "2026-08-25T10:00:00Z".to_string(),
                origin_job: Some("run:1001:job:build-x86".to_string()),
            },
        );

        index.entries.insert(
            hash_small.clone(),
            IndexEntry {
                name: "small-lib".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-small-lib", hash_small),
                    nar_basename: "small-lib.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-small"),
                nar_size: 1_000,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: Some("run:1002:job:build-x86".to_string()),
            },
        );

        index.entries.insert(
            hash_old.clone(),
            IndexEntry {
                name: "old-lib".to_string(),
                system: Some(SystemArch::Aarch64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-old-lib", hash_old),
                    nar_basename: "old-lib.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-old"),
                nar_size: 2_000,
                added: "2026-07-01T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        // Pattern filter
        let filter_pat = CachePurgeFilter {
            patterns: vec!["*chromium*".to_string()],
            cascade_mode: CascadeMode::Exact,
            ..Default::default()
        };
        let res_pat = evaluate_cache_purge(&index, &filter_pat);
        assert_eq!(res_pat.purged_hashes, vec![hash_chromium.clone()]);

        // Size filter: MinBytes(100MB)
        let filter_size = CachePurgeFilter {
            size_filter: Some(SizeFilter::MinBytes(100_000_000)),
            cascade_mode: CascadeMode::Exact,
            ..Default::default()
        };
        let res_size = evaluate_cache_purge(&index, &filter_size);
        assert_eq!(res_size.purged_hashes, vec![hash_chromium.clone()]);

        // Time filter: Before 2026-08-01
        let filter_time = CachePurgeFilter {
            time_filter: Some(TimeFilter::Before(
                DateTime::parse_from_rfc3339("2026-08-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )),
            cascade_mode: CascadeMode::Exact,
            ..Default::default()
        };
        let res_time = evaluate_cache_purge(&index, &filter_time);
        assert_eq!(res_time.purged_hashes, vec![hash_old.clone()]);

        // System filter
        let mut sys_filter = HashSet::new();
        sys_filter.insert(SystemArch::Aarch64Linux);
        let filter_sys = CachePurgeFilter {
            systems: sys_filter,
            patterns: vec!["*lib*".to_string()],
            cascade_mode: CascadeMode::Exact,
            ..Default::default()
        };
        let res_sys = evaluate_cache_purge(&index, &filter_sys);
        assert_eq!(res_sys.purged_hashes, vec![hash_old.clone()]);
    }

    #[test]
    fn test_purge_gc_roots_resynchronization() {
        let root_x86 = StoreHash::new_unchecked("000000000000000000000000000root1");
        let root_arm = StoreHash::new_unchecked("000000000000000000000000000root2");
        let dep_x86 = StoreHash::new_unchecked("0000000000000000000000000000dep1");

        let mut index = CacheIndexData::default();
        index
            .gc_roots
            .insert(SystemArch::X86_64Linux, vec![root_x86.clone()]);
        index
            .gc_roots
            .insert(SystemArch::Aarch64Linux, vec![root_arm.clone()]);

        index.entries.insert(
            root_x86.clone(),
            IndexEntry {
                name: "root-x86".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-root-x86", root_x86),
                    nar_basename: "root-x86.nar.xz".to_string(),
                    references: vec![format!("{}-dep-x86", dep_x86)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-root-x86"),
                nar_size: 100,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        index.entries.insert(
            dep_x86.clone(),
            IndexEntry {
                name: "dep-x86".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-dep-x86", dep_x86),
                    nar_basename: "dep-x86.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-dep-x86"),
                nar_size: 100,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        index.entries.insert(
            root_arm.clone(),
            IndexEntry {
                name: "root-arm".to_string(),
                system: Some(SystemArch::Aarch64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-root-arm", root_arm),
                    nar_basename: "root-arm.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-root-arm"),
                nar_size: 100,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        // Purge dep_x86 with Exact mode
        let mut hashes = HashSet::new();
        hashes.insert(dep_x86.clone());
        let filter = CachePurgeFilter {
            store_hashes: hashes,
            cascade_mode: CascadeMode::Exact,
            ..Default::default()
        };

        let result = evaluate_cache_purge(&index, &filter);
        // root_x86 should be pruned because its dependency dep_x86 was purged!
        // root_arm should remain untouched!
        assert_eq!(
            result.updated_gc_roots.get(&SystemArch::Aarch64Linux),
            Some(&vec![root_arm])
        );
        assert_eq!(result.updated_gc_roots.get(&SystemArch::X86_64Linux), None);
    }

    #[test]
    fn test_purge_with_protect_gc_roots() {
        let root_app = StoreHash::new_unchecked("hash0000000000000000000000000app");
        let shared_dep = StoreHash::new_unchecked("hash0000000000000000000000000dep");
        let orphan_old = StoreHash::new_unchecked("hash0000000000000000000000000old");

        let mut index = CacheIndexData::default();
        index
            .gc_roots
            .insert(SystemArch::X86_64Linux, vec![root_app.clone()]);

        let sixty_days_ago = (Utc::now() - Duration::days(60)).to_rfc3339();

        index.entries.insert(
            root_app.clone(),
            IndexEntry {
                name: "root-app".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-root-app", root_app),
                    nar_basename: "root-app.nar.xz".to_string(),
                    references: vec![format!("{}-shared-dep", shared_dep)],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:root-blob"),
                nar_size: 100,
                added: sixty_days_ago.clone(),
                origin_job: None,
            },
        );

        index.entries.insert(
            shared_dep.clone(),
            IndexEntry {
                name: "shared-dep".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-shared-dep", shared_dep),
                    nar_basename: "shared-dep.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:dep-blob"),
                nar_size: 100,
                added: sixty_days_ago.clone(),
                origin_job: None,
            },
        );

        index.entries.insert(
            orphan_old.clone(),
            IndexEntry {
                name: "orphan-old".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-orphan-old", orphan_old),
                    nar_basename: "orphan-old.nar.xz".to_string(),
                    references: vec![],
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:old-blob"),
                nar_size: 100,
                added: sixty_days_ago.clone(),
                origin_job: None,
            },
        );

        // When protect_gc_roots is true, root_app and shared_dep must be protected even if older_than / patterns match
        let cutoff = Utc::now() - Duration::days(30);
        let filter = CachePurgeFilter {
            time_filter: Some(TimeFilter::Before(cutoff)),
            protect_gc_roots: true,
            cascade_mode: CascadeMode::Exact,
            ..Default::default()
        };

        let result = evaluate_cache_purge(&index, &filter);
        assert_eq!(result.purged_hashes, vec![orphan_old]);
        assert_eq!(result.kept_entries.len(), 2);
        assert!(result.kept_entries.contains_key(&root_app));
        assert!(result.kept_entries.contains_key(&shared_dep));
        assert_eq!(
            result.updated_gc_roots.get(&SystemArch::X86_64Linux),
            Some(&vec![root_app])
        );
    }
}
