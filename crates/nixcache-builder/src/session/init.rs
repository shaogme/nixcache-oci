use crate::{
    env_injector::NixEnvInjector, error::BuilderError, nix::driver::get_own_public_key,
    summary::write_session_init_summary,
};
use std::{
    collections::HashSet,
    env, fs as std_fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncWriteExt, BufWriter},
    process::Command,
    task,
    time::{Instant, sleep},
};
use tracing::info;

/// 快速本地 Nix Store Inode 扫描器与差集比对引擎
pub struct FastStoreScanner;

impl FastStoreScanner {
    /// 在阻塞线程池中以原生同步 I/O 极速扫描 `/nix/store` 目录全量 Inode
    pub async fn scan_store_names(store_path: &Path) -> Result<HashSet<String>, BuilderError> {
        let p = store_path.to_path_buf();
        task::spawn_blocking(move || {
            let mut set = HashSet::with_capacity(32768);
            let entries = std_fs::read_dir(&p)?;
            for entry in entries.flatten() {
                if let Ok(file_name) = entry.file_name().into_string()
                    && !file_name.ends_with(".drv")
                    && !file_name.ends_with(".lock")
                {
                    set.insert(file_name);
                }
            }
            Ok(set)
        })
        .await
        .map_err(|e| BuilderError::Other(format!("Store scan thread joined error: {}", e)))?
    }

    /// 计算差集候选路径全量列表
    pub async fn compute_diff_paths(
        store_path: &Path,
        snapshot_file: &Path,
    ) -> Result<Vec<String>, BuilderError> {
        if !snapshot_file.exists() {
            return Ok(Vec::new());
        }

        let snap_content = fs::read_to_string(snapshot_file).await.unwrap_or_default();
        let before_set: HashSet<String> = snap_content
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        let current_set = Self::scan_store_names(store_path).await?;

        let mut diff_paths = Vec::new();
        for name in current_set.difference(&before_set) {
            diff_paths.push(format!("{}/{}", store_path.display(), name));
        }

        diff_paths.sort();
        Ok(diff_paths)
    }
}

pub fn find_proxy_binary() -> PathBuf {
    if let Ok(current_exe) = env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        let local_proxy = parent.join("nixcache-proxy");
        if local_proxy.exists() {
            return local_proxy;
        }
        let local_proxy_exe = parent.join("nixcache-proxy.exe");
        if local_proxy_exe.exists() {
            return local_proxy_exe;
        }
    }
    PathBuf::from("nixcache-proxy")
}

pub async fn record_store_snapshot(snap_path: &Path) -> Result<(), BuilderError> {
    record_store_snapshot_from_dir(Path::new("/nix/store"), snap_path).await
}

pub async fn record_store_snapshot_from_dir(
    store_dir: &Path,
    snap_path: &Path,
) -> Result<(), BuilderError> {
    let names = FastStoreScanner::scan_store_names(store_dir).await?;
    let mut paths: Vec<String> = names.into_iter().collect();
    paths.sort();
    if let Some(parent) = snap_path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let file = fs::File::create(snap_path).await?;
    let mut writer = BufWriter::new(file);
    for path in &paths {
        writer.write_all(path.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }
    writer.flush().await?;
    info!(
        "Recorded store snapshot with {} paths to {:?}",
        paths.len(),
        snap_path
    );
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SessionInitOptions<'a> {
    pub repo: &'a str,
    pub registry: &'a str,
    pub run_id: Option<u64>,
    pub branch: Option<String>,
    pub port: u16,
    pub listen: &'a str,
    pub upstream: &'a str,
    pub session_ttl: u64,
    pub baseline_ttl: u64,
    pub baseline_tag: &'a str,
    pub github_token: &'a str,
    pub signing_key_file: Option<&'a str>,
    pub snapshot_path: Option<&'a Path>,
}

/// Session Init: 启动后台 Proxy 守护进程，零侵入注入 NIX_CONFIG，并记录 store 快照
pub async fn run_session_init(opts: &SessionInitOptions<'_>) -> Result<(), BuilderError> {
    info!(
        "Initializing NixCache Session: Run ID: {:?}, Branch: {:?}, Repo: {}/{}",
        opts.run_id, opts.branch, opts.registry, opts.repo
    );

    let proxy_bin = find_proxy_binary();
    info!("Starting background proxy daemon using {:?}", proxy_bin);

    let mut proxy_cmd = Command::new(&proxy_bin);
    proxy_cmd
        .env("NIXCACHE_REPO", opts.repo)
        .env("NIXCACHE_REGISTRY", opts.registry)
        .env("NIXCACHE_PORT", opts.port.to_string())
        .env("NIXCACHE_LISTEN", opts.listen)
        .env("NIXCACHE_UPSTREAM", opts.upstream)
        .env("NIXCACHE_SESSION_TTL", opts.session_ttl.to_string())
        .env("NIXCACHE_INDEX_TTL", opts.baseline_ttl.to_string())
        .env("NIXCACHE_BASELINE_TAG", opts.baseline_tag)
        .env("GITHUB_TOKEN", opts.github_token)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Some(rid) = opts.run_id {
        proxy_cmd.env("NIXCACHE_RUN_ID", rid.to_string());
    }
    if let Some(ref br) = opts.branch {
        proxy_cmd.env("NIXCACHE_BRANCH", br);
    }

    let _child = proxy_cmd.spawn()?;

    let client = reqwest::Client::new();
    let probe_url = format!("http://{}:{}/nix-cache-info", opts.listen, opts.port);
    let mut ready = false;
    let mut backoff = Duration::from_millis(10);
    let max_backoff = Duration::from_millis(100);
    let max_wait = Duration::from_secs(10);
    let start_time = Instant::now();

    while start_time.elapsed() < max_wait {
        if let Ok(res) = client.get(&probe_url).send().await
            && res.status().is_success()
        {
            ready = true;
            break;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }

    if !ready {
        return Err(BuilderError::Proxy(format!(
            "Proxy failed to become ready on http://{}:{}",
            opts.listen, opts.port
        )));
    }
    info!(
        "Proxy is running and ready on http://{}:{}",
        opts.listen, opts.port
    );

    let proxy_substituter = format!("http://{}:{}", opts.listen, opts.port);
    let mut keys = Vec::new();
    let pub_key = get_own_public_key(opts.signing_key_file).await;
    if let Some(ref k) = pub_key {
        keys.push(k.as_str());
        info!("Trusted own public key: {}", k);
    }

    // 零侵入导出 NIX_CONFIG 至 GITHUB_ENV
    let nix_config = NixEnvInjector::generate_nix_config(&[&proxy_substituter], &keys);
    NixEnvInjector::export_to_github_env(&nix_config).await?;

    if let Some(snap) = opts.snapshot_path {
        record_store_snapshot(snap).await?;
    }

    write_session_init_summary(opts.repo, opts.run_id, opts.branch.as_deref(), opts.port).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_record_store_snapshot_from_dir() {
        let temp = tempdir().unwrap();
        let fake_store = temp.path().join("store");
        fs::create_dir_all(&fake_store).await.unwrap();

        fs::write(fake_store.join("cccc-pkg"), b"").await.unwrap();
        fs::write(fake_store.join("aaaa-pkg"), b"").await.unwrap();
        fs::write(fake_store.join("bbbb-pkg"), b"").await.unwrap();

        let snap_file = temp.path().join("snap/snapshot.txt");
        record_store_snapshot_from_dir(&fake_store, &snap_file)
            .await
            .unwrap();

        let content = fs::read_to_string(&snap_file).await.unwrap();
        assert_eq!(content, "aaaa-pkg\nbbbb-pkg\ncccc-pkg\n");
    }

    #[tokio::test]
    async fn test_find_proxy_binary_fallback() {
        let bin = find_proxy_binary();
        assert!(!bin.as_os_str().is_empty());
    }

    #[tokio::test]
    async fn test_fast_store_scanner_compute_diff_paths() {
        let temp = tempdir().unwrap();
        let fake_store = temp.path().join("store");
        fs::create_dir_all(&fake_store).await.unwrap();

        fs::write(fake_store.join("1111-old-pkg"), b"")
            .await
            .unwrap();
        fs::write(fake_store.join("2222-new-pkg"), b"")
            .await
            .unwrap();
        fs::write(fake_store.join("3333-drv.drv"), b"")
            .await
            .unwrap();
        fs::write(fake_store.join("4444-lock.lock"), b"")
            .await
            .unwrap();

        let snap_file = temp.path().join("snap.txt");
        fs::write(&snap_file, "1111-old-pkg\n").await.unwrap();

        let diff = FastStoreScanner::compute_diff_paths(&fake_store, &snap_file)
            .await
            .unwrap();

        assert_eq!(diff, vec![format!("{}/2222-new-pkg", fake_store.display())]);
    }
}
