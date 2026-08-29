use crate::error::BuilderError;
use async_compression::tokio::write::XzEncoder;
use chrono::Utc;
use futures_util::{StreamExt, TryStreamExt, stream};
use nixcache_core::{IndexEntry, NarDigest, NarInfoMeta, StoreHash, SystemArch};
use nixcache_oci::{OciClient, TransportError, UploadConfig};
use nixcache_oci_backend::ReqwestTransport;
use serde_json::Value;
use std::{
    collections::HashMap,
    io,
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::Command,
};
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};

/// 原生流式单向复制数据并写入压缩编码器。
///
/// 避免使用通用 `tokio::io::copy`，因为其在 Reader 返回 `Poll::Pending` 时会默认调用
/// `writer.poll_flush()`，而 `liblzma` 在 `LZMA_SYNC_FLUSH` 后无法无缝切换回 `LZMA_RUN`，
/// 进而导致 `liblzma internal error`。本函数采用 64KB 缓冲区进行流式传输，
/// 仅在流完全结束（EOF）后由调用方执行最终的 `shutdown`。
pub async fn copy_nar_stream<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 64 * 1024];
    let mut total_bytes = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).await?;
        total_bytes += n as u64;
    }
    Ok(total_bytes)
}

/// 并行导出与上传配置
#[derive(Clone, Debug)]
pub struct ParallelExportConfig {
    /// 最大并发 Worker 数
    pub concurrency: usize,
    /// 签名私钥文件路径
    pub signing_key_file: Option<String>,
    /// 是否在遇到单个产物导出失败时立即中止退出
    pub fail_fast: bool,
    /// OCI 上传底层配置
    pub upload_config: UploadConfig,
    /// 目标平台系统架构
    pub system: SystemArch,
    /// 来源 Job 标识符
    pub origin_job: Option<String>,
}

impl Default for ParallelExportConfig {
    fn default() -> Self {
        Self {
            concurrency: num_cpus::get().clamp(2, 8),
            signing_key_file: None,
            fail_fast: false,
            upload_config: UploadConfig::default(),
            system: SystemArch::from("x86_64-linux"),
            origin_job: None,
        }
    }
}

/// 单个 Store Path 导出上传完成后的元数据
#[derive(Clone, Debug)]
pub struct ExportedStorePath {
    pub store_hash: StoreHash,
    pub index_entry: IndexEntry,
    pub file_size: u64,
}

/// 并行导出总览报告
#[derive(Clone, Debug, Default)]
pub struct ParallelExportReport {
    pub successful: Vec<ExportedStorePath>,
    pub failed: Vec<(String, String)>,
    pub total_bytes_uploaded: u64,
    pub elapsed: Duration,
}

/// 预查的单路径原始元数据结构
#[derive(Clone, Debug, Default)]
pub struct RawPathInfo {
    pub nar_hash: String,
    pub nar_size: u64,
    pub references: Vec<String>,
    pub deriver: Option<String>,
    pub signatures: Vec<String>,
    pub ca: Option<String>,
}

/// 高性能无盘流式并行 Export 协调器
pub struct ParallelExporter;

impl ParallelExporter {
    /// 批量预处理、端到端无盘流式并行导出并上传
    pub async fn export_and_upload_paths(
        paths: &[String],
        oci_client: &OciClient<ReqwestTransport>,
        config: &ParallelExportConfig,
    ) -> Result<ParallelExportReport, BuilderError> {
        if paths.is_empty() {
            return Ok(ParallelExportReport::default());
        }

        let start_time = Instant::now();
        info!(
            "Starting parallel export for {} paths (concurrency: {}, fail_fast: {})",
            paths.len(),
            config.concurrency,
            config.fail_fast
        );

        // 1. 批量签名 (按批次调用，防止超出命令行长度限制)
        if let Some(ref key) = config.signing_key_file {
            Self::batch_sign_paths(paths, key).await?;
        }

        // 2. 批量预查元数据并构建内存字典
        let info_map = Arc::new(Self::batch_fetch_path_infos(paths).await?);

        // 3. 执行流式无盘并发池
        let report =
            Self::execute_parallel_pipeline(paths, oci_client, config, info_map, start_time)
                .await?;

        info!(
            "Parallel export finished in {:.2?}: {} succeeded, {} failed, {} bytes uploaded",
            report.elapsed,
            report.successful.len(),
            report.failed.len(),
            report.total_bytes_uploaded
        );

        Ok(report)
    }

    /// 分批执行 `nix store sign` 签名
    pub async fn batch_sign_paths(paths: &[String], key_file: &str) -> Result<(), BuilderError> {
        info!("Batch signing {} store paths", paths.len());
        const BATCH_SIZE: usize = 128;

        for chunk in paths.chunks(BATCH_SIZE) {
            let mut cmd = Command::new("nix");
            cmd.args(["store", "sign", "--key-file", key_file]);
            for p in chunk {
                cmd.arg(p);
            }
            let status = cmd.status().await?;
            if !status.success() {
                warn!("Signing command returned non-zero status, continuing...");
            }
        }
        Ok(())
    }

    /// 分批批量查询 `nix path-info --json` 元数据
    pub async fn batch_fetch_path_infos(
        paths: &[String],
    ) -> Result<HashMap<String, RawPathInfo>, BuilderError> {
        info!("Batch querying path-info for {} paths", paths.len());
        let mut map = HashMap::new();
        const BATCH_SIZE: usize = 128;

        for chunk in paths.chunks(BATCH_SIZE) {
            let mut cmd = Command::new("nix");
            cmd.args(["path-info", "--json"]);
            for p in chunk {
                cmd.arg(p);
            }

            let output = cmd.output().await?;
            if !output.status.success() {
                let err_msg = String::from_utf8_lossy(&output.stderr).to_string();
                return Err(BuilderError::NixCli(format!(
                    "nix path-info failed: {}",
                    err_msg
                )));
            }

            let json_str = String::from_utf8_lossy(&output.stdout);
            let parsed: Value = serde_json::from_str(&json_str)?;
            let chunk_map = Self::parse_path_info_json(&parsed)?;
            map.extend(chunk_map);
        }

        Ok(map)
    }

    /// 解析 `nix path-info --json` 输出（支持 Array 格式与 Map 格式）
    pub fn parse_path_info_json(
        parsed: &Value,
    ) -> Result<HashMap<String, RawPathInfo>, BuilderError> {
        let mut result = HashMap::new();

        if let Some(arr) = parsed.as_array() {
            for item in arr {
                if let Some(path) = item.get("path").and_then(|p| p.as_str()) {
                    let info = Self::extract_single_raw_path_info(item);
                    result.insert(path.to_string(), info);
                }
            }
        } else if let Some(obj) = parsed.as_object() {
            for (path, item) in obj {
                let info = Self::extract_single_raw_path_info(item);
                result.insert(path.clone(), info);
            }
        } else {
            return Err(BuilderError::NixCli(
                "Unexpected path-info JSON format".to_string(),
            ));
        }

        Ok(result)
    }

    fn extract_single_raw_path_info(item: &Value) -> RawPathInfo {
        let nar_hash = item
            .get("narHash")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        let nar_size = item.get("narSize").and_then(|s| s.as_u64()).unwrap_or(0);
        let deriver = item
            .get("deriver")
            .and_then(|d| d.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let ca = item
            .get("ca")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let mut references = Vec::new();
        if let Some(refs_arr) = item.get("references").and_then(|r| r.as_array()) {
            for r_val in refs_arr {
                if let Some(r_str) = r_val.as_str() {
                    let bname = Path::new(r_str)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(r_str);
                    references.push(bname.to_string());
                }
            }
        }

        let mut signatures = Vec::new();
        let sigs_val = item.get("signatures").or_else(|| item.get("sigs"));
        if let Some(sigs_arr) = sigs_val.and_then(|s| s.as_array()) {
            for sig_val in sigs_arr {
                if let Some(sig_str) = sig_val.as_str() {
                    signatures.push(sig_str.to_string());
                }
            }
        }

        RawPathInfo {
            nar_hash,
            nar_size,
            references,
            deriver,
            signatures,
            ca,
        }
    }

    /// 执行单路径端到端无盘流式导出并推送到 OCI Registry
    pub async fn export_single_path_stream(
        store_path: &str,
        oci_client: &OciClient<ReqwestTransport>,
        upload_config: &UploadConfig,
        path_info: &RawPathInfo,
        system: SystemArch,
        origin_job: Option<&str>,
    ) -> Result<ExportedStorePath, BuilderError> {
        let file_name = Path::new(store_path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| BuilderError::Other(format!("Invalid store path: {}", store_path)))?;

        if file_name.len() < 32 {
            return Err(BuilderError::Other(format!(
                "Path name too short: {}",
                file_name
            )));
        }
        let hash = &file_name[..32];

        let mut dump_proc = Command::new("nix-store")
            .args(["--dump", store_path])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| BuilderError::Other(format!("Failed to spawn nix-store --dump: {}", e)))?;

        let dump_stdout = dump_proc
            .stdout
            .take()
            .ok_or_else(|| BuilderError::Other("Failed to capture nix-store stdout".to_string()))?;

        let (reader, writer) = tokio::io::duplex(8 * 1024 * 1024);

        let compress_handle = tokio::spawn(async move {
            let mut encoder = XzEncoder::new(writer);
            let mut reader = dump_stdout;
            let res = copy_nar_stream(&mut reader, &mut encoder).await;
            let shutdown_res = encoder.shutdown().await;
            if let Err(e) = res {
                return Err(format!("Compression copy failed: {}", e));
            }
            if let Err(e) = shutdown_res {
                return Err(format!("Encoder shutdown failed: {}", e));
            }
            Ok(())
        });

        let reader_stream = ReaderStream::new(reader);
        let oci_stream = Box::pin(reader_stream.map_err(TransportError::Io));

        let (nar_digest, nar_size) = oci_client
            .push_blob_streaming_resumable(oci_stream, upload_config)
            .await
            .map_err(|e| BuilderError::Other(format!("Streaming upload failed: {}", e)))?;

        match compress_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(BuilderError::Other(e)),
            Err(e) => {
                return Err(BuilderError::Other(format!(
                    "Compression task join error: {}",
                    e
                )));
            }
        }

        let dump_status = dump_proc
            .wait()
            .await
            .map_err(|e| BuilderError::Other(format!("nix-store dump wait failed: {}", e)))?;

        if !dump_status.success() {
            return Err(BuilderError::Other(format!(
                "nix-store dump failed for {}",
                store_path
            )));
        }

        // 构造 NarInfo 与 IndexEntry
        let raw_hash = nar_digest.strip_prefix("sha256:").unwrap_or(&nar_digest);
        let deriver_bname = path_info.deriver.as_deref().and_then(|d| {
            Path::new(d)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        });

        let narinfo_meta = NarInfoMeta {
            store_path: store_path.to_string(),
            nar_basename: format!("{}.nar.xz", hash),
            compression: Some("xz".to_string()),
            file_hash: Some(format!("sha256:{}", raw_hash)),
            file_size: Some(nar_size),
            nar_hash: path_info.nar_hash.clone(),
            references: path_info.references.clone(),
            deriver: deriver_bname,
            signatures: path_info.signatures.clone(),
            ca: path_info.ca.clone(),
        };

        let store_hash = StoreHash::parse(hash).unwrap_or_else(|_| StoreHash::new_unchecked(hash));
        let nar_digest_obj =
            NarDigest::parse(&nar_digest).unwrap_or_else(|_| NarDigest::new_unchecked(&nar_digest));

        let name = Path::new(store_path)
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.split_once('-'))
            .map(|x| x.1.to_string())
            .unwrap_or_else(|| hash.to_string());

        let index_entry = IndexEntry {
            name,
            system: Some(system),
            narinfo_meta,
            nar_digest: nar_digest_obj,
            nar_size: nar_size.max(path_info.nar_size),
            added: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            origin_job: origin_job.map(|s| s.to_string()),
        };

        Ok(ExportedStorePath {
            store_hash,
            index_entry,
            file_size: nar_size,
        })
    }

    /// 执行并发流式管道调度
    async fn execute_parallel_pipeline(
        paths: &[String],
        oci_client: &OciClient<ReqwestTransport>,
        config: &ParallelExportConfig,
        info_map: Arc<HashMap<String, RawPathInfo>>,
        start_time: Instant,
    ) -> Result<ParallelExportReport, BuilderError> {
        let concurrency = config.concurrency.max(1);
        let paths_stream = stream::iter(paths.iter().cloned().map(|path| {
            let oci_client = oci_client.clone();
            let upload_config = config.upload_config.clone();
            let path_info = info_map.get(&path).cloned().unwrap_or_default();
            let system = config.system;
            let origin_job = config.origin_job.clone();

            async move {
                let res = Self::export_single_path_stream(
                    &path,
                    &oci_client,
                    &upload_config,
                    &path_info,
                    system,
                    origin_job.as_deref(),
                )
                .await;
                (path, res)
            }
        }));

        let mut buffered = paths_stream.buffer_unordered(concurrency);
        let mut report = ParallelExportReport::default();

        while let Some((path, result)) = buffered.next().await {
            match result {
                Ok(exported) => {
                    info!(
                        "  [OK] Exported & uploaded {} ({} bytes)",
                        exported.index_entry.name, exported.file_size
                    );
                    report.total_bytes_uploaded += exported.file_size;
                    report.successful.push(exported);
                }
                Err(e) => {
                    error!("  [FAIL] Failed to export {}: {}", path, e);
                    if config.fail_fast {
                        return Err(e);
                    }
                    report.failed.push((path, e.to_string()));
                }
            }
        }

        report.elapsed = start_time.elapsed();
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::{ExportedStorePath, ParallelExporter, copy_nar_stream};
    use async_compression::tokio::{bufread::XzDecoder, write::XzEncoder};
    use nixcache_core::{IndexEntry, NarDigest, StoreHash, SystemArch};
    use serde_json::json;
    use std::{
        io::{self},
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};

    #[test]
    fn test_parse_path_info_json_array_and_object() {
        let array_json = json!([
            {
                "path": "/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-test-pkg",
                "narHash": "sha256:1111",
                "narSize": 1024,
                "references": ["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep"],
                "deriver": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-test.drv",
                "signatures": ["sig1", "sig2"]
            }
        ]);

        let map = ParallelExporter::parse_path_info_json(&array_json).unwrap();
        assert_eq!(map.len(), 1);
        let item = map
            .get("/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-test-pkg")
            .unwrap();
        assert_eq!(item.nar_hash, "sha256:1111");
        assert_eq!(item.nar_size, 1024);
        assert_eq!(
            item.references,
            vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep"]
        );
        assert_eq!(
            item.deriver.as_deref(),
            Some("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-test.drv")
        );
        assert_eq!(item.signatures, vec!["sig1", "sig2"]);

        let object_json = json!({
            "/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-test-pkg": {
                "narHash": "sha256:2222",
                "narSize": 2048,
                "references": []
            }
        });

        let map2 = ParallelExporter::parse_path_info_json(&object_json).unwrap();
        assert_eq!(map2.len(), 1);
        let item2 = map2
            .get("/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-test-pkg")
            .unwrap();
        assert_eq!(item2.nar_hash, "sha256:2222");
        assert_eq!(item2.nar_size, 2048);
    }

    #[test]
    fn test_exported_store_path_structure() {
        let store_hash = StoreHash::new_unchecked("s66mzxpvicwk07gjbjfw9izjfa797vsw");
        let nar_digest = NarDigest::new_unchecked(
            "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let meta = nixcache_core::NarInfoMeta {
            store_path: "/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-test".to_string(),
            nar_basename: "s66mzxpvicwk07gjbjfw9izjfa797vsw.nar.xz".to_string(),
            compression: Some("xz".to_string()),
            file_hash: Some(
                "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .to_string(),
            ),
            file_size: Some(100),
            nar_hash: "sha256:2222".to_string(),
            references: vec![],
            deriver: None,
            signatures: vec![],
            ca: None,
        };

        let index_entry = IndexEntry {
            name: "test".to_string(),
            system: Some(SystemArch::from("x86_64-linux")),
            narinfo_meta: meta,
            nar_digest: nar_digest.clone(),
            nar_size: 200,
            added: "2026-08-29T00:00:00Z".to_string(),
            origin_job: None,
        };

        let exported = ExportedStorePath {
            store_hash: store_hash.clone(),
            index_entry,
            file_size: 100,
        };

        assert_eq!(
            exported.store_hash.as_str(),
            "s66mzxpvicwk07gjbjfw9izjfa797vsw"
        );
        assert_eq!(exported.file_size, 100);
        assert_eq!(exported.index_entry.name, "test");
    }

    struct IntermittentPendingReader {
        chunks: Vec<Vec<u8>>,
        index: usize,
        yielded_pending: bool,
    }

    impl AsyncRead for IntermittentPendingReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.index >= self.chunks.len() {
                return Poll::Ready(Ok(()));
            }

            if !self.yielded_pending {
                self.yielded_pending = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }

            self.yielded_pending = false;
            let chunk = &self.chunks[self.index];
            buf.put_slice(chunk);
            self.index += 1;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn test_copy_nar_stream_prevents_liblzma_flush_conflict_regression() {
        let mut original_data = Vec::new();
        let mut chunks = Vec::new();
        for i in 0..10 {
            let chunk =
                format!("Chunk data payload section {} - {}\n", i, "X".repeat(1024)).into_bytes();
            original_data.extend_from_slice(&chunk);
            chunks.push(chunk);
        }

        let mut pending_reader = IntermittentPendingReader {
            chunks,
            index: 0,
            yielded_pending: false,
        };

        let compressed_buf = Vec::new();
        let mut encoder = XzEncoder::new(compressed_buf);

        let copied_bytes = copy_nar_stream(&mut pending_reader, &mut encoder)
            .await
            .expect("copy_nar_stream should succeed without liblzma error");
        assert_eq!(copied_bytes, original_data.len() as u64);

        encoder
            .shutdown()
            .await
            .expect("Encoder shutdown should succeed");

        let compressed = encoder.into_inner();
        assert!(!compressed.is_empty());

        let mut decoder = XzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .await
            .expect("Decompression should succeed");
        assert_eq!(decompressed, original_data);
    }

    #[tokio::test]
    async fn test_parallel_exporter_empty_paths() {
        let server = wiremock::MockServer::start().await;
        let oci = nixcache_oci_backend::create_tokio_reqwest_client(
            &server.address().to_string(),
            "test/repo",
            "",
            true,
        );
        let config = super::ParallelExportConfig::default();
        let report = ParallelExporter::export_and_upload_paths(&[], &oci, &config)
            .await
            .unwrap();
        assert!(report.successful.is_empty());
        assert!(report.failed.is_empty());
        assert_eq!(report.total_bytes_uploaded, 0);
    }

    #[tokio::test]
    async fn test_parallel_exporter_real_store_mock_oci() {
        let nix_store_available = tokio::process::Command::new("nix-store")
            .arg("--version")
            .output()
            .await
            .is_ok_and(|o| o.status.success());

        if !nix_store_available {
            return;
        }

        let mut paths = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir("/nix/store").await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str()
                    && !name.ends_with(".drv")
                    && !name.ends_with(".lock")
                {
                    paths.push(format!("/nix/store/{}", name));
                    if paths.len() >= 3 {
                        break;
                    }
                }
            }
        }

        if paths.is_empty() {
            return;
        }

        let server = wiremock::MockServer::start().await;
        let host = server.address().to_string();

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v2/test/repo/nix-cache/blobs/uploads/",
            ))
            .respond_with(wiremock::ResponseTemplate::new(202).insert_header(
                "Location",
                "/v2/test/repo/nix-cache/blobs/uploads/session-mock",
            ))
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .respond_with(wiremock::ResponseTemplate::new(202).insert_header("Range", "0-1048575"))
            .mount(&server)
            .await;

        let oci = nixcache_oci_backend::create_tokio_reqwest_client(&host, "test/repo", "", true);
        let config = super::ParallelExportConfig {
            concurrency: 4,
            signing_key_file: None,
            fail_fast: true,
            upload_config: nixcache_oci::UploadConfig::default(),
            system: nixcache_core::SystemArch::from("x86_64-linux"),
            origin_job: Some("job:test".to_string()),
        };

        let report = ParallelExporter::export_and_upload_paths(&paths, &oci, &config)
            .await
            .expect("Parallel export should succeed");

        assert_eq!(report.successful.len(), paths.len());
        assert!(report.failed.is_empty());
        assert!(report.total_bytes_uploaded > 0);
    }
}
