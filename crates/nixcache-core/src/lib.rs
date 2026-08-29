pub mod error;
pub mod gc;
pub mod lookup;
pub mod narinfo;
pub mod types;

pub use error::{CoreError, GcError, NarInfoParseError};
pub use gc::{GcEvaluationResult, evaluate_multi_arch_gc};
pub use lookup::{build_nar_lookup_map, extract_nar_basename, extract_store_hash};
pub use narinfo::NarInfo;
pub use types::{
    BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, JobSummaryMetadata,
    RECEIPT_VERSION, RUN_SESSION_VERSION, RunSessionManifest, SCHEMA_VERSION, deserialize_gc_roots,
};

#[cfg(test)]
mod tests {
    use super::{
        BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry,
        JobSummaryMetadata, NarInfo, RECEIPT_VERSION, RUN_SESSION_VERSION, RunSessionManifest,
        build_nar_lookup_map, evaluate_multi_arch_gc, extract_nar_basename, extract_store_hash,
    };
    use chrono::{Duration, Utc};
    use std::collections::HashMap;

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
            parsed.url,
            "nar/14j8s5vg8w80z5k86k6r00000000000000000000000.nar.xz"
        );
        assert_eq!(parsed.compression, Some("xz".to_string()));
        assert_eq!(parsed.file_size, Some(51200));
        assert_eq!(parsed.nar_size, 204800);
        assert_eq!(parsed.references.len(), 2);
        assert_eq!(parsed.signatures.len(), 2);
        assert_eq!(
            parsed.deriver,
            Some("a0000000000000000000000000000000-hello-2.12.1.drv".to_string())
        );

        assert_eq!(
            parsed.nar_basename(),
            "14j8s5vg8w80z5k86k6r00000000000000000000000.nar.xz"
        );
        assert_eq!(
            parsed.store_hash(),
            Some("s66mzxpvicwk07gjbjfw9izjfa797vsw")
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

        let mut entries = HashMap::new();
        entries.insert(
            "hash1".to_string(),
            IndexEntry {
                name: "pkg1".to_string(),
                narinfo: "StorePath: /nix/store/hash1-pkg1\nURL: nar/pkg1.nar.xz\n".to_string(),
                nar_digest: "sha256:blob1".to_string(),
                ..Default::default()
            },
        );
        entries.insert(
            "hash2".to_string(),
            IndexEntry {
                name: "pkg2".to_string(),
                narinfo: "StorePath: /nix/store/hash2-pkg2\nURL: nar/pkg2.nar.xz\n".to_string(),
                nar_digest: "sha256:blob2".to_string(),
                ..Default::default()
            },
        );

        let lookup = build_nar_lookup_map(&entries);
        assert_eq!(lookup.get("pkg1.nar.xz"), Some(&"sha256:blob1".to_string()));
        assert_eq!(lookup.get("pkg2.nar.xz"), Some(&"sha256:blob2".to_string()));
        assert_eq!(lookup.get("nonexistent.nar.xz"), None);
    }

    #[test]
    fn test_multi_arch_gc_evaluation() {
        let mut index = CacheIndexData::default();
        index.gc_roots.insert(
            "x86_64-linux".to_string(),
            vec!["hash-shared-lib".to_string(), "hash-x86-app".to_string()],
        );
        index.gc_roots.insert(
            "aarch64-linux".to_string(),
            vec!["hash-shared-lib".to_string(), "hash-arm-app".to_string()],
        );

        let now = Utc::now();
        let sixty_days_ago = (now - Duration::days(60)).to_rfc3339();
        let five_days_ago = (now - Duration::days(5)).to_rfc3339();

        index.entries.insert(
            "hash-shared-lib".to_string(),
            IndexEntry {
                name: "shared-lib".to_string(),
                added: sixty_days_ago.clone(),
                ..Default::default()
            },
        );
        index.entries.insert(
            "hash-x86-app".to_string(),
            IndexEntry {
                name: "x86-app".to_string(),
                added: sixty_days_ago.clone(),
                ..Default::default()
            },
        );
        index.entries.insert(
            "hash-arm-app".to_string(),
            IndexEntry {
                name: "arm-app".to_string(),
                added: sixty_days_ago.clone(),
                ..Default::default()
            },
        );
        index.entries.insert(
            "hash-orphan-old".to_string(),
            IndexEntry {
                name: "orphan-old".to_string(),
                added: sixty_days_ago.clone(),
                ..Default::default()
            },
        );
        index.entries.insert(
            "hash-orphan-new".to_string(),
            IndexEntry {
                name: "orphan-new".to_string(),
                added: five_days_ago,
                ..Default::default()
            },
        );

        let cutoff = now - Duration::days(30);
        let result = evaluate_multi_arch_gc(&index, &cutoff);

        assert_eq!(result.deleted_hashes, vec!["hash-orphan-old"]);
        assert_eq!(result.kept_entries.len(), 4);
        assert!(result.kept_entries.contains_key("hash-shared-lib"));
        assert!(result.kept_entries.contains_key("hash-x86-app"));
        assert!(result.kept_entries.contains_key("hash-arm-app"));
        assert!(result.kept_entries.contains_key("hash-orphan-new"));
        assert_eq!(result.live_roots.len(), 3);
    }

    #[test]
    fn test_types_serialization_and_migration() {
        let mut index = CacheIndexData {
            repo: "owner/repo".to_string(),
            ..Default::default()
        };
        index.entries.insert(
            "hash1".to_string(),
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/hash1-pkg1\n".to_string(),
                nar_digest: "sha256:digest1".to_string(),
                nar_size: 1024,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: Some("job:ci".to_string()),
            },
        );
        index
            .gc_roots
            .insert("x86_64-linux".to_string(), vec!["hash1".to_string()]);

        let json_str = serde_json::to_string(&index).expect("Failed to serialize index");
        let deserialized: CacheIndexData =
            serde_json::from_str(&json_str).expect("Failed to deserialize index");

        assert_eq!(deserialized.version, CACHE_INDEX_VERSION);
        assert_eq!(deserialized.repo, "owner/repo");
        assert_eq!(deserialized.entries.len(), 1);
        assert_eq!(
            deserialized.gc_roots.get("x86_64-linux").unwrap(),
            &vec!["hash1".to_string()]
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
            system: "x86_64-linux".to_string(),
            uploaded_blobs: 5,
            uploaded_bytes: 10240,
            timestamp: "2026-08-29T10:00:00Z".to_string(),
        });
        let session_json = serde_json::to_string(&session).unwrap();
        let loaded_session: RunSessionManifest = serde_json::from_str(&session_json).unwrap();
        assert_eq!(loaded_session.version, RUN_SESSION_VERSION);
        assert_eq!(loaded_session.run_id, 12345);
        assert_eq!(loaded_session.completed_jobs.len(), 1);

        let receipt = BuildReceipt::new(
            "x86_64-linux".to_string(),
            "owner/repo".to_string(),
            "2026-08-29T10:00:00Z".to_string(),
            Some("key:pub".to_string()),
            HashMap::new(),
            vec!["root1".to_string()],
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
