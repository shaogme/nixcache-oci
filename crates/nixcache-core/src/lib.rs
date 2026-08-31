pub mod bloom;
pub mod error;
pub mod filter;
pub mod gc;
pub mod lookup;
pub mod narinfo;
pub mod purge;
pub mod sharding;
pub mod types;

pub use bloom::{BloomFilter, FastBlockedBloomFilter, murmur3_x64_128};
pub use error::{BloomError, CoreError, NarInfoParseError, TypeError};
pub use filter::{
    CacheQueryResult, CacheSelector, CascadeMode, FilterPredicates, SelectionScope, SizeFilter,
    SortBy, SortOrder, TimeFilter, evaluate_arch_cache_query, evaluate_cache_query,
    matches_pattern,
};
pub use gc::{GcEvaluationResult, evaluate_gc, evaluate_multi_arch_gc};
pub use lookup::{
    build_nar_lookup_map, extract_nar_basename, extract_store_hash, extract_store_hash_str,
};
pub use narinfo::NarInfo;
pub use purge::{
    PurgeEvaluationResult, evaluate_arch_cache_purge, evaluate_cache_purge, prune_broken_gc_roots,
};
pub use sharding::{
    EMPTY_SHARD_MERKLE_HASH, NIX_BASE32_ALPHABET, calculate_shard_id, calculate_shard_id_from_str,
    compute_merkle_root, compute_shard_merkle_hash, diff_shard_descriptors, nix_base32_char,
    nix_base32_val, partition_entries_by_shard, partition_hashes_by_shard, shard_id_to_prefix,
    shard_id_to_prefix_bytes,
};
pub use types::{
    BloomFilterManifest, BuildReceipt, BuildStats, CACHE_INDEX_VERSION, DeltaPatchData, IndexEntry,
    JobSummaryMetadata, NUM_SHARDS, NarDigest, NarInfoMeta, RECEIPT_VERSION, RUN_SESSION_VERSION,
    SCHEMA_VERSION, SCHEMA_VERSION_V5, ShardDataPayload, ShardDescriptor,
    ShardedArchCacheIndexData, StoreHash, SystemArch,
};

#[cfg(test)]
mod tests {
    use super::{
        BloomError, BloomFilter, BloomFilterManifest, BuildReceipt, BuildStats,
        CACHE_INDEX_VERSION, CacheQueryResult, CacheSelector, CascadeMode, CoreError,
        DeltaPatchData, EMPTY_SHARD_MERKLE_HASH, FastBlockedBloomFilter, FilterPredicates,
        IndexEntry, JobSummaryMetadata, NIX_BASE32_ALPHABET, NUM_SHARDS, NarDigest, NarInfo,
        NarInfoMeta, NarInfoParseError, RECEIPT_VERSION, SCHEMA_VERSION_V5, SelectionScope,
        ShardDataPayload, ShardDescriptor, ShardedArchCacheIndexData, SizeFilter, StoreHash,
        SystemArch, TimeFilter, TypeError, build_nar_lookup_map, calculate_shard_id,
        calculate_shard_id_from_str, compute_merkle_root, compute_shard_merkle_hash,
        diff_shard_descriptors, evaluate_arch_cache_purge, evaluate_arch_cache_query,
        evaluate_cache_purge, evaluate_cache_query, evaluate_gc, evaluate_multi_arch_gc,
        extract_nar_basename, extract_store_hash, extract_store_hash_str, matches_pattern,
        nix_base32_char, nix_base32_val, partition_entries_by_shard, shard_id_to_prefix,
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
            Err(TypeError::StoreHashInvalidChar {
                char: 'e',
                index: 31
            })
        ));
        assert!(matches!(
            StoreHash::parse("short"),
            Err(TypeError::StoreHashInvalidLength { actual: 5 })
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
    fn test_sharding_and_prefix_mapping() {
        assert_eq!(NIX_BASE32_ALPHABET.len(), 32);

        // Test all 32 alphabet characters mapping
        for (i, &byte) in NIX_BASE32_ALPHABET.iter().enumerate() {
            let val = nix_base32_val(byte).expect("Valid base32 char");
            assert_eq!(val as usize, i);
            let converted_byte = nix_base32_char(val).expect("Valid base32 val");
            assert_eq!(converted_byte, byte);
        }

        // Test invalid base32 characters: 'e', 'o', 't', 'u'
        assert!(nix_base32_val(b'e').is_err());
        assert!(nix_base32_val(b'o').is_err());
        assert!(nix_base32_val(b't').is_err());
        assert!(nix_base32_val(b'u').is_err());

        // Test all 1024 shards round-trip
        for shard_id in 0..1024u16 {
            let prefix = shard_id_to_prefix(shard_id);
            assert_eq!(prefix.len(), 2);
            let parsed_shard_id =
                calculate_shard_id_from_str(&prefix).expect("Valid prefix to shard_id");
            assert_eq!(parsed_shard_id, shard_id);
        }

        // Boundary cases
        assert_eq!(shard_id_to_prefix(0), "00");
        assert_eq!(shard_id_to_prefix(1), "01");
        assert_eq!(shard_id_to_prefix(1023), "zz");

        // Specific hash test: "s66mzxpvicwk07gjbjfw9izjfa797vsw"
        let hash = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let sid = calculate_shard_id(&hash);
        assert_eq!(sid, hash.shard_id());
        assert_eq!(shard_id_to_prefix(sid), "s6");

        // Partition test
        let mut entries = HashMap::new();
        let h1 = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let h2 = StoreHash::parse("00000000000000000000000000000002").unwrap();
        let h3 = StoreHash::parse("s6000000000000000000000000000000").unwrap();

        entries.insert(h1.clone(), IndexEntry::default());
        entries.insert(h2.clone(), IndexEntry::default());
        entries.insert(h3.clone(), IndexEntry::default());

        let partitioned = partition_entries_by_shard(entries);
        assert_eq!(partitioned.get(&0).unwrap().len(), 2);
        assert_eq!(partitioned.get(&sid).unwrap().len(), 1);
    }

    #[test]
    fn test_fast_blocked_bloom_filter() {
        let mut filter = FastBlockedBloomFilter::new_with_defaults(100);
        assert!(filter.is_empty());
        assert_eq!(filter.num_entries(), 0);

        let h1 = StoreHash::parse("00000000000000000000000000000001").unwrap();
        let h2 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let h3 = StoreHash::parse("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").unwrap();
        let non_existent = StoreHash::parse("ffffffffffffffffffffffffffffffff").unwrap();

        filter.insert(&h1);
        filter.insert(&h2);
        filter.insert(&h3);

        assert_eq!(filter.num_entries(), 3);
        assert!(!filter.is_empty());

        // Zero false negatives
        assert!(filter.contains(&h1));
        assert!(filter.contains(&h2));
        assert!(filter.contains(&h3));
        assert!(!filter.contains(&non_existent));

        // Binary serialization & deserialization roundtrip
        let bytes = filter.to_bytes();
        assert_eq!(bytes.len() % 64, 0);

        let restored =
            FastBlockedBloomFilter::from_bytes(&bytes, filter.num_entries(), filter.num_hashes())
                .expect("Valid bloom bytes restore");

        assert_eq!(filter, restored);
        assert!(restored.contains(&h1));
        assert!(restored.contains(&h2));
        assert!(restored.contains(&h3));
        assert!(!restored.contains(&non_existent));

        // Large scale false positive rate test
        let mut large_filter = FastBlockedBloomFilter::new_with_defaults(2000);
        let mut inserted_set = HashSet::new();

        for i in 0..2000 {
            let hash = StoreHash::new_unchecked(format!("a{:031}", i));
            large_filter.insert(&hash);
            inserted_set.insert(hash);
        }

        // Verify zero false negatives
        for hash in &inserted_set {
            assert!(large_filter.contains(hash));
        }

        // BloomFilter type alias test
        let mut alias_filter: BloomFilter = BloomFilter::new_with_defaults(10);
        alias_filter.insert(&h1);
        assert!(alias_filter.contains(&h1));

        // Test false positive rate on 5000 distinct items
        let mut false_positives = 0;
        let test_count = 5000;
        for i in 0..test_count {
            let probe_hash = StoreHash::new_unchecked(format!("z{:031}", i));
            if !inserted_set.contains(&probe_hash) && large_filter.contains(&probe_hash) {
                false_positives += 1;
            }
        }

        let fpr = (false_positives as f64) / (test_count as f64);
        // Design specifies ~1% false positive rate (allow tolerance up to 2.5% in probabilistic sample)
        assert!(
            fpr < 0.025,
            "False positive rate too high: {} (fp: {})",
            fpr,
            false_positives
        );
    }

    #[test]
    fn test_merkle_tree_and_diffing() {
        let mut entries1 = HashMap::new();
        let h1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        entries1.insert(
            h1.clone(),
            IndexEntry {
                name: "pkg1".to_string(),
                nar_size: 500,
                nar_digest: NarDigest::new_unchecked("sha256:digest1"),
                ..Default::default()
            },
        );

        let hash_a = compute_shard_merkle_hash(&entries1);
        let hash_b = compute_shard_merkle_hash(&entries1);
        assert_eq!(hash_a, hash_b, "Merkle hash must be deterministic");

        let empty_entries = HashMap::new();
        assert_eq!(
            compute_shard_merkle_hash(&empty_entries),
            EMPTY_SHARD_MERKLE_HASH
        );

        let mut shards1 = Vec::with_capacity(NUM_SHARDS);
        let mut shards2 = Vec::with_capacity(NUM_SHARDS);
        for id in 0..NUM_SHARDS {
            shards1.push(ShardDescriptor::empty(id as u16));
            shards2.push(ShardDescriptor::empty(id as u16));
        }

        let root1 = compute_merkle_root(&shards1);
        let root2 = compute_merkle_root(&shards2);
        assert_eq!(root1, root2);
        assert!(diff_shard_descriptors(&shards1, &shards2).is_empty());

        // Modify shard 42 in shards2
        shards2[42].merkle_hash = "sha256:changed42".to_string();
        shards2[42].entry_count = 1;

        let root2_changed = compute_merkle_root(&shards2);
        assert_ne!(root1, root2_changed);

        let diff = diff_shard_descriptors(&shards1, &shards2);
        assert_eq!(diff, vec![42]);
    }

    #[test]
    fn test_schema_v5_structures_and_serialization() {
        let mut root_index =
            ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "owner/repo", "ghcr.io");
        assert_eq!(root_index.version, SCHEMA_VERSION_V5);
        assert_eq!(root_index.shards.len(), NUM_SHARDS);
        assert_eq!(root_index.total_entries(), 0);

        let h1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        root_index.gc_roots.push(h1.clone());

        let target_shard = root_index.find_shard_mut(&h1).expect("Shard exists");
        target_shard.entry_count = 1;
        target_shard.blob_digest = "sha256:shard_blob_digest".to_string();
        target_shard.compressed_size = 1024;
        target_shard.uncompressed_size = 4096;
        target_shard.merkle_hash = "sha256:new_merkle".to_string();
        root_index.recalculate_merkle_root();

        let json = serde_json::to_string(&root_index).expect("Serialize root index");
        let deserialized: ShardedArchCacheIndexData =
            serde_json::from_str(&json).expect("Deserialize root index");

        assert_eq!(deserialized.version, SCHEMA_VERSION_V5);
        assert_eq!(deserialized.system, SystemArch::X86_64Linux);
        assert_eq!(deserialized.total_entries(), 1);
        assert_eq!(deserialized.shards.len(), NUM_SHARDS);
        assert_eq!(deserialized.gc_roots, vec![h1.clone()]);
        assert_eq!(deserialized.merkle_root, root_index.merkle_root);

        // ShardDataPayload
        let mut payload = ShardDataPayload::new(838);
        payload.entries.insert(
            h1.clone(),
            IndexEntry {
                name: "pkg1".to_string(),
                nar_size: 100,
                ..Default::default()
            },
        );
        let payload_json = serde_json::to_string(&payload).unwrap();
        let loaded_payload: ShardDataPayload = serde_json::from_str(&payload_json).unwrap();
        assert_eq!(loaded_payload.version, SCHEMA_VERSION_V5);
        assert_eq!(loaded_payload.shard_id, 838);
        assert_eq!(loaded_payload.len(), 1);

        // DeltaPatchData
        let mut delta = DeltaPatchData::new(12345, "job-build", SystemArch::X86_64Linux);
        delta.new_entries.insert(h1.clone(), IndexEntry::default());
        delta.active_gc_roots.push(h1.clone());

        let delta_json = serde_json::to_string(&delta).unwrap();
        let loaded_delta: DeltaPatchData = serde_json::from_str(&delta_json).unwrap();
        assert_eq!(loaded_delta.run_id, 12345);
        assert_eq!(loaded_delta.new_entries.len(), 1);
        let partitioned = loaded_delta.partition_by_shard();
        assert_eq!(partitioned.get(&838).unwrap().len(), 1);

        // BloomFilterManifest
        let bf_manifest = BloomFilterManifest::new(100, 1024, 7, "sha256:bloom_blob", 120);
        let bf_json = serde_json::to_string(&bf_manifest).unwrap();
        let loaded_bf: BloomFilterManifest = serde_json::from_str(&bf_json).unwrap();
        assert_eq!(loaded_bf.num_entries, 100);
        assert!(!loaded_bf.is_empty());

        // JobSummaryMetadata
        let job_summary = JobSummaryMetadata {
            job_id: "build-1".to_string(),
            system: SystemArch::X86_64Linux,
            uploaded_blobs: 2,
            uploaded_bytes: 2048,
            timestamp: "2026-08-30T10:00:00Z".to_string(),
        };
        let js_json = serde_json::to_string(&job_summary).unwrap();
        let loaded_js: JobSummaryMetadata = serde_json::from_str(&js_json).unwrap();
        assert_eq!(loaded_js.job_id, "build-1");

        // BuildReceipt
        let receipt = BuildReceipt::new(
            SystemArch::X86_64Linux,
            "owner/repo".to_string(),
            "2026-08-29T10:00:00Z".to_string(),
            Some("pubkey".to_string()),
            HashMap::new(),
            vec![h1],
            BuildStats {
                discovered_outputs: 1,
                built_paths: 1,
                substituted_paths: 0,
                uploaded_blobs: 1,
                total_bytes_uploaded: 500,
            },
        )
        .with_run_info(Some(12345), Some("job1".to_string()));
        assert_eq!(receipt.version, RECEIPT_VERSION);
        assert_eq!(CACHE_INDEX_VERSION, SCHEMA_VERSION_V5);
    }

    #[test]
    fn test_gc_evaluation() {
        let root_app = StoreHash::new_unchecked("hash0000000000000000000000000app");
        let shared_dep = StoreHash::new_unchecked("hash0000000000000000000000000dep");
        let sub_dep = StoreHash::new_unchecked("hash0000000000000000000000000sub");
        let orphan_old = StoreHash::new_unchecked("hash0000000000000000000000000old");
        let orphan_new = StoreHash::new_unchecked("hash0000000000000000000000000new");

        let mut entries = HashMap::new();
        let now = Utc::now();
        let sixty_days_ago = (now - Duration::days(60)).to_rfc3339();
        let five_days_ago = (now - Duration::days(5)).to_rfc3339();

        entries.insert(
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

        entries.insert(
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

        entries.insert(
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

        entries.insert(
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

        entries.insert(
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

        let gc_roots = vec![root_app.clone()];
        let cutoff = now - Duration::days(30);

        let result = evaluate_gc(&entries, &gc_roots, &cutoff);
        assert_eq!(result.deleted_hashes, vec![orphan_old]);
        assert_eq!(result.kept_entries.len(), 4);
        assert!(result.kept_entries.contains_key(&root_app));
        assert!(result.kept_entries.contains_key(&shared_dep));
        assert!(result.kept_entries.contains_key(&sub_dep));
        assert!(result.kept_entries.contains_key(&orphan_new));
        assert_eq!(result.reachable_roots.len(), 3);

        let mut multi_roots = HashMap::new();
        multi_roots.insert(SystemArch::X86_64Linux, vec![root_app.clone()]);
        let multi_result = evaluate_multi_arch_gc(&entries, &multi_roots, &cutoff);
        assert_eq!(multi_result.deleted_hashes, result.deleted_hashes);
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
    fn test_evaluate_cache_query_and_selector() {
        let hash1 = StoreHash::new_unchecked("hash1111111111111111111111111111");
        let hash2 = StoreHash::new_unchecked("hash2222222222222222222222222222");

        let mut entries = HashMap::new();
        entries.insert(
            hash1.clone(),
            IndexEntry {
                name: "rust-1.80".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-rust-1.80", hash1),
                    nar_basename: "rust-1.80.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-rust"),
                nar_size: 1000,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );
        entries.insert(
            hash2.clone(),
            IndexEntry {
                name: "llvm-18".to_string(),
                system: Some(SystemArch::Aarch64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-llvm-18", hash2),
                    nar_basename: "llvm-18.nar.xz".to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_unchecked("sha256:blob-llvm"),
                nar_size: 2000,
                added: "2026-08-29T10:00:00Z".to_string(),
                origin_job: None,
            },
        );

        let selector = CacheSelector::filtered(FilterPredicates {
            patterns: vec!["*rust*".to_string()],
            ..Default::default()
        });
        let query_res: CacheQueryResult =
            evaluate_cache_query(&entries, &HashMap::new(), &selector);
        assert_eq!(query_res.matched_entries.len(), 1);
        assert_eq!(query_res.unmatched_entries.len(), 1);
        assert_eq!(query_res.matched_bytes, 1000);
        assert_eq!(query_res.unmatched_bytes, 2000);
        assert_eq!(query_res.final_matched_hashes, vec![hash1.clone()]);

        let arch_query_res =
            evaluate_arch_cache_query(&entries, &[], SystemArch::X86_64Linux, &selector);
        assert_eq!(arch_query_res.matched_entries.len(), 1);
    }

    #[test]
    fn test_purge_all_clears_everything() {
        let hash1 = StoreHash::new_unchecked("hash1111111111111111111111111111");
        let hash2 = StoreHash::new_unchecked("hash2222222222222222222222222222");

        let mut entries = HashMap::new();
        entries.insert(
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
        entries.insert(
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

        let mut gc_roots = HashMap::new();
        gc_roots.insert(SystemArch::X86_64Linux, vec![hash1.clone()]);
        gc_roots.insert(SystemArch::Aarch64Linux, vec![hash2.clone()]);

        let selector = CacheSelector::all(HashSet::new());

        let result = evaluate_cache_purge(&entries, &gc_roots, &selector);
        assert!(result.kept_entries.is_empty());
        assert_eq!(result.purged_entries.len(), 2);
        assert_eq!(result.purged_hashes.len(), 2);
        assert_eq!(result.purged_nar_digests.len(), 2);
        assert_eq!(result.estimated_freed_bytes, 3000);
        assert!(result.updated_gc_roots.is_empty());

        let arch_result =
            evaluate_arch_cache_purge(&entries, &[hash1], SystemArch::X86_64Linux, &selector);
        assert_eq!(arch_result.purged_entries.len(), 2);
    }

    #[test]
    fn test_purge_exact_preserves_dependents() {
        let app = StoreHash::new_unchecked("hash0000000000000000000000000app");
        let lib_a = StoreHash::new_unchecked("hash000000000000000000000000liba");
        let core = StoreHash::new_unchecked("hash000000000000000000000000core");

        let mut entries = HashMap::new();
        let mut gc_roots = HashMap::new();
        gc_roots.insert(SystemArch::X86_64Linux, vec![app.clone()]);

        entries.insert(
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

        entries.insert(
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

        entries.insert(
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

        let selector = CacheSelector::filtered(FilterPredicates {
            store_hashes: hashes,
            ..Default::default()
        })
        .with_cascade(CascadeMode::Exact);

        let result = evaluate_cache_purge(&entries, &gc_roots, &selector);
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

        let mut entries = HashMap::new();
        let mut gc_roots = HashMap::new();
        gc_roots.insert(SystemArch::X86_64Linux, vec![app.clone()]);

        entries.insert(
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

        entries.insert(
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

        entries.insert(
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

        let selector = CacheSelector::filtered(FilterPredicates {
            store_hashes: hashes,
            ..Default::default()
        })
        .with_cascade(CascadeMode::Dependents);

        let result = evaluate_cache_purge(&entries, &gc_roots, &selector);
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

        let mut entries = HashMap::new();
        entries.insert(
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
        entries.insert(
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
        entries.insert(
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
        let selector_transitive = CacheSelector::filtered(FilterPredicates {
            store_hashes: hashes.clone(),
            ..Default::default()
        })
        .with_cascade(CascadeMode::Transitive);
        let res_transitive = evaluate_cache_purge(&entries, &HashMap::new(), &selector_transitive);
        assert_eq!(res_transitive.purged_entries.len(), 2);
        assert!(res_transitive.purged_entries.contains_key(&lib_a));
        assert!(res_transitive.purged_entries.contains_key(&core));
        assert!(res_transitive.kept_entries.contains_key(&app));

        // FullTree: app + lib_a + core
        let selector_full = CacheSelector::filtered(FilterPredicates {
            store_hashes: hashes,
            ..Default::default()
        })
        .with_cascade(CascadeMode::FullTree);
        let res_full = evaluate_cache_purge(&entries, &HashMap::new(), &selector_full);
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

        let mut entries = HashMap::new();
        entries.insert(
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

        entries.insert(
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

        entries.insert(
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
        let selector_pat = CacheSelector::filtered(FilterPredicates {
            patterns: vec!["*chromium*".to_string()],
            ..Default::default()
        })
        .with_cascade(CascadeMode::Exact);
        let res_pat = evaluate_cache_purge(&entries, &HashMap::new(), &selector_pat);
        assert_eq!(res_pat.purged_hashes, vec![hash_chromium.clone()]);

        // Size filter: MinBytes(100MB)
        let selector_size = CacheSelector::filtered(FilterPredicates {
            size_filter: Some(SizeFilter::MinBytes(100_000_000)),
            ..Default::default()
        })
        .with_cascade(CascadeMode::Exact);
        let res_size = evaluate_cache_purge(&entries, &HashMap::new(), &selector_size);
        assert_eq!(res_size.purged_hashes, vec![hash_chromium.clone()]);

        // Time filter: Before 2026-08-01
        let selector_time = CacheSelector::filtered(FilterPredicates {
            time_filter: Some(TimeFilter::Before(
                DateTime::parse_from_rfc3339("2026-08-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )),
            ..Default::default()
        })
        .with_cascade(CascadeMode::Exact);
        let res_time = evaluate_cache_purge(&entries, &HashMap::new(), &selector_time);
        assert_eq!(res_time.purged_hashes, vec![hash_old.clone()]);

        // System filter
        let mut sys_filter = HashSet::new();
        sys_filter.insert(SystemArch::Aarch64Linux);
        let selector_sys = CacheSelector::filtered(FilterPredicates {
            systems: sys_filter,
            patterns: vec!["*lib*".to_string()],
            ..Default::default()
        })
        .with_cascade(CascadeMode::Exact);
        let res_sys = evaluate_cache_purge(&entries, &HashMap::new(), &selector_sys);
        assert_eq!(res_sys.purged_hashes, vec![hash_old.clone()]);
    }

    #[test]
    fn test_purge_gc_roots_resynchronization() {
        let root_x86 = StoreHash::new_unchecked("000000000000000000000000000root1");
        let root_arm = StoreHash::new_unchecked("000000000000000000000000000root2");
        let dep_x86 = StoreHash::new_unchecked("0000000000000000000000000000dep1");

        let mut entries = HashMap::new();
        let mut gc_roots = HashMap::new();
        gc_roots.insert(SystemArch::X86_64Linux, vec![root_x86.clone()]);
        gc_roots.insert(SystemArch::Aarch64Linux, vec![root_arm.clone()]);

        entries.insert(
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

        entries.insert(
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

        entries.insert(
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
        let selector = CacheSelector::filtered(FilterPredicates {
            store_hashes: hashes,
            ..Default::default()
        })
        .with_cascade(CascadeMode::Exact);

        let result = evaluate_cache_purge(&entries, &gc_roots, &selector);
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

        let mut entries = HashMap::new();
        let mut gc_roots = HashMap::new();
        gc_roots.insert(SystemArch::X86_64Linux, vec![root_app.clone()]);

        let sixty_days_ago = (Utc::now() - Duration::days(60)).to_rfc3339();

        entries.insert(
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

        entries.insert(
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

        entries.insert(
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

        // When protect_gc_roots is true, root_app and shared_dep must be protected even if older_than / patterns match
        let cutoff = Utc::now() - Duration::days(30);
        let selector = CacheSelector::filtered(FilterPredicates {
            time_filter: Some(TimeFilter::Before(cutoff)),
            ..Default::default()
        })
        .with_protect_gc_roots(true)
        .with_cascade(CascadeMode::Exact);

        let result = evaluate_cache_purge(&entries, &gc_roots, &selector);
        assert_eq!(result.purged_hashes, vec![orphan_old]);
        assert_eq!(result.kept_entries.len(), 2);
        assert!(result.kept_entries.contains_key(&root_app));
        assert!(result.kept_entries.contains_key(&shared_dep));
        assert_eq!(
            result.updated_gc_roots.get(&SystemArch::X86_64Linux),
            Some(&vec![root_app])
        );
    }

    #[test]
    fn test_selector_predicates_and_describe() {
        let mut hashes = HashSet::new();
        let hash = StoreHash::new_unchecked("0000000000000000000000000000pkg1");
        hashes.insert(hash);

        let mut systems = HashSet::new();
        systems.insert(SystemArch::X86_64Linux);

        let predicates = FilterPredicates {
            store_hashes: hashes,
            patterns: vec!["*foo*".to_string()],
            systems: systems.clone(),
            time_filter: Some(TimeFilter::Before(
                DateTime::parse_from_rfc3339("2026-08-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )),
            size_filter: Some(SizeFilter::MinBytes(1024)),
            origin_jobs: HashSet::from(["job-ci".to_string()]),
            origin_runs: HashSet::from([9999]),
        };

        assert!(!predicates.is_empty());
        assert!(predicates.has_item_filters());

        let selector = CacheSelector::filtered(predicates)
            .with_cascade(CascadeMode::Dependents)
            .with_protect_gc_roots(true);

        assert!(matches!(selector.scope, SelectionScope::Filtered(_)));

        let desc = selector.describe();
        assert!(desc.contains("hashes=[1 items]"));
        assert!(desc.contains("patterns=[*foo*]"));
        assert!(desc.contains("systems=[x86_64-linux]"));
        assert!(desc.contains("min_size=1024B"));
        assert!(desc.contains("protect_gc_roots=true"));
        assert!(desc.contains("cascade=dependents"));

        let all_sel = CacheSelector::all(systems);
        assert!(matches!(all_sel.scope, SelectionScope::All { .. }));
        let all_desc = all_sel.describe();
        assert!(all_desc.contains("all=true"));
        assert!(all_desc.contains("systems=[x86_64-linux]"));

        let none_sel = CacheSelector::none();
        assert_eq!(none_sel.scope, SelectionScope::None);
        assert_eq!(none_sel.describe(), "none=true");
    }

    #[test]
    fn test_core_error_conversions() {
        let type_err = TypeError::UnknownSystemArch {
            raw: "invalid".to_string(),
        };
        let core_type: CoreError = type_err.into();
        assert!(matches!(core_type, CoreError::Type(_)));

        let bloom_err = BloomError::ZeroHashCount(0);
        let core_bloom: CoreError = bloom_err.into();
        assert!(matches!(core_bloom, CoreError::Bloom(_)));

        let parse_err = NarInfoParseError::EmptyContent;
        let core_parse: CoreError = parse_err.into();
        assert!(matches!(core_parse, CoreError::NarInfoParse(_)));

        let json_err = CoreError::Json("test error".to_string());
        assert_eq!(
            format!("{}", json_err),
            "Serialization / Deserialization error: test error"
        );
    }
}
