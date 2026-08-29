use crate::{error::BuilderError, summary::write_promote_step_summary};
use chrono::Utc;
use nixcache_core::{
    BuildReceipt, CACHE_INDEX_VERSION, CacheIndexData, IndexEntry, StoreHash, SystemArch,
};
use nixcache_oci::OciClient;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use tokio::fs;
use tracing::{info, warn};

/// Promote: 汇聚会话清单 (run-<run_id>) 与 Receipt，原子 CAS 发布生产 cache-index
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
        "Promoting cache to tag '{}' for repo: {}/{} (Run ID: {:?})",
        target_tag, registry, repo, run_id
    );

    let oci = OciClient::new(registry, repo, github_token, true);

    // 1. 准备待合并的数据集
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
                                        .entry(receipt.system.clone())
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
                            .entry(receipt.system.clone())
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
        info!("No session manifest or receipts found to promote. Pushing current baseline state.");
    }

    // 3. 基于 OCI CAS 乐观并发重试机制合并并发布全局 cache-index
    let repo_str = repo.to_string();
    let registry_str = registry.to_string();
    let image_str = format!("{}/{}/nix-cache", registry, repo);

    oci.update_manifest_cas::<CacheIndexData, _>(target_tag, 5, |existing_opt| {
        let mut index = existing_opt.unwrap_or_default();
        index.version = CACHE_INDEX_VERSION;
        index.repo = repo_str.clone();
        index.registry = registry_str.clone();
        index.image = image_str.clone();
        index.generated = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        index.last_promoted_run = run_id;

        if let Some(pk) = session_pub_key.as_deref().or(receipt_pub_key.as_deref()) {
            index.public_key = pk.to_string();
        }

        // 合并 session 条目
        index.entries.extend(session_entries.clone());
        for (sys, roots) in &session_roots {
            let system_roots = index.gc_roots.entry(sys.clone()).or_default();
            let mut set: HashSet<StoreHash> = system_roots.iter().cloned().collect();
            set.extend(roots.iter().cloned());
            let mut sorted: Vec<StoreHash> = set.into_iter().collect();
            sorted.sort();
            *system_roots = sorted;
        }

        // 合并 receipt 条目
        index.entries.extend(receipt_entries.clone());
        for (sys, roots) in &receipt_roots {
            let system_roots = index.gc_roots.entry(sys.clone()).or_default();
            let mut set: HashSet<StoreHash> = system_roots.iter().cloned().collect();
            set.extend(roots.iter().cloned());
            let mut sorted: Vec<StoreHash> = set.into_iter().collect();
            sorted.sort();
            *system_roots = sorted;
        }

        Ok(index)
    })
    .await?;

    info!(
        "Promote complete! Global cache-index updated atomically via CAS for tag '{}'.",
        target_tag
    );

    // 4. 清理会话标签
    if cleanup_session && let Some(rid) = run_id {
        let tag = format!("run-{}", rid);
        if let Ok(deleted) = oci.delete_manifest(&tag).await
            && deleted
        {
            info!("Cleaned up session tag {}", tag);
        }
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
