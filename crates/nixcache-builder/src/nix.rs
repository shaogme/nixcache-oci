use clap::ValueEnum;
use serde_json::Value;
use std::{path::Path, process::Stdio, sync::Arc};
use tokio::{fs, process::Command, sync::Semaphore};
use tracing::{error, info};

#[derive(serde::Serialize, serde::Deserialize, ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildMode {
    #[value(name = "flake")]
    Flake,
    #[value(name = "non-flake")]
    NonFlake,
}

#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub mode: BuildMode,
    pub flake_path: String,
    pub file: String,
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum BuildTarget {
    Flake {
        flake_ref: String,
        attribute: String,
    },
    NonFlake {
        file: String,
        attribute: Option<String>,
    },
}

impl std::fmt::Display for BuildTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildTarget::Flake {
                flake_ref,
                attribute,
            } => {
                write!(f, "{}#{}", flake_ref, attribute)
            }
            BuildTarget::NonFlake { file, attribute } => {
                if let Some(attr) = attribute {
                    write!(f, "{} -A {}", file, attr)
                } else {
                    write!(f, "{}", file)
                }
            }
        }
    }
}

pub async fn get_system() -> Result<String, String> {
    let output = Command::new("nix")
        .args([
            "eval",
            "--raw",
            "--impure",
            "--expr",
            "builtins.currentSystem",
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run nix eval: {}", e))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub async fn discover_outputs(config: &BuildConfig) -> Result<Vec<BuildTarget>, String> {
    match config.mode {
        BuildMode::Flake => {
            let system = get_system().await?;
            info!(
                "Discovering flake outputs for {} in {}",
                system, config.flake_path
            );

            let flake_ref = format!(
                "path:{}",
                fs::canonicalize(&config.flake_path)
                    .await
                    .map_err(|e| format!("Invalid path {}: {}", config.flake_path, e))?
                    .to_string_lossy()
            );

            let lock_file = Path::new(&config.flake_path).join("flake.lock");
            if !lock_file.exists() {
                info!("Generating flake.lock for {}", config.flake_path);
                let status = Command::new("nix")
                    .args(["flake", "update", "--flake", &flake_ref])
                    .status()
                    .await
                    .map_err(|e| format!("Failed to run nix flake update: {}", e))?;
                if !status.success() {
                    return Err("nix flake update failed".to_string());
                }
            }

            let mut targets = Vec::new();

            // 1. Packages
            let expr = format!("{}#packages.{}", flake_ref, system);
            let output = Command::new("nix")
                .args([
                    "eval",
                    &expr,
                    "--apply",
                    "attrs: builtins.concatStringsSep \"\\n\" (builtins.attrNames attrs)",
                    "--raw",
                ])
                .output()
                .await;
            if let Ok(out) = output
                && out.status.success()
            {
                let names = String::from_utf8_lossy(&out.stdout);
                for name in names.lines() {
                    if !name.trim().is_empty() {
                        targets.push(BuildTarget::Flake {
                            flake_ref: flake_ref.clone(),
                            attribute: format!("packages.{}.{}", system, name.trim()),
                        });
                    }
                }
            }

            // 2. NixOS Configurations
            let expr = format!("{}#nixosConfigurations", flake_ref);
            let output = Command::new("nix")
                .args([
                    "eval",
                    &expr,
                    "--apply",
                    "attrs: builtins.concatStringsSep \"\\n\" (builtins.attrNames attrs)",
                    "--raw",
                ])
                .output()
                .await;
            if let Ok(out) = output
                && out.status.success()
            {
                let names = String::from_utf8_lossy(&out.stdout);
                for name in names.lines() {
                    if !name.trim().is_empty() {
                        targets.push(BuildTarget::Flake {
                            flake_ref: flake_ref.clone(),
                            attribute: format!(
                                "nixosConfigurations.{}.config.system.build.toplevel",
                                name.trim()
                            ),
                        });
                    }
                }
            }

            // 3. DevShells
            let expr = format!("{}#devShells.{}", flake_ref, system);
            let output = Command::new("nix")
                .args([
                    "eval",
                    &expr,
                    "--apply",
                    "attrs: builtins.concatStringsSep \"\\n\" (builtins.attrNames attrs)",
                    "--raw",
                ])
                .output()
                .await;
            if let Ok(out) = output
                && out.status.success()
            {
                let names = String::from_utf8_lossy(&out.stdout);
                for name in names.lines() {
                    if !name.trim().is_empty() {
                        targets.push(BuildTarget::Flake {
                            flake_ref: flake_ref.clone(),
                            attribute: format!("devShells.{}.{}", system, name.trim()),
                        });
                    }
                }
            }

            if targets.is_empty() {
                return Err(format!(
                    "No buildable outputs found for {} in {}",
                    system, config.flake_path
                ));
            }

            Ok(targets)
        }
        BuildMode::NonFlake => {
            if !config.attributes.is_empty() {
                let targets = config
                    .attributes
                    .iter()
                    .map(|attr| BuildTarget::NonFlake {
                        file: config.file.clone(),
                        attribute: Some(attr.clone()),
                    })
                    .collect();
                Ok(targets)
            } else {
                Ok(vec![BuildTarget::NonFlake {
                    file: config.file.clone(),
                    attribute: None,
                }])
            }
        }
    }
}

pub async fn build_outputs(targets: &[BuildTarget]) -> Result<Vec<String>, String> {
    let mut all_paths = Vec::new();
    for target in targets {
        info!("Building target: {}", target);
        let mut cmd = Command::new("nix");
        cmd.arg("build");

        match target {
            BuildTarget::Flake {
                flake_ref,
                attribute,
            } => {
                cmd.arg(format!("{}#{}", flake_ref, attribute));
                cmd.args(["--no-link", "--accept-flake-config", "--json"]);
            }
            BuildTarget::NonFlake { file, attribute } => {
                cmd.args(["--file", file]);
                if let Some(attr) = attribute {
                    cmd.arg(attr);
                }
                cmd.args(["--no-link", "--json"]);
            }
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| format!("Failed to build target: {}", e))?;

        if !output.status.success() {
            error!(
                "nix build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            // Fallback to nix path-info
            let mut path_info_cmd = Command::new("nix");
            path_info_cmd.arg("path-info");
            match target {
                BuildTarget::Flake {
                    flake_ref,
                    attribute,
                } => {
                    path_info_cmd.arg(format!("{}#{}", flake_ref, attribute));
                }
                BuildTarget::NonFlake { file, attribute } => {
                    path_info_cmd.args(["--file", file]);
                    if let Some(attr) = attribute {
                        path_info_cmd.arg(attr);
                    }
                }
            }

            let path_info_out = path_info_cmd
                .output()
                .await
                .map_err(|e| format!("Fallback path-info failed: {}", e))?;
            if path_info_out.status.success() {
                let paths = String::from_utf8_lossy(&path_info_out.stdout);
                for p in paths.lines() {
                    if !p.trim().is_empty() {
                        all_paths.push(p.trim().to_string());
                    }
                }
                continue;
            }
            return Err(format!("Failed to build target: {}", target));
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        if let Ok(val) = serde_json::from_str::<Value>(&json_str) {
            if let Some(arr) = val.as_array() {
                for item in arr {
                    if let Some(outputs) = item.get("outputs").and_then(|o| o.as_object()) {
                        for val in outputs.values() {
                            if let Some(p) = val.as_str() {
                                all_paths.push(p.to_string());
                            }
                        }
                    }
                }
            }
        } else {
            return Err(format!(
                "Failed to parse build JSON output for target: {}",
                target
            ));
        }
    }

    Ok(all_paths)
}

pub async fn get_closure(paths: &[String]) -> Result<Vec<String>, String> {
    let mut cmd = Command::new("nix-store");
    cmd.args(["--query", "--requisites"]);
    for p in paths {
        cmd.arg(p);
    }

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("Failed to run nix-store query: {}", e))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let out_str = String::from_utf8_lossy(&output.stdout);
    let mut requisites: Vec<String> = out_str
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    requisites.sort();
    requisites.dedup();
    Ok(requisites)
}

pub async fn find_locally_built_paths(
    paths: &[String],
    own_hashes: &[String],
) -> Result<Vec<String>, String> {
    let mut cmd = Command::new("nix");
    cmd.args(["path-info", "--json", "--recursive"]);
    for p in paths {
        cmd.arg(p);
    }

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("Failed to run nix path-info: {}", e))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let parsed = serde_json::from_str::<Value>(&json_str)
        .map_err(|e| format!("Failed to parse path-info JSON: {}", e))?;

    let items = if let Some(arr) = parsed.as_array() {
        arr.clone()
    } else if let Some(obj) = parsed.as_object() {
        // Handle path-keyed dictionary by merging key and value
        let mut list = Vec::new();
        for (path, val) in obj {
            let mut item = val.clone();
            if let Some(item_obj) = item.as_object_mut() {
                item_obj.insert("path".to_string(), Value::String(path.clone()));
            }
            list.push(item);
        }
        list
    } else {
        return Err("Unexpected path-info JSON format".to_string());
    };

    let mut locally_built = Vec::new();
    for item in items {
        if let Some(path) = item.get("path").and_then(|p| p.as_str()) {
            let sigs = item
                .get("signatures")
                .or_else(|| item.get("sigs"))
                .and_then(|s| s.as_array());

            // If signatures array is empty or missing, it's built locally
            let has_sig = match sigs {
                None => false,
                Some(arr) => !arr.is_empty(),
            };

            if !has_sig
                && let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str())
                && name.len() >= 32
            {
                let hash = &name[..32];
                if !own_hashes.iter().any(|h| h == hash) {
                    locally_built.push(path.to_string());
                }
            }
        }
    }

    locally_built.sort();
    locally_built.dedup();
    Ok(locally_built)
}

pub async fn export_paths_directly(
    paths: &[String],
    signing_key_file: Option<&str>,
    cache_dir: &Path,
) -> Result<Vec<(String, String)>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let nar_dir = cache_dir.join("nar");
    fs::create_dir_all(&nar_dir)
        .await
        .map_err(|e| format!("Failed to create nar directory: {}", e))?;

    if let Some(key) = signing_key_file {
        info!("Signing {} store paths", paths.len());
        let mut cmd = Command::new("nix");
        cmd.args(["store", "sign", "--key-file", key]);
        for p in paths {
            cmd.arg(p);
        }
        let status = cmd
            .status()
            .await
            .map_err(|e| format!("Failed to sign store paths: {}", e))?;
        if !status.success() {
            info!("Signing command returned non-zero status, continuing...");
        }
    }

    info!(
        "Exporting {} store paths (direct NAR dump, skipping full closure)",
        paths.len()
    );

    // Limit concurrency using semaphore to avoid running out of file descriptors or system limits
    let concurrency_limit = num_cpus::get().max(2);
    let sem = Arc::new(Semaphore::new(concurrency_limit));
    let mut tasks = Vec::new();

    for store_path in paths {
        let store_path = store_path.clone();
        let nar_dir = nar_dir.clone();
        let sem = sem.clone();

        let task = tokio::spawn(async move {
            let _permit = sem.acquire().await.map_err(|e| e.to_string())?;

            let file_name = Path::new(&store_path)
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| format!("Invalid store path: {}", store_path))?;

            if file_name.len() < 32 {
                return Err(format!("Path name too short: {}", file_name));
            }
            let hash = &file_name[..32];
            let nar_file_path = nar_dir.join(format!("{}.nar.xz", hash));

            // 1. nix-store --dump <path> | xz -1 > <file>
            let mut dump_proc = std::process::Command::new("nix-store")
                .args(["--dump", &store_path])
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("Failed to spawn nix-store --dump: {}", e))?;

            let dump_stdout = dump_proc
                .stdout
                .take()
                .ok_or_else(|| "Failed to capture nix-store stdout".to_string())?;

            let nar_file = std::fs::File::create(&nar_file_path)
                .map_err(|e| format!("Failed to create nar file: {}", e))?;

            let mut xz_proc = std::process::Command::new("xz")
                .arg("-1")
                .stdin(dump_stdout)
                .stdout(nar_file)
                .spawn()
                .map_err(|e| format!("Failed to spawn xz: {}", e))?;

            let dump_status = dump_proc
                .wait()
                .map_err(|e| format!("nix-store failed: {}", e))?;
            let xz_status = xz_proc.wait().map_err(|e| format!("xz failed: {}", e))?;

            if !dump_status.success() || !xz_status.success() {
                return Err(format!(
                    "Export/compress pipeline failed for {}",
                    store_path
                ));
            }

            // 2. Compute size and nix hash
            let metadata = fs::metadata(&nar_file_path)
                .await
                .map_err(|e| format!("Failed to read metadata: {}", e))?;
            let file_size = metadata.len();

            let hash_output = Command::new("nix")
                .args([
                    "hash",
                    "file",
                    "--type",
                    "sha256",
                    "--base32",
                    &nar_file_path.to_string_lossy(),
                ])
                .output()
                .await
                .map_err(|e| format!("Failed to run nix hash: {}", e))?;

            if !hash_output.status.success() {
                return Err(format!(
                    "nix hash failed: {}",
                    String::from_utf8_lossy(&hash_output.stderr)
                ));
            }
            let file_hash = String::from_utf8_lossy(&hash_output.stdout)
                .trim()
                .to_string();

            // 3. Get path info for narinfo metadata
            let path_info_out = Command::new("nix")
                .args(["path-info", "--json", &store_path])
                .output()
                .await
                .map_err(|e| format!("Failed to run nix path-info for {}: {}", store_path, e))?;

            if !path_info_out.status.success() {
                return Err(format!(
                    "nix path-info failed: {}",
                    String::from_utf8_lossy(&path_info_out.stderr)
                ));
            }

            let path_info_json = String::from_utf8_lossy(&path_info_out.stdout);
            let parsed_info = serde_json::from_str::<Value>(&path_info_json)
                .map_err(|e| format!("Failed to parse path info: {}", e))?;

            let info = if let Some(arr) = parsed_info.as_array() {
                arr.first()
                    .ok_or_else(|| "Empty path-info array".to_string())?
                    .clone()
            } else if let Some(obj) = parsed_info.as_object() {
                obj.get(&store_path)
                    .ok_or_else(|| "Path not found in path-info object".to_string())?
                    .clone()
            } else {
                return Err("Unexpected path-info structure".to_string());
            };

            let nar_hash = info
                .get("narHash")
                .and_then(|h| h.as_str())
                .unwrap_or_default()
                .to_string();
            let nar_size = info.get("narSize").and_then(|s| s.as_u64()).unwrap_or(0);
            let references = info.get("references").and_then(|r| r.as_array());
            let deriver = info
                .get("deriver")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string();
            let signatures = info
                .get("signatures")
                .or_else(|| info.get("sigs"))
                .and_then(|s| s.as_array());

            let mut ref_basenames = Vec::new();
            if let Some(refs_arr) = references {
                for r_val in refs_arr {
                    if let Some(r_str) = r_val.as_str()
                        && let Some(bname) = Path::new(r_str).file_name().and_then(|n| n.to_str())
                    {
                        ref_basenames.push(bname.to_string());
                    }
                }
            }
            let ref_names = ref_basenames.join(" ");

            let mut lines = vec![
                format!("StorePath: {}", store_path),
                format!("URL: nar/{}.nar.xz", hash),
                "Compression: xz".to_string(),
                format!("FileHash: sha256:{}", file_hash),
                format!("FileSize: {}", file_size),
                format!("NarHash: {}", nar_hash),
                format!("NarSize: {}", nar_size),
            ];

            if !ref_names.is_empty() {
                lines.push(format!("References: {}", ref_names));
            }
            if !deriver.is_empty()
                && let Some(deriver_bname) =
                    Path::new(&deriver).file_name().and_then(|n| n.to_str())
            {
                lines.push(format!("Deriver: {}", deriver_bname));
            }

            if let Some(sigs_arr) = signatures {
                for sig_val in sigs_arr {
                    if let Some(sig_str) = sig_val.as_str() {
                        lines.push(format!("Sig: {}", sig_str));
                    }
                }
            }

            let narinfo_content = lines.join("\n") + "\n";
            let narinfo_path = nar_dir
                .parent()
                .ok_or("No parent dir")?
                .join(format!("{}.narinfo", hash));
            fs::write(&narinfo_path, narinfo_content)
                .await
                .map_err(|e| format!("Failed to write narinfo: {}", e))?;

            info!("  Exported {} ({} bytes)", hash, file_size);
            Ok((hash.to_string(), store_path))
        });

        tasks.push(task);
    }

    let mut results = Vec::new();
    for t in tasks {
        match t.await {
            Ok(Ok(pair)) => results.push(pair),
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(format!("Task panicked: {}", e)),
        }
    }

    Ok(results)
}
