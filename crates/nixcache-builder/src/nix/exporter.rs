use crate::{
    error::BuilderError,
    nix::{driver::parse_path_info_items_typed, filter::NixPathInfoItem},
};
use async_compression::tokio::write::ZstdEncoder;
use chrono::Utc;
use futures_util::{StreamExt, TryStreamExt, stream};
use nixcache_core::{IndexEntry, NarDigest, NarInfoMeta, StoreHash, SystemArch};
use nixcache_oci::{OciClient, TransportError, UploadConfig};
use nixcache_oci_backend::ReqwestTransport;
use std::{
    io,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::Command,
};
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};

/// 导出管道环形缓冲区大小 (256KB)
pub const DUPLEX_BUFFER_SIZE: usize = 256 * 1024;

/// 原生流式单向复制数据并写入压缩编码器。
///
/// 避免使用通用 `tokio::io::copy` 在 Reader 产生 `Poll::Pending` 时频繁触发 flush。
/// 本函数采用 64KB 缓冲区进行流式传输，仅在流完全结束（EOF）后由调用方执行最终的 `shutdown`。
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
    pub strict: bool,
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
            strict: false,
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

        // 1. 批量签名 (按批次调用，防止超出命令行长度限制)
        if let Some(ref key) = config.signing_key_file {
            Self::batch_sign_paths(paths, key).await?;
        }

        // 2. 批量预查强类型元数据
        let items = Self::batch_fetch_path_infos_typed(paths, 128).await?;

        // 3. 执行流式无盘并发池
        let mut modified_config = config.clone();
        modified_config.signing_key_file = None;
        Self::export_and_upload_paths_with_preinfo(&items, oci_client, &modified_config).await
    }

    /// 消费过滤后的强类型 NixPathInfoItem，端到端无盘流式并行导出并上传
    pub async fn export_and_upload_paths_with_preinfo(
        items: &[NixPathInfoItem],
        oci_client: &OciClient<ReqwestTransport>,
        config: &ParallelExportConfig,
    ) -> Result<ParallelExportReport, BuilderError> {
        if items.is_empty() {
            return Ok(ParallelExportReport::default());
        }

        let start_time = Instant::now();
        info!(
            "Starting parallel export with pre-info for {} items (concurrency: {}, strict: {})",
            items.len(),
            config.concurrency,
            config.strict
        );

        if let Some(ref key) = config.signing_key_file {
            let paths: Vec<String> = items.iter().map(|i| i.path.clone()).collect();
            Self::batch_sign_paths(&paths, key).await?;
        }

        let concurrency = config.concurrency.max(1);
        let items_stream = stream::iter(items.iter().cloned().map(|item| {
            let oci_client = oci_client.clone();
            let upload_config = config.upload_config.clone();
            let system = config.system;
            let origin_job = config.origin_job.clone();

            async move {
                let res = Self::export_single_item_stream(
                    &item,
                    &oci_client,
                    &upload_config,
                    system,
                    origin_job.as_deref(),
                )
                .await;
                (item.path.clone(), res)
            }
        }));

        let mut buffered = items_stream.buffer_unordered(concurrency);
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
                    if config.strict {
                        return Err(e);
                    }
                    report.failed.push((path, e.to_string()));
                }
            }
        }

        report.elapsed = start_time.elapsed();
        info!(
            "Parallel export finished in {:.2?}: {} succeeded, {} failed, {} bytes uploaded",
            report.elapsed,
            report.successful.len(),
            report.failed.len(),
            report.total_bytes_uploaded
        );
        Ok(report)
    }

    /// 批量查询强类型 NixPathInfoItem 元数据
    pub async fn batch_fetch_path_infos_typed(
        paths: &[String],
        batch_size: usize,
    ) -> Result<Vec<NixPathInfoItem>, BuilderError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let bsize = if batch_size == 0 { 128 } else { batch_size };
        info!(
            "Batch querying typed path-info for {} paths (batch size: {})",
            paths.len(),
            bsize
        );
        let mut all_items = Vec::with_capacity(paths.len());

        for chunk in paths.chunks(bsize) {
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
            let items = parse_path_info_items_typed(&json_str)?;
            all_items.extend(items);
        }

        Ok(all_items)
    }

    /// 执行强类型单个产物端到端无盘流式导出并推送到 OCI Registry
    pub async fn export_single_item_stream(
        item: &NixPathInfoItem,
        oci_client: &OciClient<ReqwestTransport>,
        upload_config: &UploadConfig,
        system: SystemArch,
        origin_job: Option<&str>,
    ) -> Result<ExportedStorePath, BuilderError> {
        let store_path = &item.path;
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

        let (reader, writer) = tokio::io::duplex(DUPLEX_BUFFER_SIZE);

        let compress_handle = tokio::spawn(async move {
            let mut encoder =
                ZstdEncoder::with_quality(writer, async_compression::Level::Precise(3));
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
        let deriver_bname = item.deriver.as_deref().and_then(|d| {
            Path::new(d)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        });

        let narinfo_meta = NarInfoMeta {
            store_path: store_path.to_string(),
            nar_basename: format!("{}.nar.zst", hash),
            compression: Some("zstd".to_string()),
            file_hash: Some(format!("sha256:{}", raw_hash)),
            file_size: Some(nar_size),
            nar_hash: item.nar_hash.clone(),
            references: item.normalized_references(),
            deriver: deriver_bname,
            signatures: item.signatures.clone(),
            ca: item.ca.clone(),
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
            nar_size: nar_size.max(item.nar_size),
            added: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            origin_job: origin_job.map(|s| s.to_string()),
        };

        Ok(ExportedStorePath {
            store_hash,
            index_entry,
            file_size: nar_size,
        })
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
}

#[cfg(test)]
mod tests {
    use super::{
        ExportedStorePath, ParallelExporter, copy_nar_stream, parse_path_info_items_typed,
    };
    use async_compression::tokio::{bufread::ZstdDecoder, write::ZstdEncoder};
    use nixcache_core::{IndexEntry, NarDigest, StoreHash, SystemArch};
    use std::{
        io::{self},
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};

    #[test]
    fn test_parse_path_info_json_array_and_object() {
        let array_json = r#"[
            {
                "path": "/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-test-pkg",
                "narHash": "sha256:1111",
                "narSize": 1024,
                "references": ["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep"],
                "deriver": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-test.drv",
                "signatures": ["sig1", "sig2"]
            }
        ]"#;

        let items = parse_path_info_items_typed(array_json).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            item.path,
            "/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-test-pkg"
        );
        assert_eq!(item.nar_hash, "sha256:1111");
        assert_eq!(item.nar_size, 1024);
        assert_eq!(
            item.normalized_references(),
            vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep"]
        );
        assert_eq!(
            item.deriver.as_deref(),
            Some("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-test.drv")
        );
        assert_eq!(item.signatures, vec!["sig1", "sig2"]);

        let object_json = r#"{
            "/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-test-pkg": {
                "narHash": "sha256:2222",
                "narSize": 2048,
                "references": []
            }
        }"#;

        let items2 = parse_path_info_items_typed(object_json).unwrap();
        assert_eq!(items2.len(), 1);
        let item2 = &items2[0];
        assert_eq!(
            item2.path,
            "/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-test-pkg"
        );
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
            nar_basename: "s66mzxpvicwk07gjbjfw9izjfa797vsw.nar.zst".to_string(),
            compression: Some("zstd".to_string()),
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
    async fn test_copy_nar_stream_zstd() {
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
        let mut encoder =
            ZstdEncoder::with_quality(compressed_buf, async_compression::Level::Precise(3));

        let copied_bytes = copy_nar_stream(&mut pending_reader, &mut encoder)
            .await
            .expect("copy_nar_stream should succeed without error");
        assert_eq!(copied_bytes, original_data.len() as u64);

        encoder
            .shutdown()
            .await
            .expect("Encoder shutdown should succeed");

        let compressed = encoder.into_inner();
        assert!(!compressed.is_empty());

        let mut decoder = ZstdDecoder::new(&compressed[..]);
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
            strict: true,
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

    #[tokio::test]
    async fn test_parallel_exporter_prevents_ghcr_416_regression() {
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
                    if paths.len() >= 2 {
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

        wiremock::Mock::given(wiremock::matchers::method("HEAD"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

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

        // 模拟 GHCR 对所有 PATCH 请求返回 416
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .respond_with(wiremock::ResponseTemplate::new(416))
            .mount(&server)
            .await;

        let oci = nixcache_oci_backend::create_tokio_reqwest_client_with_driver(
            &host,
            "test/repo",
            "",
            true,
            nixcache_oci::GhcrDriver,
        );
        let config = super::ParallelExportConfig {
            concurrency: 2,
            signing_key_file: None,
            strict: true,
            upload_config: nixcache_oci::UploadConfig::default(),
            system: nixcache_core::SystemArch::from("x86_64-linux"),
            origin_job: Some("job:test".to_string()),
        };

        let report = ParallelExporter::export_and_upload_paths(&paths, &oci, &config)
            .await
            .expect("Parallel export must succeed without failing on GHCR 416");

        assert_eq!(report.successful.len(), paths.len());
        assert!(report.failed.is_empty());
        assert!(report.total_bytes_uploaded > 0);
    }
}
