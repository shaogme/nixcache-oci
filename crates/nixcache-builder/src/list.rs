use crate::{
    error::BuilderError, nix::resolve_flake_output_hashes, summary::write_list_step_summary,
};
use futures_util::future::join_all;
use nixcache_cli::{ListArgs, OutputFormat};
use nixcache_core::{
    CacheQueryResult, CascadeMode, IndexEntry, SortBy, SortOrder, StoreHash, SystemArch,
    evaluate_cache_query,
};
use nixcache_oci::OciArtifactManifest;
use nixcache_oci_backend::create_tokio_reqwest_client;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    env,
};
use tokio::fs;
use tracing::{info, warn};

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.2} TiB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// 架构统计明细
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchStat {
    pub count: usize,
    pub bytes: u64,
    pub roots_count: usize,
}

/// 单个列表条目传输对象
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItemDto {
    pub hash: String,
    pub name: String,
    pub system: String,
    pub store_path: String,
    pub nar_basename: String,
    pub nar_size: u64,
    pub nar_size_human: String,
    pub added: String,
    pub reason: String,
    pub references: Vec<String>,
    pub nar_digest: String,
}

/// 综合列表查询摘要报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheListSummaryReport {
    pub target_tag: String,
    pub registry: String,
    pub repo: String,
    pub total_entries: usize,
    pub total_bytes: u64,
    pub matched_entries: usize,
    pub matched_bytes: u64,
    pub arch_breakdown: HashMap<String, ArchStat>,
    pub items: Vec<ListItemDto>,
}

fn build_selector_description(args: &ListArgs) -> String {
    let mut parts = Vec::new();
    if args.selector.resolve_all() {
        parts.push("all=true".to_string());
    }
    let hashes = args.selector.resolve_hashes();
    if !hashes.is_empty() {
        parts.push(format!("hashes=[{} items]", hashes.len()));
    }
    let pats = args.selector.resolve_patterns();
    if !pats.is_empty() {
        parts.push(format!("patterns=[{}]", pats.join(", ")));
    }
    let sys = args.selector.resolve_systems();
    if !sys.is_empty() {
        let sys_str: Vec<_> = sys.iter().map(|s| s.as_str()).collect();
        parts.push(format!("systems=[{}]", sys_str.join(", ")));
    }
    if let Some(older) = args.selector.resolve_older_than() {
        parts.push(format!("older_than={}", older.to_rfc3339()));
    }
    if let Some(newer) = args.selector.resolve_newer_than() {
        parts.push(format!("newer_than={}", newer.to_rfc3339()));
    }
    if let Some(min) = args.selector.resolve_min_size() {
        parts.push(format!("min_size={}", format_bytes(min)));
    }
    if let Some(max) = args.selector.resolve_max_size() {
        parts.push(format!("max_size={}", format_bytes(max)));
    }
    if args.selector.resolve_protect_gc_roots() {
        parts.push("protect_gc_roots=true".to_string());
    }

    if parts.is_empty() {
        "Default Scope (Exact Match / All)".to_string()
    } else {
        parts.join(", ")
    }
}

pub fn format_table(
    report: &CacheListSummaryReport,
    displayed_items: &[ListItemDto],
    details: bool,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n📦 NixCache Build Cache Inspector: {}/{} [Tag: {}]\n",
        report.registry, report.repo, report.target_tag
    ));
    out.push_str(&format!(
        "📊 Total: {} entries ({}) | Matched: {} entries ({})\n",
        report.total_entries,
        format_bytes(report.total_bytes),
        report.matched_entries,
        format_bytes(report.matched_bytes)
    ));
    out.push_str("─".repeat(88).as_str());
    out.push('\n');

    if displayed_items.is_empty() {
        out.push_str("  No matching cache entries found.\n");
    } else {
        out.push_str(&format!(
            "{:<4} {:<34} {:<16} {:<10} {:<20} {}\n",
            "#", "Store Hash / Name", "Arch", "Size", "Added At", "Match Reason"
        ));
        out.push_str("─".repeat(88).as_str());
        out.push('\n');

        for (idx, item) in displayed_items.iter().enumerate() {
            let name_display = if item.name.len() > 32 {
                format!("{}...", &item.name[..29])
            } else if item.name.is_empty() {
                item.hash.clone()
            } else {
                item.name.clone()
            };

            let added_display = if item.added.len() >= 19 {
                &item.added[..19]
            } else {
                &item.added
            };

            out.push_str(&format!(
                "{:<4} {:<34} {:<16} {:<10} {:<20} {}\n",
                idx + 1,
                name_display,
                item.system,
                item.nar_size_human,
                added_display,
                item.reason
            ));

            if details {
                out.push_str(&format!("     └─ Path:   {}\n", item.store_path));
                out.push_str(&format!("     └─ Digest: {}\n", item.nar_digest));
                if !item.references.is_empty() {
                    out.push_str(&format!(
                        "     └─ References ({}): {}\n",
                        item.references.len(),
                        item.references.join(", ")
                    ));
                }
            }
        }
    }

    out.push_str("─".repeat(88).as_str());
    out.push('\n');

    if !report.arch_breakdown.is_empty() {
        out.push_str("🖥️  Architecture Breakdown:\n");
        let mut sorted_arch: Vec<_> = report.arch_breakdown.iter().collect();
        sorted_arch.sort_by_key(|(k, _)| (*k).clone());
        for (arch, stat) in sorted_arch {
            out.push_str(&format!(
                "   • {:<16}: {:>4} entries | {:>10} | {:>3} active GC roots\n",
                arch,
                stat.count,
                format_bytes(stat.bytes),
                stat.roots_count
            ));
        }
    }

    out
}

pub fn format_summary(report: &CacheListSummaryReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Cache Summary [{} / tag: {}]:\n",
        report.repo, report.target_tag
    ));
    out.push_str(&format!("- Total Entries: {}\n", report.total_entries));
    out.push_str(&format!(
        "- Total Volume: {} ({} bytes)\n",
        format_bytes(report.total_bytes),
        report.total_bytes
    ));
    out.push_str(&format!("- Matched Entries: {}\n", report.matched_entries));
    out.push_str(&format!(
        "- Matched Volume: {} ({} bytes)\n",
        format_bytes(report.matched_bytes),
        report.matched_bytes
    ));

    if !report.arch_breakdown.is_empty() {
        out.push_str("- Architectures:\n");
        let mut sorted_arch: Vec<_> = report.arch_breakdown.iter().collect();
        sorted_arch.sort_by_key(|(k, _)| (*k).clone());
        for (arch, stat) in sorted_arch {
            out.push_str(&format!(
                "  • {}: {} items, {}, {} roots\n",
                arch,
                stat.count,
                format_bytes(stat.bytes),
                stat.roots_count
            ));
        }
    }

    out
}

async fn append_github_output(key: &str, value: &str) {
    if let Ok(file_path) = env::var("GITHUB_OUTPUT")
        && !file_path.trim().is_empty()
    {
        let line = format!("{}={}\n", key, value);
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .await;
        let _ = fs::write(&file_path, line).await;
    }
}

/// 执行构建缓存查询、列表与统计工作流
pub async fn run_list(
    args: &ListArgs,
    repo: &str,
    registry: &str,
    github_token: &str,
) -> Result<(), BuilderError> {
    let target_tag = args.resolve_target_tag();
    let format = args.resolve_format();
    let sort_by = args.resolve_sort_by();
    let sort_order = args.resolve_sort_order();
    let limit = args.resolve_limit();
    let details = args.resolve_details();

    let oci = create_tokio_reqwest_client(registry, repo, github_token, true);

    // 1. 探查多架构并获取所有架构的 ShardedArchCacheIndexData
    let mut target_systems: HashSet<SystemArch> = HashSet::new();
    if let Ok(Some(artifact)) = oci.fetch_artifact(&target_tag).await {
        match artifact.manifest {
            OciArtifactManifest::Index(index) => {
                for desc in index.manifests {
                    if let Some(ref plat) = desc.platform {
                        let sys = SystemArch::from_oci(
                            &plat.os,
                            &plat.architecture,
                            plat.variant.as_deref(),
                        );
                        if sys.is_known() {
                            target_systems.insert(sys);
                        }
                    }
                }
            }
            OciArtifactManifest::Manifest(_) => {
                let detected = SystemArch::detect_current();
                if detected.is_known() {
                    target_systems.insert(detected);
                }
            }
        }
    }

    if target_systems.is_empty() {
        for sys in SystemArch::all() {
            target_systems.insert(sys);
        }
    }

    let mut all_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    let mut all_gc_roots: HashMap<SystemArch, Vec<StoreHash>> = HashMap::new();

    let root_futures = target_systems.into_iter().map(|sys| {
        let oci = oci.clone();
        let tag = target_tag.clone();
        async move {
            if let Ok(Some((root_data, _))) = oci.get_sharded_root_index(&tag, &sys).await {
                let non_empty_shards: Vec<_> = root_data
                    .shards
                    .iter()
                    .filter(|s| s.entry_count > 0 && !s.blob_digest.is_empty())
                    .map(|s| s.blob_digest.clone())
                    .collect();

                let shard_futures = non_empty_shards.into_iter().map(|digest| {
                    let oci = oci.clone();
                    async move { oci.get_shard_data(&digest).await.ok() }
                });
                let payloads = join_all(shard_futures).await;
                let mut entries = HashMap::new();
                for p in payloads.into_iter().flatten() {
                    entries.extend(p.entries);
                }
                Some((sys, root_data.gc_roots, entries))
            } else {
                None
            }
        }
    });

    let results = join_all(root_futures).await;
    for (sys, roots, entries) in results.into_iter().flatten() {
        all_gc_roots.insert(sys, roots);
        all_entries.extend(entries);
    }

    if all_entries.is_empty() {
        warn!(
            "No cache index found for target tag '{}' in {}/{}",
            target_tag, registry, repo
        );
    }

    // 动态解析 Flake 输出 StoreHash (若有指定)
    let mut extra_hashes = Vec::new();
    if let Some(ref flake_path) = args.selector.resolve_flake_path() {
        let attrs = args.selector.resolve_attributes();
        info!("Evaluating flake outputs for listing from: {}", flake_path);
        let flake_hashes = resolve_flake_output_hashes(flake_path, &attrs).await?;
        info!(
            "Resolved {} store hashes from flake outputs",
            flake_hashes.len()
        );
        extra_hashes.extend(flake_hashes);
    }

    let selector = args.selector.to_selector(&extra_hashes, CascadeMode::Exact);
    let query_res: CacheQueryResult = evaluate_cache_query(&all_entries, &all_gc_roots, &selector);

    let total_entries = all_entries.len();
    let total_bytes: u64 = all_entries.values().map(|e| e.nar_size).sum();
    let matched_entries = query_res.matched_entries.len();
    let matched_bytes = query_res.matched_bytes;

    // 统计架构分布
    let mut arch_breakdown: HashMap<String, ArchStat> = HashMap::new();
    for entry in all_entries.values() {
        let sys_name = entry
            .system
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let stat = arch_breakdown.entry(sys_name).or_insert(ArchStat {
            count: 0,
            bytes: 0,
            roots_count: 0,
        });
        stat.count += 1;
        stat.bytes += entry.nar_size;
    }
    for (sys, roots) in &all_gc_roots {
        let sys_name = sys.to_string();
        let stat = arch_breakdown.entry(sys_name).or_insert(ArchStat {
            count: 0,
            bytes: 0,
            roots_count: 0,
        });
        stat.roots_count = roots.len();
    }

    // 构造条目 DTO 列表
    let mut items: Vec<ListItemDto> = query_res
        .matched_entries
        .iter()
        .map(|(hash, entry)| {
            let reason = query_res
                .reason_map
                .get(hash)
                .cloned()
                .unwrap_or_else(|| "Match".to_string());
            ListItemDto {
                hash: hash.to_string(),
                name: entry.name.clone(),
                system: entry
                    .system
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                store_path: entry.narinfo_meta.store_path.clone(),
                nar_basename: entry.narinfo_meta.nar_basename.clone(),
                nar_size: entry.nar_size,
                nar_size_human: format_bytes(entry.nar_size),
                added: entry.added.clone(),
                reason,
                references: entry.narinfo_meta.references.clone(),
                nar_digest: entry.nar_digest.to_string(),
            }
        })
        .collect();

    // 排序
    items.sort_by(|a, b| {
        let ord = match sort_by {
            SortBy::AddedDate => a.added.cmp(&b.added),
            SortBy::NarSize => a.nar_size.cmp(&b.nar_size),
            SortBy::Name => a.name.cmp(&b.name),
            SortBy::StoreHash => a.hash.cmp(&b.hash),
        };
        match sort_order {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        }
    });

    let displayed_items: Vec<ListItemDto> = if let Some(n) = limit {
        items.iter().take(n).cloned().collect()
    } else {
        items.clone()
    };

    let report = CacheListSummaryReport {
        target_tag: target_tag.clone(),
        registry: registry.to_string(),
        repo: repo.to_string(),
        total_entries,
        total_bytes,
        matched_entries,
        matched_bytes,
        arch_breakdown,
        items: items.clone(),
    };

    // 格式化输出到标准输出
    match format {
        OutputFormat::Table => {
            let table_str = format_table(&report, &displayed_items, details);
            println!("{}", table_str);
        }
        OutputFormat::Json => {
            let json_str = serde_json::to_string_pretty(&report)?;
            println!("{}", json_str);
        }
        OutputFormat::Ndjson => {
            for item in &displayed_items {
                let json_line = serde_json::to_string(item)?;
                println!("{}", json_line);
            }
        }
        OutputFormat::Paths => {
            for item in &displayed_items {
                println!("{}", item.store_path);
            }
        }
        OutputFormat::Summary => {
            let summary_str = format_summary(&report);
            println!("{}", summary_str);
        }
    }

    // 写入 GitHub Actions Output
    append_github_output("total_count", &total_entries.to_string()).await;
    append_github_output("total_size_bytes", &total_bytes.to_string()).await;
    append_github_output("matched_count", &matched_entries.to_string()).await;
    append_github_output("matched_size_bytes", &matched_bytes.to_string()).await;
    if let Ok(entries_json) = serde_json::to_string(&displayed_items) {
        append_github_output("entries_json", &entries_json).await;
    }

    // 写入 GitHub Step Summary
    let selector_desc = build_selector_description(args);
    write_list_step_summary(&report, &selector_desc, &displayed_items).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixcache_core::StoreHash;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 50), "50.00 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 2), "2.00 GiB");
    }

    #[test]
    fn test_table_formatting() {
        let hash1 = StoreHash::new_unchecked("0000000000000000000000000000pkg1");
        let mut arch_breakdown = HashMap::new();
        arch_breakdown.insert(
            "x86_64-linux".to_string(),
            ArchStat {
                count: 1,
                bytes: 1024,
                roots_count: 1,
            },
        );

        let report = CacheListSummaryReport {
            target_tag: "cache-index".to_string(),
            registry: "ghcr.io".to_string(),
            repo: "owner/repo".to_string(),
            total_entries: 1,
            total_bytes: 1024,
            matched_entries: 1,
            matched_bytes: 1024,
            arch_breakdown,
            items: vec![ListItemDto {
                hash: hash1.to_string(),
                name: "test-pkg".to_string(),
                system: "x86_64-linux".to_string(),
                store_path: format!("/nix/store/{}-test-pkg", hash1),
                nar_basename: "test-pkg.nar.xz".to_string(),
                nar_size: 1024,
                nar_size_human: "1.00 KiB".to_string(),
                added: "2026-08-30T10:00:00Z".to_string(),
                reason: "Explicit Match".to_string(),
                references: vec![],
                nar_digest: "sha256:blob1".to_string(),
            }],
        };

        let formatted = format_table(&report, &report.items, false);
        assert!(formatted.contains("test-pkg"));
        assert!(formatted.contains("1.00 KiB"));
        assert!(formatted.contains("x86_64-linux"));

        let summary = format_summary(&report);
        assert!(summary.contains("Total Entries: 1"));
    }
}
