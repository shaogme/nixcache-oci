use bytes::Bytes;
use std::path::PathBuf;

/// 确定性 Blob 上传载荷
#[derive(Debug, Clone)]
pub enum BlobPayload<S> {
    /// 内存全量字节 (已知 Digest 与 Size)
    Bytes { digest: String, data: Bytes },
    /// 本地文件 (已知 Digest 与 Size，支持零拷贝流式读取)
    File {
        digest: String,
        path: PathBuf,
        size: u64,
    },
    /// 未知 Digest/Size 的流式数据 (在进入 Provider 管道前由 Spooler 确定)
    DynamicStream { stream: S },
}

/// 上传流式与分块配置
#[derive(Debug, Clone)]
pub struct UploadConfig {
    /// 分块阈值，超过此大小且后端支持分块时触发分块上传（默认 64MB）
    pub chunk_threshold_bytes: u64,
    /// 单个分块大小（默认 32MB，最小 1MB）
    pub chunk_size_bytes: usize,
    /// 最大网络中断重试次数（默认 5 次）
    pub max_retry_attempts: usize,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            chunk_threshold_bytes: 64 * 1024 * 1024,
            chunk_size_bytes: 32 * 1024 * 1024,
            max_retry_attempts: 5,
        }
    }
}
