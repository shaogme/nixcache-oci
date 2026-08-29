use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const CACHE_INDEX_VERSION: u32 = 3;
pub const RUN_SESSION_VERSION: u32 = 3;
pub const RECEIPT_VERSION: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub narinfo: String,
    pub nar_digest: String,
    pub nar_size: u64,
    pub added: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_job: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct JobSummaryMetadata {
    pub job_id: String,
    pub system: String,
    pub uploaded_blobs: usize,
    pub uploaded_bytes: u64,
    pub timestamp: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunSessionManifest {
    pub version: u32,
    pub run_id: u64,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub ref_name: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    pub entries: HashMap<String, IndexEntry>,
    #[serde(default, deserialize_with = "deserialize_gc_roots")]
    pub gc_roots: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub completed_jobs: Vec<JobSummaryMetadata>,
}

impl Default for RunSessionManifest {
    fn default() -> Self {
        Self {
            version: RUN_SESSION_VERSION,
            run_id: 0,
            head_sha: String::new(),
            ref_name: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            public_key: None,
            entries: HashMap::new(),
            gc_roots: HashMap::new(),
            completed_jobs: Vec::new(),
        }
    }
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
    #[serde(default, deserialize_with = "deserialize_gc_roots")]
    pub gc_roots: HashMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_promoted_run: Option<u64>,
}

pub fn deserialize_gc_roots<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;
    let val = Value::deserialize(deserializer)?;
    match val {
        Value::Object(map) => {
            let mut result = HashMap::new();
            for (k, v) in map {
                if let Value::Array(arr) = v {
                    let strings: Vec<String> = arr
                        .into_iter()
                        .filter_map(|item| match item {
                            Value::String(s) => Some(s),
                            _ => None,
                        })
                        .collect();
                    result.insert(k, strings);
                }
            }
            Ok(result)
        }
        Value::Array(arr) => {
            let strings: Vec<String> = arr
                .into_iter()
                .filter_map(|item| match item {
                    Value::String(s) => Some(s),
                    _ => None,
                })
                .collect();
            let mut result = HashMap::new();
            if !strings.is_empty() {
                result.insert("default".to_string(), strings);
            }
            Ok(result)
        }
        _ => Ok(HashMap::new()),
    }
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
            last_promoted_run: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    #[serde(default)]
    pub discovered_outputs: usize,
    #[serde(default)]
    pub built_paths: usize,
    #[serde(default)]
    pub substituted_paths: usize,
    #[serde(default)]
    pub uploaded_blobs: usize,
    #[serde(default)]
    pub total_bytes_uploaded: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BuildReceipt {
    pub version: u32,
    pub system: String,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
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
            run_id: None,
            job_id: None,
            timestamp,
            public_key,
            new_entries,
            active_gc_roots,
            stats,
        }
    }

    pub fn with_run_info(mut self, run_id: Option<u64>, job_id: Option<String>) -> Self {
        self.run_id = run_id;
        self.job_id = job_id;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BuildReceipt, BuildStats, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry,
        JobSummaryMetadata, RECEIPT_VERSION, RUN_SESSION_VERSION, RunSessionManifest,
    };
    use std::collections::{HashMap, HashSet};

    #[test]
    fn test_cache_index_default_and_serialization() {
        let mut index = CacheIndexData::default();
        assert_eq!(index.version, CACHE_INDEX_VERSION);
        assert_eq!(index.version, 3);

        index.repo = "owner/repo".to_string();
        index.last_promoted_run = Some(123456789);
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
                origin_job: Some("job:vm-tests".to_string()),
            },
        );

        let json = serde_json::to_string(&index).expect("serialization failed");
        let parsed: CacheIndexData = serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.repo, "owner/repo");
        assert_eq!(parsed.last_promoted_run, Some(123456789));
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.gc_roots.get("x86_64-linux").unwrap().len(), 2);
        let entry = parsed.entries.get("hash1").unwrap();
        assert_eq!(entry.system, Some("x86_64-linux".to_string()));
        assert_eq!(entry.origin_job, Some("job:vm-tests".to_string()));
    }

    #[test]
    fn test_run_session_manifest_serialization() {
        let mut session = RunSessionManifest {
            run_id: 987654321,
            head_sha: "abc1234def5678".to_string(),
            ref_name: "refs/pull/42/merge".to_string(),
            created_at: "2026-08-29T10:00:00Z".to_string(),
            updated_at: "2026-08-29T10:05:00Z".to_string(),
            public_key: Some("cache-key-pub:ABCD".to_string()),
            ..Default::default()
        };
        assert_eq!(session.version, RUN_SESSION_VERSION);
        assert_eq!(session.version, 3);

        session.entries.insert(
            "hash-session-1".to_string(),
            IndexEntry {
                name: "session-pkg-1".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/hash-session-1-session-pkg-1\n".to_string(),
                nar_digest: "sha256:sessiondigest1".to_string(),
                nar_size: 2048,
                added: "2026-08-29T10:05:00Z".to_string(),
                origin_job: Some("job:nixos-vm-tests".to_string()),
            },
        );
        session.gc_roots.insert(
            "x86_64-linux".to_string(),
            vec!["hash-session-1".to_string()],
        );
        session.completed_jobs.push(JobSummaryMetadata {
            job_id: "nixos-vm-tests".to_string(),
            system: "x86_64-linux".to_string(),
            uploaded_blobs: 1,
            uploaded_bytes: 2048,
            timestamp: "2026-08-29T10:05:00Z".to_string(),
        });

        let json = serde_json::to_string(&session).expect("session serialization failed");
        let parsed: RunSessionManifest =
            serde_json::from_str(&json).expect("session deserialization failed");

        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.run_id, 987654321);
        assert_eq!(parsed.head_sha, "abc1234def5678");
        assert_eq!(parsed.ref_name, "refs/pull/42/merge");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.completed_jobs.len(), 1);
        assert_eq!(parsed.completed_jobs[0].job_id, "nixos-vm-tests");
    }

    #[test]
    fn test_schema_v1_to_v3_migration() {
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
        assert_eq!(legacy_entry.origin_job, None);

        // 升级至 Schema v3
        index.version = CACHE_INDEX_VERSION;
        index.gc_roots.insert(
            "x86_64-linux".to_string(),
            vec!["hash_legacy_1".to_string()],
        );

        let upgraded_json = serde_json::to_string(&index).expect("Failed to serialize v3 index");
        let reloaded: CacheIndexData =
            serde_json::from_str(&upgraded_json).expect("Failed to reload v3 index");

        assert_eq!(reloaded.version, 3);
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
                        origin_job: None,
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
                        origin_job: None,
                    },
                ),
            ]),
            vec!["hash-shared".to_string(), "hash-x86-app".to_string()],
            BuildStats {
                discovered_outputs: 2,
                built_paths: 2,
                substituted_paths: 0,
                uploaded_blobs: 2,
                total_bytes_uploaded: 3000,
            },
        )
        .with_run_info(Some(12345), Some("build-x86".to_string()));

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
                    origin_job: None,
                },
            )]),
            vec!["hash-arm-app".to_string(), "hash-arm-app".to_string()], // 包含重复项
            BuildStats {
                discovered_outputs: 1,
                built_paths: 1,
                substituted_paths: 0,
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
                origin_job: Some("job:build-arm".to_string()),
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
                substituted_paths: 1,
                uploaded_blobs: 1,
                total_bytes_uploaded: 2048,
            },
        )
        .with_run_info(Some(123456), Some("job:build-arm".to_string()));

        assert_eq!(receipt.version, RECEIPT_VERSION);
        assert_eq!(receipt.version, 3);
        assert_eq!(receipt.system, "aarch64-linux");
        assert_eq!(receipt.run_id, Some(123456));
        assert_eq!(receipt.job_id, Some("job:build-arm".to_string()));
        assert_eq!(receipt.stats.uploaded_blobs, 1);
        assert_eq!(receipt.stats.substituted_paths, 1);

        let json = serde_json::to_string(&receipt).expect("receipt serialize failed");
        let parsed: BuildReceipt = serde_json::from_str(&json).expect("receipt deserialize failed");

        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.system, "aarch64-linux");
        assert_eq!(parsed.active_gc_roots, vec!["root-arm-1"]);
        assert_eq!(parsed.public_key, Some("key:pub".to_string()));
        assert_eq!(parsed.run_id, Some(123456));
    }
}
