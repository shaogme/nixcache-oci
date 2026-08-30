use std::env;
use tokio::fs;

async fn append_summary(content: String, target_file_opt: Option<&str>) {
    let path = match target_file_opt {
        Some(p) => Some(p.to_string()),
        None => env::var("GITHUB_STEP_SUMMARY")
            .ok()
            .filter(|s| !s.trim().is_empty()),
    };

    if let Some(file_path) = path {
        let _ = fs::write(&file_path, content).await;
    }
}

/// 为 Session Init 步骤生成并写入 GitHub Actions Step Summary
pub async fn write_session_init_summary(
    repo: &str,
    run_id: Option<u64>,
    branch: Option<&str>,
    port: u16,
) {
    write_session_init_summary_to(repo, run_id, branch, port, None).await;
}

pub async fn write_session_init_summary_to(
    repo: &str,
    run_id: Option<u64>,
    branch: Option<&str>,
    port: u16,
    file_opt: Option<&str>,
) {
    let content = format!(
        "### 🚀 NixCache Session Initialized\n\n- **Repository:** `{}`\n- **Run ID:** `{:?}`\n- **Branch/PR:** `{:?}`\n- **Proxy Daemon Port:** `{}`\n",
        repo, run_id, branch, port
    );
    append_summary(content, file_opt).await;
}

/// 为 Session Capture 步骤生成并写入 GitHub Actions Step Summary
pub async fn write_session_capture_summary(
    job_id: &str,
    system: &str,
    candidate_paths: usize,
    uploaded_blobs: usize,
    total_bytes: u64,
) {
    write_session_capture_summary_to(
        job_id,
        system,
        candidate_paths,
        uploaded_blobs,
        total_bytes,
        None,
    )
    .await;
}

pub async fn write_session_capture_summary_to(
    job_id: &str,
    system: &str,
    candidate_paths: usize,
    uploaded_blobs: usize,
    total_bytes: u64,
    file_opt: Option<&str>,
) {
    let content = format!(
        "### 📦 NixCache Session Captured\n\n- **Job:** `{}`\n- **System:** `{}`\n- **Candidate Paths:** `{}`\n- **Uploaded Blobs:** `{}`\n- **Uploaded Bytes:** `{}` bytes\n",
        job_id, system, candidate_paths, uploaded_blobs, total_bytes
    );
    append_summary(content, file_opt).await;
}

/// 为 Worker 节点构建步骤生成并写入 GitHub Actions Step Summary
pub async fn write_worker_step_summary(
    system: &str,
    discovered: usize,
    built: usize,
    to_upload: usize,
    uploaded: usize,
) {
    write_worker_step_summary_to(system, discovered, built, to_upload, uploaded, None).await;
}

pub async fn write_worker_step_summary_to(
    system: &str,
    discovered: usize,
    built: usize,
    to_upload: usize,
    uploaded: usize,
    file_opt: Option<&str>,
) {
    let content = format!(
        "### 🔨 NixCache Worker Build\n\n- **System:** `{}`\n- **Discovered Outputs:** `{}`\n- **Built Outputs:** `{}`\n- **New Paths to Upload:** `{}`\n- **Uploaded Blobs:** `{}`\n",
        system, discovered, built, to_upload, uploaded
    );
    append_summary(content, file_opt).await;
}

/// 为 Promote 步骤生成并写入 GitHub Actions Step Summary
pub async fn write_promote_step_summary(
    run_id: Option<u64>,
    target_tag: &str,
    total_entries: usize,
    promoted_entries: usize,
) {
    write_promote_step_summary_to(run_id, target_tag, total_entries, promoted_entries, None).await;
}

pub async fn write_promote_step_summary_to(
    run_id: Option<u64>,
    target_tag: &str,
    total_entries: usize,
    promoted_entries: usize,
    file_opt: Option<&str>,
) {
    let content = format!(
        "### 🌟 NixCache Promotion Complete\n\n- **Run ID:** `{:?}`\n- **Target Tag:** `{}`\n- **Total Index Entries:** `{}`\n- **Promoted New Entries:** `{}`\n",
        run_id, target_tag, total_entries, promoted_entries
    );
    append_summary(content, file_opt).await;
}

/// 为 Purge 步骤生成并写入 GitHub Actions Step Summary
pub async fn write_purge_step_summary(
    dry_run: bool,
    purged_count: usize,
    kept_count: usize,
    freed_bytes: u64,
    blobs_deleted: usize,
) {
    write_purge_step_summary_to(
        dry_run,
        purged_count,
        kept_count,
        freed_bytes,
        blobs_deleted,
        None,
    )
    .await;
}

pub async fn write_purge_step_summary_to(
    dry_run: bool,
    purged_count: usize,
    kept_count: usize,
    freed_bytes: u64,
    blobs_deleted: usize,
    file_opt: Option<&str>,
) {
    let mode_str = if dry_run {
        " (Dry Run - Preview Only)"
    } else {
        ""
    };
    let content = format!(
        "### 🧹 NixCache Cache Purge Report{}\n\n- **Purged Entries:** `{}`\n- **Kept Entries:** `{}`\n- **Estimated Space Freed:** `{}` bytes\n- **Physical Blobs Deleted:** `{}`\n",
        mode_str, purged_count, kept_count, freed_bytes, blobs_deleted
    );
    append_summary(content, file_opt).await;
}

/// 为 List 步骤生成并写入 GitHub Actions Step Summary
pub async fn write_list_step_summary(
    report: &crate::list::CacheListSummaryReport,
    selector_desc: &str,
    displayed_items: &[crate::list::ListItemDto],
) {
    write_list_step_summary_to(report, selector_desc, displayed_items, None).await;
}

pub async fn write_list_step_summary_to(
    report: &crate::list::CacheListSummaryReport,
    selector_desc: &str,
    displayed_items: &[crate::list::ListItemDto],
    file_opt: Option<&str>,
) {
    use crate::list::format_bytes;

    let mut md = String::new();
    md.push_str("### 📋 NixCache Build Cache Inspection Report\n\n");
    md.push_str(&format!(
        "- **Target Repository:** `{}/{}`\n- **Target Tag:** `{}`\n- **Selector Scope:** `{}`\n\n",
        report.registry, report.repo, report.target_tag, selector_desc
    ));

    md.push_str("#### 📊 Cache Overview & Statistics\n\n");
    md.push_str("| Metric | Value |\n");
    md.push_str("| :--- | :--- |\n");
    md.push_str(&format!(
        "| Total Index Entries | `{}` |\n",
        report.total_entries
    ));
    md.push_str(&format!(
        "| Total Cache Volume | `{}` ({} bytes) |\n",
        format_bytes(report.total_bytes),
        report.total_bytes
    ));
    md.push_str(&format!(
        "| Matched Query Entries | `{}` |\n",
        report.matched_entries
    ));
    md.push_str(&format!(
        "| Matched Query Volume | `{}` ({} bytes) |\n",
        format_bytes(report.matched_bytes),
        report.matched_bytes
    ));
    let total_roots: usize = report.arch_breakdown.values().map(|a| a.roots_count).sum();
    md.push_str(&format!(
        "| Total Active GC Roots | `{}` |\n\n",
        total_roots
    ));

    if !report.arch_breakdown.is_empty() {
        md.push_str("#### 🖥️ System Architecture Breakdown\n\n");
        md.push_str("| Architecture | Total Entries | Total Size | GC Roots |\n");
        md.push_str("| :--- | :--- | :--- | :--- |\n");
        let mut sorted_arch: Vec<_> = report.arch_breakdown.iter().collect();
        sorted_arch.sort_by_key(|(k, _)| (*k).clone());
        for (arch, stat) in sorted_arch {
            md.push_str(&format!(
                "| `{}` | `{}` | `{}` | `{}` |\n",
                arch,
                stat.count,
                format_bytes(stat.bytes),
                stat.roots_count
            ));
        }
        md.push('\n');
    }

    if !displayed_items.is_empty() {
        md.push_str(&format!(
            "#### 🔍 Matched Entries Preview (Top {})\n\n",
            displayed_items.len()
        ));
        md.push_str("| # | Package / Store Path | Arch | NAR Size | Added At | Match Reason |\n");
        md.push_str("| :--- | :--- | :--- | :--- | :--- | :--- |\n");
        for (i, item) in displayed_items.iter().enumerate() {
            let pkg_display = if item.name.is_empty() {
                format!("`{}`", item.hash)
            } else {
                format!("**{}**<br>`{}`", item.name, item.store_path)
            };
            md.push_str(&format!(
                "| {} | {} | `{}` | `{}` | `{}` | {} |\n",
                i + 1,
                pkg_display,
                item.system,
                item.nar_size_human,
                item.added,
                item.reason
            ));
        }
        md.push('\n');
    }

    append_summary(md, file_opt).await;
}

#[cfg(test)]
mod tests {
    use super::{
        write_list_step_summary_to, write_promote_step_summary_to, write_purge_step_summary_to,
        write_session_capture_summary_to, write_session_init_summary_to,
        write_worker_step_summary_to,
    };
    use crate::list::{ArchStat, CacheListSummaryReport, ListItemDto};
    use std::collections::HashMap;
    use tempfile::NamedTempFile;
    use tokio::fs;

    #[tokio::test]
    async fn test_summaries_generation() {
        let temp_file = NamedTempFile::new().unwrap();
        let path_str = temp_file.path().to_string_lossy().to_string();

        write_session_init_summary_to(
            "shaogme/nixcache-oci",
            Some(12345),
            Some("main"),
            37515,
            Some(&path_str),
        )
        .await;
        let content1 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content1.contains("NixCache Session Initialized"));
        assert!(content1.contains("12345"));

        write_session_capture_summary_to("build-job", "x86_64-linux", 5, 3, 10240, Some(&path_str))
            .await;
        let content2 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content2.contains("NixCache Session Captured"));
        assert!(content2.contains("10240"));

        write_worker_step_summary_to("x86_64-linux", 10, 8, 4, 4, Some(&path_str)).await;
        let content3 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content3.contains("NixCache Worker Build"));

        write_promote_step_summary_to(Some(12345), "cache-index", 100, 10, Some(&path_str)).await;
        let content4 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content4.contains("NixCache Promotion Complete"));

        write_purge_step_summary_to(false, 10, 90, 50000, 5, Some(&path_str)).await;
        let content5 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content5.contains("NixCache Cache Purge Report"));

        let mut arch_breakdown = HashMap::new();
        arch_breakdown.insert(
            "x86_64-linux".to_string(),
            ArchStat {
                count: 1,
                bytes: 1024,
                roots_count: 1,
            },
        );
        let list_report = CacheListSummaryReport {
            target_tag: "cache-index".to_string(),
            registry: "ghcr.io".to_string(),
            repo: "owner/repo".to_string(),
            total_entries: 1,
            total_bytes: 1024,
            matched_entries: 1,
            matched_bytes: 1024,
            arch_breakdown,
            items: vec![ListItemDto {
                hash: "0000000000000000000000000000pkg1".to_string(),
                name: "pkg1".to_string(),
                system: "x86_64-linux".to_string(),
                store_path: "/nix/store/0000000000000000000000000000pkg1-pkg1".to_string(),
                nar_basename: "pkg1.nar.xz".to_string(),
                nar_size: 1024,
                nar_size_human: "1.00 KiB".to_string(),
                added: "2026-08-30T10:00:00Z".to_string(),
                reason: "Match".to_string(),
                references: vec![],
                nar_digest: "sha256:blob1".to_string(),
            }],
        };
        write_list_step_summary_to(
            &list_report,
            "all=true",
            &list_report.items,
            Some(&path_str),
        )
        .await;
        let content6 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content6.contains("NixCache Build Cache Inspection Report"));
        assert!(content6.contains("pkg1"));
    }
}
