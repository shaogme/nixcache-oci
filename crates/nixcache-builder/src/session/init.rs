use crate::{
    env_injector::NixEnvInjector,
    error::BuilderError,
    nix::driver::get_own_public_key,
    summary::write_session_init_summary,
};
use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, process::Command, time::sleep};
use tracing::info;

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
    let mut paths = Vec::new();
    if let Ok(mut entries) = fs::read_dir("/nix/store").await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                paths.push(name.to_string());
            }
        }
    }
    paths.sort();
    if let Some(parent) = snap_path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let content = paths.join("\n");
    fs::write(snap_path, content).await?;
    info!(
        "Recorded store snapshot with {} paths to {:?}",
        paths.len(),
        snap_path
    );
    Ok(())
}

/// Session Init: 启动后台 Proxy 守护进程，零侵入注入 NIX_CONFIG，并记录 store 快照
#[allow(clippy::too_many_arguments)]
pub async fn run_session_init(
    repo: &str,
    registry: &str,
    run_id: Option<u64>,
    branch: Option<String>,
    port: u16,
    listen: &str,
    upstream: &str,
    session_ttl: u64,
    baseline_ttl: u64,
    baseline_tag: &str,
    github_token: &str,
    signing_key_file: Option<&str>,
    snapshot_path: Option<&Path>,
) -> Result<(), BuilderError> {
    info!(
        "Initializing NixCache Session: Run ID: {:?}, Branch: {:?}, Repo: {}/{}",
        run_id, branch, registry, repo
    );

    let proxy_bin = find_proxy_binary();
    info!("Starting background proxy daemon using {:?}", proxy_bin);

    let mut proxy_cmd = Command::new(&proxy_bin);
    proxy_cmd
        .env("NIXCACHE_REPO", repo)
        .env("NIXCACHE_REGISTRY", registry)
        .env("NIXCACHE_PORT", port.to_string())
        .env("NIXCACHE_LISTEN", listen)
        .env("NIXCACHE_UPSTREAM", upstream)
        .env("NIXCACHE_SESSION_TTL", session_ttl.to_string())
        .env("NIXCACHE_INDEX_TTL", baseline_ttl.to_string())
        .env("NIXCACHE_BASELINE_TAG", baseline_tag)
        .env("GITHUB_TOKEN", github_token)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    if let Some(rid) = run_id {
        proxy_cmd.env("NIXCACHE_RUN_ID", rid.to_string());
    }
    if let Some(ref br) = branch {
        proxy_cmd.env("NIXCACHE_BRANCH", br);
    }

    let _child = proxy_cmd.spawn()?;

    let client = reqwest::Client::new();
    let probe_url = format!("http://{}:{}/nix-cache-info", listen, port);
    let mut ready = false;
    for _ in 1..=20 {
        if let Ok(res) = client.get(&probe_url).send().await
            && res.status().is_success()
        {
            ready = true;
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }

    if !ready {
        return Err(BuilderError::Proxy(format!(
            "Proxy failed to become ready on http://{}:{}",
            listen, port
        )));
    }
    info!("Proxy is running and ready on http://{}:{}", listen, port);

    let proxy_substituter = format!("http://{}:{}", listen, port);
    let mut keys = Vec::new();
    let pub_key = get_own_public_key(signing_key_file).await;
    if let Some(ref k) = pub_key {
        keys.push(k.as_str());
        info!("Trusted own public key: {}", k);
    }

    // 零侵入注入 NIX_CONFIG
    let nix_config = NixEnvInjector::generate_nix_config(&[&proxy_substituter], &keys);
    unsafe {
        env::set_var("NIX_CONFIG", &nix_config);
    }
    NixEnvInjector::export_to_github_env(&nix_config).await?;

    if let Some(snap) = snapshot_path {
        record_store_snapshot(snap).await?;
    }

    write_session_init_summary(repo, run_id, branch.as_deref(), port).await;
    Ok(())
}
