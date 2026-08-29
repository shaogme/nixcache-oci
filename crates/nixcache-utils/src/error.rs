use thiserror::Error;

/// 跨平台统一压缩错误枚举
#[derive(Error, Debug)]
pub enum CompressionError {
    #[error("Zstd internal error: {0}")]
    ZstdError(String),

    #[error("Invalid Zstd magic number header")]
    InvalidMagic,

    #[error("Zstd operation is not supported on target platform: {0}")]
    Unsupported(String),

    #[error("I/O error during compression/decompression: {0}")]
    Io(#[from] std::io::Error),
}

/// 平台差异工具库通用错误枚举
#[derive(Error, Debug)]
pub enum UtilError {
    #[error("Compression error: {0}")]
    Compression(#[from] CompressionError),

    #[error("System runtime error: {0}")]
    Sys(String),
}
