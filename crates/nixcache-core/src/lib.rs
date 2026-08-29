pub mod error;
pub mod gc;
pub mod lookup;
pub mod narinfo;
pub mod types;

pub use error::{CoreError, GcError, NarInfoParseError, TypeError};
pub use gc::{GcEvaluationResult, evaluate_multi_arch_gc};
pub use lookup::{
    build_nar_lookup_map, extract_nar_basename, extract_store_hash, extract_store_hash_str,
};
pub use narinfo::NarInfo;
pub use types::{
    BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, JobSummaryMetadata,
    NarDigest, NarInfoMeta, RECEIPT_VERSION, RUN_SESSION_VERSION, RunSessionManifest,
    SCHEMA_VERSION, StoreHash, SystemArch,
};

#[cfg(test)]
mod tests {
    use super::{
        BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry,
        JobSummaryMetadata, NarDigest, NarInfo, NarInfoMeta, RECEIPT_VERSION, RUN_SESSION_VERSION,
        RunSessionManifest, StoreHash, SystemArch, TypeError, build_nar_lookup_map,
        evaluate_multi_arch_gc, extract_nar_basename, extract_store_hash, extract_store_hash_str,
    };
    use chrono::{Duration, Utc};
    use std::collections::HashMap;

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
        assert_eq!(arch.as_str(), "x86_64-linux");
        assert_eq!(format!("{}", arch), "x86_64-linux");
        assert_eq!(SystemArch::from("x86_64-linux"), SystemArch::X86_64Linux);
        assert_eq!(
            SystemArch::from("custom-arch"),
            SystemArch::Other("custom-arch".to_string())
        );
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
            StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap()
        );
        assert_eq!(
            parsed.references[1],
            StoreHash::parse("00000000000000000000000000000000").unwrap()
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
                    references: vec![shared_dep.clone()],
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
                    references: vec![sub_dep.clone()],
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
    fn test_types_serialization_and_migration() {
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
                    nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_sha256("0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0").unwrap(),
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
}
