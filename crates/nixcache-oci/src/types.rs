use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const CACHE_INDEX_VERSION: u32 = 2;
pub const RECEIPT_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub narinfo: String,
    pub nar_digest: String,
    pub nar_size: u64,
    pub added: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CacheIndexData {
    pub version: u32,
    pub repo: String,
    pub registry: String,
    pub image: String,
    pub generated: String,
    #[serde(default)]
    pub public_key: String,
    pub entries: HashMap<String, IndexEntry>,
    #[serde(default)]
    pub gc_roots: HashMap<String, Vec<String>>,
}

impl Default for CacheIndexData {
    fn default() -> Self {
        Self {
            version: CACHE_INDEX_VERSION,
            repo: String::new(),
            registry: String::new(),
            image: String::new(),
            generated: String::new(),
            public_key: String::new(),
            entries: HashMap::new(),
            gc_roots: HashMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub discovered_outputs: usize,
    pub built_paths: usize,
    pub uploaded_blobs: usize,
    pub total_bytes_uploaded: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BuildReceipt {
    pub version: u32,
    pub system: String,
    pub repo: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    pub new_entries: HashMap<String, IndexEntry>,
    pub active_gc_roots: Vec<String>,
    pub stats: BuildStats,
}

impl BuildReceipt {
    pub fn new(
        system: String,
        repo: String,
        timestamp: String,
        public_key: Option<String>,
        new_entries: HashMap<String, IndexEntry>,
        active_gc_roots: Vec<String>,
        stats: BuildStats,
    ) -> Self {
        Self {
            version: RECEIPT_VERSION,
            system,
            repo,
            timestamp,
            public_key,
            new_entries,
            active_gc_roots,
            stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, RECEIPT_VERSION,
    };
    use std::collections::{HashMap, HashSet};

    #[test]
    fn test_cache_index_default_and_serialization() {
        let mut index = CacheIndexData::default();
        assert_eq!(index.version, CACHE_INDEX_VERSION);
        assert_eq!(index.version, 2);

        index.repo = "owner/repo".to_string();
        index.gc_roots.insert(
            "x86_64-linux".to_string(),
            vec!["root-hash-1".to_string(), "root-hash-2".to_string()],
        );
        index.entries.insert(
            "hash1".to_string(),
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/hash1-pkg1\n".to_string(),
                nar_digest: "sha256:1111".to_string(),
                nar_size: 1024,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );

        let json = serde_json::to_string(&index).expect("serialization failed");
        let parsed: CacheIndexData = serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.repo, "owner/repo");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.gc_roots.get("x86_64-linux").unwrap().len(), 2);
        assert_eq!(
            parsed.entries.get("hash1").unwrap().system,
            Some("x86_64-linux".to_string())
        );
    }

    #[test]
    fn test_schema_v1_to_v2_migration() {
        let v1_json = r#"{
            "version": 1,
            "repo": "owner/legacy-repo",
            "registry": "ghcr.io",
            "image": "ghcr.io/owner/legacy-repo/nix-cache",
            "generated": "2026-01-01T00:00:00Z",
            "public_key": "cache-key-1:ABCD1234",
            "entries": {
                "hash_legacy_1": {
                    "name": "legacy-pkg-1",
                    "narinfo": "StorePath: /nix/store/hash_legacy_1-legacy-pkg-1\n",
                    "nar_digest": "sha256:legacy_digest",
                    "nar_size": 4096,
                    "added": "2026-01-01T00:00:00Z"
                }
            }
        }"#;

        let mut index: CacheIndexData =
            serde_json::from_str(v1_json).expect("Failed to deserialize Schema v1 index");

        assert_eq!(index.version, 1);
        assert_eq!(index.repo, "owner/legacy-repo");
        assert_eq!(index.entries.len(), 1);
        assert!(index.gc_roots.is_empty());

        let legacy_entry = index.entries.get("hash_legacy_1").unwrap();
        assert_eq!(legacy_entry.name, "legacy-pkg-1");
        assert_eq!(legacy_entry.system, None);

        // 升级至 Schema v2
        index.version = CACHE_INDEX_VERSION;
        index.gc_roots.insert(
            "x86_64-linux".to_string(),
            vec!["hash_legacy_1".to_string()],
        );

        let upgraded_json = serde_json::to_string(&index).expect("Failed to serialize v2 index");
        let reloaded: CacheIndexData =
            serde_json::from_str(&upgraded_json).expect("Failed to reload v2 index");

        assert_eq!(reloaded.version, 2);
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(
            reloaded.gc_roots.get("x86_64-linux").unwrap(),
            &vec!["hash_legacy_1".to_string()]
        );
    }

    #[test]
    fn test_receipt_merging_and_deduplication() {
        let mut index = CacheIndexData::default();

        let receipt_x86 = BuildReceipt::new(
            "x86_64-linux".to_string(),
            "owner/repo".to_string(),
            "2026-08-28T00:00:00Z".to_string(),
            Some("key:pub".to_string()),
            HashMap::from([
                (
                    "hash-shared".to_string(),
                    IndexEntry {
                        name: "shared-lib".to_string(),
                        system: Some("x86_64-linux".to_string()),
                        narinfo: "StorePath: /nix/store/hash-shared-lib\n".to_string(),
                        nar_digest: "sha256:shared-digest".to_string(),
                        nar_size: 1000,
                        added: "2026-08-28T00:00:00Z".to_string(),
                    },
                ),
                (
                    "hash-x86-app".to_string(),
                    IndexEntry {
                        name: "x86-app".to_string(),
                        system: Some("x86_64-linux".to_string()),
                        narinfo: "StorePath: /nix/store/hash-x86-app\n".to_string(),
                        nar_digest: "sha256:x86-digest".to_string(),
                        nar_size: 2000,
                        added: "2026-08-28T00:00:00Z".to_string(),
                    },
                ),
            ]),
            vec!["hash-shared".to_string(), "hash-x86-app".to_string()],
            BuildStats {
                discovered_outputs: 2,
                built_paths: 2,
                uploaded_blobs: 2,
                total_bytes_uploaded: 3000,
            },
        );

        let receipt_arm = BuildReceipt::new(
            "aarch64-linux".to_string(),
            "owner/repo".to_string(),
            "2026-08-28T01:00:00Z".to_string(),
            Some("key:pub".to_string()),
            HashMap::from([(
                "hash-arm-app".to_string(),
                IndexEntry {
                    name: "arm-app".to_string(),
                    system: Some("aarch64-linux".to_string()),
                    narinfo: "StorePath: /nix/store/hash-arm-app\n".to_string(),
                    nar_digest: "sha256:arm-digest".to_string(),
                    nar_size: 2500,
                    added: "2026-08-28T01:00:00Z".to_string(),
                },
            )]),
            vec!["hash-arm-app".to_string(), "hash-arm-app".to_string()], // 包含重复项
            BuildStats {
                discovered_outputs: 1,
                built_paths: 1,
                uploaded_blobs: 1,
                total_bytes_uploaded: 2500,
            },
        );

        let receipts = [receipt_x86, receipt_arm];
        for r in &receipts {
            index.entries.extend(r.new_entries.clone());
            let roots = index.gc_roots.entry(r.system.clone()).or_default();
            let mut set: HashSet<String> = roots.iter().cloned().collect();
            set.extend(r.active_gc_roots.clone());
            let mut sorted: Vec<String> = set.into_iter().collect();
            sorted.sort();
            *roots = sorted;
        }

        assert_eq!(index.entries.len(), 3);
        assert!(index.entries.contains_key("hash-shared"));
        assert!(index.entries.contains_key("hash-x86-app"));
        assert!(index.entries.contains_key("hash-arm-app"));

        let x86_roots = index.gc_roots.get("x86_64-linux").unwrap();
        assert_eq!(
            x86_roots,
            &vec!["hash-shared".to_string(), "hash-x86-app".to_string()]
        );

        let arm_roots = index.gc_roots.get("aarch64-linux").unwrap();
        assert_eq!(arm_roots, &vec!["hash-arm-app".to_string()]);
    }

    #[test]
    fn test_build_receipt_creation_and_serialization() {
        let mut new_entries = HashMap::new();
        new_entries.insert(
            "hash-arm".to_string(),
            IndexEntry {
                name: "arm-pkg".to_string(),
                system: Some("aarch64-linux".to_string()),
                narinfo: "StorePath: /nix/store/hash-arm-arm-pkg\n".to_string(),
                nar_digest: "sha256:arm-digest".to_string(),
                nar_size: 2048,
                added: "2026-08-28T12:00:00Z".to_string(),
            },
        );

        let receipt = BuildReceipt::new(
            "aarch64-linux".to_string(),
            "owner/repo".to_string(),
            "2026-08-28T12:00:00Z".to_string(),
            Some("key:pub".to_string()),
            new_entries,
            vec!["root-arm-1".to_string()],
            BuildStats {
                discovered_outputs: 2,
                built_paths: 2,
                uploaded_blobs: 1,
                total_bytes_uploaded: 2048,
            },
        );

        assert_eq!(receipt.version, RECEIPT_VERSION);
        assert_eq!(receipt.system, "aarch64-linux");
        assert_eq!(receipt.stats.uploaded_blobs, 1);

        let json = serde_json::to_string(&receipt).expect("receipt serialize failed");
        let parsed: BuildReceipt = serde_json::from_str(&json).expect("receipt deserialize failed");

        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.system, "aarch64-linux");
        assert_eq!(parsed.active_gc_roots, vec!["root-arm-1"]);
        assert_eq!(parsed.public_key, Some("key:pub".to_string()));
    }
}
