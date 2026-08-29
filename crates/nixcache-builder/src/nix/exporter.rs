use crate::error::BuilderError;
use async_compression::tokio::write::XzEncoder;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io,
    path::Path,
    pin::Pin,
    process::Stdio,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    fs,
    io::{AsyncWrite, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
};
use tracing::info;

/// 异步哈希与字节计数写入器
pub struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes_written: u64,
}

impl<W> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes_written: 0,
        }
    }

    pub fn into_inner_and_digest(self) -> (W, String, u64) {
        let hash = self.hasher.finalize();
        let hex = hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();
        (self.inner, hex, self.bytes_written)
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for HashingWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.hasher.update(&buf[..n]);
                self.bytes_written += n as u64;
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// 原生流式导出 store paths 并生成 .nar.xz 与 .narinfo
pub async fn export_paths_directly(
    paths: &[String],
    signing_key_file: Option<&str>,
    cache_dir: &Path,
) -> Result<Vec<(String, String)>, BuilderError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let nar_dir = cache_dir.join("nar");
    fs::create_dir_all(&nar_dir).await?;

    if let Some(key) = signing_key_file {
        info!("Signing {} store paths", paths.len());
        let mut cmd = Command::new("nix");
        cmd.args(["store", "sign", "--key-file", key]);
        for p in paths {
            cmd.arg(p);
        }
        let status = cmd.status().await?;
        if !status.success() {
            info!("Signing command returned non-zero status, continuing...");
        }
    }

    info!(
        "Exporting {} store paths via native async streaming pipeline",
        paths.len()
    );

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

            // 1. 原生流式导出与压缩：nix-store --dump -> Async XzEncoder -> HashingWriter
            let mut dump_proc = Command::new("nix-store")
                .args(["--dump", &store_path])
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("Failed to spawn nix-store --dump: {}", e))?;

            let mut dump_stdout = dump_proc
                .stdout
                .take()
                .ok_or_else(|| "Failed to capture nix-store stdout".to_string())?;

            let nar_file = fs::File::create(&nar_file_path)
                .await
                .map_err(|e| format!("Failed to create nar file: {}", e))?;

            let hashing_writer = HashingWriter::new(nar_file);
            let mut encoder = XzEncoder::new(hashing_writer);

            tokio::io::copy(&mut dump_stdout, &mut encoder)
                .await
                .map_err(|e| format!("Failed to compress NAR stream: {}", e))?;

            encoder
                .shutdown()
                .await
                .map_err(|e| format!("Failed to flush encoder: {}", e))?;

            let hashing_writer = encoder.into_inner();
            let (mut file, file_hash_hex, file_size) = hashing_writer.into_inner_and_digest();
            file.flush()
                .await
                .map_err(|e| format!("Failed to flush file: {}", e))?;

            let dump_status = dump_proc
                .wait()
                .await
                .map_err(|e| format!("nix-store dump wait failed: {}", e))?;

            if !dump_status.success() {
                return Err(format!("nix-store dump failed for {}", store_path));
            }

            // 2. 获取 path-info 构建 narinfo 元数据
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
                format!("FileHash: sha256:{}", file_hash_hex),
                format!("FileSize: {}", file_size),
                format!("NarHash: {}", nar_hash),
                format!("NarSize: {}", nar_size),
            ];

            if !ref_names.is_empty() {
                lines.push(format!("References: {}", ref_names));
            }
            if !deriver.is_empty()
                && let Some(deriver_bname) = Path::new(&deriver).file_name().and_then(|n| n.to_str())
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
                .ok_or_else(|| "No parent dir".to_string())?
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
            Ok(Err(e)) => return Err(BuilderError::NixCli(e)),
            Err(e) => return Err(BuilderError::Other(format!("Task panicked: {}", e))),
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::HashingWriter;
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_hashing_writer_computes_sha256_and_size() {
        let buffer = Vec::new();
        let cursor = Cursor::new(buffer);
        let mut writer = HashingWriter::new(cursor);

        let data1 = b"Hello, ";
        let data2 = b"NixCache OCI Native Stream!";

        writer.write_all(data1).await.unwrap();
        writer.write_all(data2).await.unwrap();
        writer.flush().await.unwrap();

        let (cursor, digest_hex, bytes_written) = writer.into_inner_and_digest();
        assert_eq!(bytes_written, (data1.len() + data2.len()) as u64);
        assert_eq!(cursor.into_inner(), b"Hello, NixCache OCI Native Stream!");

        let mut hasher = Sha256::new();
        hasher.update(b"Hello, NixCache OCI Native Stream!");
        let expected_hex = hasher
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        assert_eq!(digest_hex, expected_hex);
    }
}
