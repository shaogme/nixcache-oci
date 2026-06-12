use crate::nix;
use chrono::{DateTime, Utc};
use nixcache_oci::OciClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    env,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, process::Command, time::sleep};
use tracing::{error, info};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct IndexEntry {
    name: String,
    narinfo: String,
    nar_digest: String,
    nar_size: u64,
    added: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct CacheIndexData {
    version: u32,
    repo: String,
    registry: String,
    image: String,
    generated: String,
    public_key: String,
    entries: HashMap<String, IndexEntry>,
    gc_roots: Vec<String>,
}

fn find_proxy_binary() -> PathBuf {
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

async fn write_nix_conf(
    substituters: &[&str],
    trusted_keys: &[&str],
) -> Result<Option<(PathBuf, String)>, String> {
    let nix_conf_path = PathBuf::from("/etc/nix/nix.conf");

    let original_content = if nix_conf_path.exists() {
        fs::read_to_string(&nix_conf_path).await.unwrap_or_default()
    } else {
        String::new()
    };

    let mut new_content = original_content.clone();
    if !new_content.ends_with('\n') && !new_content.is_empty() {
        new_content.push('\n');
    }

    for sub in substituters {
        new_content.push_str(&format!("extra-substituters = {}\n", sub));
        new_content.push_str(&format!("extra-trusted-substituters = {}\n", sub));
    }
    for key in trusted_keys {
        new_content.push_str(&format!("extra-trusted-public-keys = {}\n", key));
    }

    match fs::write(&nix_conf_path, &new_content).await {
        Ok(_) => {
            info!("Added self-substituter to {:?}", nix_conf_path);
            Ok(Some((nix_conf_path, original_content)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if let Ok(home) = env::var("HOME") {
                let user_conf_path = PathBuf::from(home).join(".config/nix/nix.conf");
                if let Some(parent) = user_conf_path.parent() {
                    let _ = fs::create_dir_all(parent).await;
                }
                let user_original = if user_conf_path.exists() {
                    fs::read_to_string(&user_conf_path)
                        .await
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let mut user_new = user_original.clone();
                if !user_new.ends_with('\n') && !user_new.is_empty() {
                    user_new.push('\n');
                }
                for sub in substituters {
                    user_new.push_str(&format!("extra-substituters = {}\n", sub));
                    user_new.push_str(&format!("extra-trusted-substituters = {}\n", sub));
                }
                for key in trusted_keys {
                    user_new.push_str(&format!("extra-trusted-public-keys = {}\n", key));
                }
                fs::write(&user_conf_path, user_new)
                    .await
                    .map_err(|err| format!("Failed to write user nix.conf: {}", err))?;
                info!("Added self-substituter to {:?}", user_conf_path);
                Ok(Some((user_conf_path, user_original)))
            } else {
                Err(format!(
                    "Permission denied for /etc/nix/nix.conf and HOME env is not set: {}",
                    e
                ))
            }
        }
        Err(e) => Err(format!("Failed to write nix.conf: {}", e)),
    }
}

async fn restore_nix_conf(backup: Option<(PathBuf, String)>) {
    if let Some((path, content)) = backup {
        if content.is_empty() {
            let _ = fs::remove_file(&path).await;
        } else {
            let _ = fs::write(&path, content).await;
        }
        info!("Restored nix.conf at {:?}", path);
    }
}

async fn get_own_public_key(signing_key_file: Option<&str>) -> Option<String> {
    let key_file = signing_key_file?;
    let pub_file = format!("{}.pub", key_file);
    if Path::new(&pub_file).exists() {
        fs::read_to_string(&pub_file)
            .await
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        // Run nix key convert-secret-to-public
        let mut output = Command::new("nix")
            .args(["key", "convert-secret-to-public"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?;

        // Write secret key to stdin
        use tokio::io::AsyncWriteExt;
        let secret = fs::read_to_string(key_file).await.ok()?;
        let mut stdin = output.stdin.take()?;
        let _ = stdin.write_all(secret.as_bytes()).await;
        let _ = stdin.flush().await;
        drop(stdin);

        let res = output.wait_with_output().await.ok()?;
        if res.status.success() {
            Some(String::from_utf8_lossy(&res.stdout).trim().to_string())
        } else {
            None
        }
    }
}

pub async fn run_pipeline(
    build_config: &nix::BuildConfig,
    repo: &str,
    registry: &str,
    signing_key_file: Option<&str>,
    github_token: &str,
) -> Result<(), String> {
    info!("Starting OCI cache pipeline");
    let image = format!("{}/{}/nix-cache", registry, repo);
    info!("Config: {:?} | Image: {}", build_config, image);

    // 1. Start self-substituter proxy
    let proxy_bin = find_proxy_binary();
    info!("Starting self-substituter proxy using {:?}", proxy_bin);

    let mut proxy_cmd = Command::new(&proxy_bin);
    proxy_cmd
        .env("NIXCACHE_REPO", repo)
        .env("NIXCACHE_REGISTRY", registry)
        .env("NIXCACHE_PORT", "37515")
        .env("NIXCACHE_LISTEN", "127.0.0.1")
        .env("NIXCACHE_UPSTREAM", "")
        .env("GITHUB_TOKEN", github_token)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut proxy_child = proxy_cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn nixcache-proxy: {}", e))?;

    let mut ready = false;
    let client = reqwest::Client::new();
    for _ in 1..=15 {
        if let Ok(res) = client
            .get("http://127.0.0.1:37515/nix-cache-info")
            .send()
            .await
            && res.status().is_success()
        {
            ready = true;
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }

    let mut nix_conf_backup = None;
    if ready {
        info!("Self-substituter running on port 37515");
        let mut keys = Vec::new();
        let pub_key = get_own_public_key(signing_key_file).await;
        if let Some(ref k) = pub_key {
            keys.push(k.as_str());
            info!("Trusted own public key: {}", k);
        }
        nix_conf_backup = write_nix_conf(&["http://127.0.0.1:37515"], &keys).await?;
    } else {
        info!("Self-substituter failed to respond, proceeding without it");
    }

    // 2. Discover outputs
    let discovered = nix::discover_outputs(build_config).await?;
    info!("Discovered {} output(s) to build", discovered.len());

    // 3. Build outputs
    let output_paths = nix::build_outputs(&discovered).await?;
    info!("Built {} top-level output(s)", output_paths.len());

    // 4. Restore configuration and stop proxy
    restore_nix_conf(nix_conf_backup).await;
    let _ = proxy_child.kill().await;

    // 5. Fetch remote cache-index
    let oci = OciClient::new(registry, repo, github_token, true);
    let mut remote_index = CacheIndexData::default();
    let mut own_hashes = Vec::new();

    if let Ok(Some(manifest_json)) = oci.get_manifest("cache-index").await
        && let Ok(manifest) = serde_json::from_str::<Value>(&manifest_json)
        && let Some(layers) = manifest.get("layers").and_then(|l| l.as_array())
        && !layers.is_empty()
        && let Some(digest) = layers[0].get("digest").and_then(|d| d.as_str())
        && let Ok(blob_bytes) = oci.get_blob(digest).await
        && let Ok(data) = serde_json::from_slice::<CacheIndexData>(&blob_bytes)
    {
        remote_index = data;
        own_hashes = remote_index.entries.keys().cloned().collect();
    }

    info!(
        "GHCR index contains {} previously-cached entries",
        own_hashes.len()
    );

    // 6. Find locally built paths
    info!("Inspecting closure signatures to find locally-built paths");
    let upload_list = nix::find_locally_built_paths(&output_paths, &own_hashes).await?;
    if upload_list.is_empty() {
        info!("Nothing to upload — every path has an external or existing signature");
        write_step_summary(discovered.len(), 0, 0, output_paths.len()).await;
        return Ok(());
    }

    info!("Locally-built paths to upload: {}", upload_list.len());

    // 7. Export paths
    let temp_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temporary directory: {}", e))?;
    let exported =
        nix::export_paths_directly(&upload_list, signing_key_file, temp_dir.path()).await?;

    // 8. Upload NARs and accumulate new index entries
    let mut new_entries = HashMap::new();
    let mut uploaded_count = 0;

    for (hash, store_path) in exported {
        let narinfo_path = temp_dir.path().join(format!("{}.narinfo", hash));
        let nar_file_path = temp_dir.path().join("nar").join(format!("{}.nar.xz", hash));

        if !nar_file_path.exists() {
            error!("NAR file not found for {}", hash);
            continue;
        }

        let metadata = fs::metadata(&nar_file_path)
            .await
            .map_err(|e| format!("Failed to read nar file metadata: {}", e))?;
        let size = metadata.len();

        info!("  Uploading NAR for {} ({} bytes)", hash, size);
        match oci.push_blob(&nar_file_path).await {
            Ok(nar_digest) => {
                if let Ok(narinfo_content) = fs::read_to_string(&narinfo_path).await {
                    let name = Path::new(&store_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|s| s.split_once('-'))
                        .map(|x| x.1.to_string())
                        .unwrap_or(hash.clone());

                    new_entries.insert(
                        hash.clone(),
                        IndexEntry {
                            name,
                            narinfo: narinfo_content,
                            nar_digest,
                            nar_size: size,
                            added: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        },
                    );
                    uploaded_count += 1;
                }
            }
            Err(e) => {
                error!("Failed to upload NAR for {}: {}", hash, e);
            }
        }
    }

    if uploaded_count == 0 {
        info!("No new paths successfully uploaded");
        return Ok(());
    }

    // 9. Update cache-index metadata
    let pub_key = get_own_public_key(signing_key_file).await;
    let mut index = CacheIndexData {
        version: 1,
        repo: repo.to_string(),
        registry: registry.to_string(),
        image: image.clone(),
        generated: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        public_key: pub_key.unwrap_or(remote_index.public_key),
        entries: remote_index.entries,
        gc_roots: remote_index.gc_roots,
    };

    index.entries.extend(new_entries);

    // Merge gc_roots
    let mut gc_roots_set: HashSet<String> = index.gc_roots.into_iter().collect();
    for p in &output_paths {
        if let Some(file_name) = Path::new(p).file_name().and_then(|n| n.to_str())
            && file_name.len() >= 32
        {
            gc_roots_set.insert(file_name[..32].to_string());
        }
    }
    index.gc_roots = gc_roots_set.into_iter().collect();
    index.gc_roots.sort();

    // 10. Write and upload updated cache-index
    let index_json = serde_json::to_string_pretty(&index)
        .map_err(|e| format!("Failed to serialize cache index: {}", e))?;

    let index_temp_path = temp_dir.path().join("cache-index.json");
    fs::write(&index_temp_path, &index_json)
        .await
        .map_err(|e| format!("Failed to write index temp: {}", e))?;

    let index_digest = oci
        .push_blob(&index_temp_path)
        .await
        .map_err(|e| e.to_string())?;
    let index_size = fs::metadata(&index_temp_path)
        .await
        .map_err(|e| format!("Failed to get index size: {}", e))?
        .len();

    // Push empty config
    let config_temp_path = temp_dir.path().join("config.json");
    fs::write(&config_temp_path, "{}")
        .await
        .map_err(|e| format!("Failed to write config temp: {}", e))?;
    let config_digest = oci
        .push_blob(&config_temp_path)
        .await
        .map_err(|e| e.to_string())?;
    let config_size = 2; // size of "{}"

    // Push index manifest
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_size
        },
        "layers": [{
            "mediaType": "application/vnd.nix.cache.index.v1+json",
            "digest": index_digest,
            "size": index_size
        }]
    });

    oci.push_manifest("cache-index", &manifest.to_string())
        .await
        .map_err(|e| e.to_string())?;
    info!("Cache index updated ({} new entries)", uploaded_count);

    write_step_summary(
        discovered.len(),
        upload_list.len(),
        uploaded_count,
        output_paths.len(),
    )
    .await;
    info!("Pipeline complete!");
    Ok(())
}

pub async fn run_gc(
    retention_days: u64,
    dry_run: bool,
    build_config: &nix::BuildConfig,
    repo: &str,
    registry: &str,
    github_token: &str,
) -> Result<(), String> {
    info!("Running garbage collection");
    let oci = OciClient::new(registry, repo, github_token, true);

    let manifest_json = oci
        .get_manifest("cache-index")
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "No cache index found, nothing to GC".to_string())?;

    let manifest = serde_json::from_str::<Value>(&manifest_json)
        .map_err(|e| format!("Failed to parse index manifest: {}", e))?;

    let layers = manifest
        .get("layers")
        .and_then(|l| l.as_array())
        .ok_or_else(|| "Invalid manifest layers".to_string())?;

    if layers.is_empty() {
        return Err("Manifest layers are empty".to_string());
    }

    let index_digest = layers[0]
        .get("digest")
        .and_then(|d| d.as_str())
        .ok_or_else(|| "Index digest missing".to_string())?;

    let index_bytes = oci
        .get_blob(index_digest)
        .await
        .map_err(|e| e.to_string())?;
    let mut index = serde_json::from_slice::<CacheIndexData>(&index_bytes)
        .map_err(|e| format!("Failed to parse index JSON: {}", e))?;

    info!("Loaded index with {} entries", index.entries.len());

    // Build current outputs to find live closure
    let discovered = nix::discover_outputs(build_config).await?;
    let output_paths = nix::build_outputs(&discovered).await?;
    let live_closure = nix::get_closure(&output_paths).await?;

    let mut live_hashes = HashSet::new();
    for p in live_closure {
        if let Some(file_name) = Path::new(&p).file_name().and_then(|n| n.to_str())
            && file_name.len() >= 32
        {
            live_hashes.insert(file_name[..32].to_string());
        }
    }

    let now = Utc::now();
    let retention_duration = chrono::Duration::days(retention_days as i64);
    let cutoff = now - retention_duration;

    info!(
        "Cutoff date: {}",
        cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );

    let mut kept_entries = HashMap::new();
    let mut to_delete_count = 0;

    for (hash, entry) in index.entries {
        let is_live = live_hashes.contains(&hash);
        let added_dt = DateTime::parse_from_rfc3339(&entry.added)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(now); // Keep if parse fails

        let is_old = added_dt < cutoff;

        if !is_live && is_old {
            to_delete_count += 1;
            info!("DELETE: {} ({}) added={}", hash, entry.name, entry.added);
        } else {
            kept_entries.insert(hash.clone(), entry);
            let reason = if is_live { "live" } else { "recent" };
            info!("KEEP:   {} reason={}", hash, reason);
        }
    }

    info!(
        "\n>>> Total: {} keep, {} delete",
        kept_entries.len(),
        to_delete_count
    );

    if dry_run {
        info!(">>> Dry run, no changes written");
        return Ok(());
    }

    if to_delete_count > 0 {
        index.entries = kept_entries;
        index.gc_roots.retain(|h| index.entries.contains_key(h));

        let updated_index_json = serde_json::to_string_pretty(&index)
            .map_err(|e| format!("Failed to serialize index: {}", e))?;

        let temp_dir = tempfile::tempdir()
            .map_err(|e| format!("Failed to create temporary directory: {}", e))?;

        let index_temp_path = temp_dir.path().join("cache-index.json");
        fs::write(&index_temp_path, &updated_index_json)
            .await
            .map_err(|e| format!("Failed to write index: {}", e))?;

        let index_digest = oci
            .push_blob(&index_temp_path)
            .await
            .map_err(|e| e.to_string())?;
        let index_size = fs::metadata(&index_temp_path)
            .await
            .map_err(|e| format!("Failed to get index size: {}", e))?
            .len();

        let config_temp_path = temp_dir.path().join("config.json");
        fs::write(&config_temp_path, "{}")
            .await
            .map_err(|e| format!("Failed to write config: {}", e))?;
        let config_digest = oci
            .push_blob(&config_temp_path)
            .await
            .map_err(|e| e.to_string())?;
        let config_size = 2;

        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config_size
            },
            "layers": [{
                "mediaType": "application/vnd.nix.cache.index.v1+json",
                "digest": index_digest,
                "size": index_size
            }]
        });

        oci.push_manifest("cache-index", &manifest.to_string())
            .await
            .map_err(|e| e.to_string())?;
        info!(">>> GC complete, index updated");
    }

    Ok(())
}

async fn write_step_summary(
    outputs: usize,
    new_paths: usize,
    uploaded: usize,
    total_outputs: usize,
) {
    if let Ok(summary_path) = env::var("GITHUB_STEP_SUMMARY") {
        let content = format!(
            "## Cache Build Summary\n\n| Metric | Count |\n|---|---|\n| Flake outputs discovered | {} |\n| Output paths built | {} |\n| New paths cached to GHCR | {} |\n| Successfully uploaded | {} |\n| Paths already cached | skipped |\n",
            outputs, total_outputs, new_paths, uploaded
        );
        let _ = fs::write(&summary_path, content).await;
    }
}
