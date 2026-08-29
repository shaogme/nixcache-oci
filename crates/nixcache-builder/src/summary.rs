use std::env;
use tokio::fs;

/// 为 Session Init 步骤生成并写入 GitHub Actions Step Summary
pub async fn write_session_init_summary(
    repo: &str,
    run_id: Option<u64>,
    branch: Option<&str>,
    port: u16,
) {
    if let Ok(summary_file) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "### 🚀 NixCache Session Initialized\n\n- **Repository:** `{}`\n- **Run ID:** `{:?}`\n- **Branch/PR:** `{:?}`\n- **Proxy Daemon Port:** `{}`\n",
            repo, run_id, branch, port
        );
        let _ = fs::write(&summary_file, content).await;
    }
}

/// 为 Session Capture 步骤生成并写入 GitHub Actions Step Summary
pub async fn write_session_capture_summary(
    job_id: &str,
    system: &str,
    candidate_paths: usize,
    uploaded_blobs: usize,
    total_bytes: u64,
) {
    if let Ok(summary_file) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "### 📦 NixCache Session Captured\n\n- **Job:** `{}`\n- **System:** `{}`\n- **Candidate Paths:** `{}`\n- **Uploaded Blobs:** `{}`\n- **Uploaded Bytes:** `{}` bytes\n",
            job_id, system, candidate_paths, uploaded_blobs, total_bytes
        );
        let _ = fs::write(&summary_file, content).await;
    }
}

/// 为 Worker 节点构建步骤生成并写入 GitHub Actions Step Summary
pub async fn write_worker_step_summary(
    system: &str,
    discovered: usize,
    built: usize,
    to_upload: usize,
    uploaded: usize,
) {
    if let Ok(summary_file) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "### 🔨 NixCache Worker Build\n\n- **System:** `{}`\n- **Discovered Outputs:** `{}`\n- **Built Outputs:** `{}`\n- **New Paths to Upload:** `{}`\n- **Uploaded Blobs:** `{}`\n",
            system, discovered, built, to_upload, uploaded
        );
        let _ = fs::write(&summary_file, content).await;
    }
}

/// 为 Promote 步骤生成并写入 GitHub Actions Step Summary
pub async fn write_promote_step_summary(
    run_id: Option<u64>,
    target_tag: &str,
    total_entries: usize,
    promoted_entries: usize,
) {
    if let Ok(summary_file) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "### 🌟 NixCache Promotion Complete\n\n- **Run ID:** `{:?}`\n- **Target Tag:** `{}`\n- **Total Index Entries:** `{}`\n- **Promoted New Entries:** `{}`\n",
            run_id, target_tag, total_entries, promoted_entries
        );
        let _ = fs::write(&summary_file, content).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        write_promote_step_summary, write_session_capture_summary, write_session_init_summary,
        write_worker_step_summary,
    };
    use std::env;
    use tempfile::NamedTempFile;
    use tokio::fs;

    #[tokio::test]
    async fn test_summaries_generation() {
        let temp_file = NamedTempFile::new().unwrap();
        let path_str = temp_file.path().to_string_lossy().to_string();

        unsafe {
            env::set_var("GITHUB_STEP_SUMMARY", &path_str);
        }

        write_session_init_summary("shaogme/nixcache-oci", Some(12345), Some("main"), 37515).await;
        let content1 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content1.contains("NixCache Session Initialized"));
        assert!(content1.contains("12345"));

        write_session_capture_summary("build-job", "x86_64-linux", 5, 3, 10240).await;
        let content2 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content2.contains("NixCache Session Captured"));
        assert!(content2.contains("10240"));

        write_worker_step_summary("x86_64-linux", 10, 8, 4, 4).await;
        let content3 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content3.contains("NixCache Worker Build"));

        write_promote_step_summary(Some(12345), "cache-index", 100, 10).await;
        let content4 = fs::read_to_string(&path_str).await.unwrap();
        assert!(content4.contains("NixCache Promotion Complete"));

        unsafe {
            env::remove_var("GITHUB_STEP_SUMMARY");
        }
    }
}
