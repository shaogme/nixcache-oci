use crate::error::BuilderError;
use std::path::Path;
use tokio::fs;
use tracing::info;

/// Session Clean: 清理本地快照文件与临时会话资源
pub async fn run_session_clean(snapshot_path: Option<&Path>) -> Result<(), BuilderError> {
    if let Some(path) = snapshot_path
        && path.exists()
    {
        fs::remove_file(path).await?;
        info!("Cleaned up snapshot file at {:?}", path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run_session_clean;
    use tempfile::tempdir;
    use tokio::fs;

    #[tokio::test]
    async fn test_session_clean_logic() {
        let temp_dir = tempdir().unwrap();
        let snap_file = temp_dir.path().join("snapshot.txt");
        fs::write(&snap_file, "storepath1\nstorepath2\n")
            .await
            .unwrap();
        assert!(snap_file.exists());

        let res = run_session_clean(Some(&snap_file)).await;
        assert!(res.is_ok());
        assert!(!snap_file.exists());

        let res2 = run_session_clean(Some(&snap_file)).await;
        assert!(res2.is_ok());
    }
}
