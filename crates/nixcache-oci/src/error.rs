use crate::backend::RegistryKind;
use http::StatusCode;
use nixcache_core::CoreError;
use serde_json::Error as JsonError;
use std::io::Error as IoError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TransportError {
    #[error("IO error: {0}")]
    Io(#[from] IoError),

    #[error("Network error: {0}")]
    Network(String),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Other transport error: {0}")]
    Other(String),
}

#[derive(Error, Debug)]
pub enum OciError {
    #[error("Registry operation not supported on backend '{backend}': {reason}")]
    OperationNotSupported {
        backend: RegistryKind,
        reason: String,
    },

    #[error("Deletion failed for target '{target}' with status {status}: {details}")]
    DeletionFailed {
        target: String,
        status: StatusCode,
        details: String,
    },

    #[error(
        "Insufficient permissions to delete '{target}'. Required scope: '{required_scope}'. Server response: {details}"
    )]
    InsufficientPermission {
        target: String,
        required_scope: &'static str,
        details: String,
    },

    #[error("Target resource '{target}' not found on remote registry")]
    ResourceNotFound { target: String },

    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),

    #[error("IO error: {0}")]
    Io(#[from] IoError),

    #[error("JSON error: {0}")]
    Json(#[from] JsonError),

    #[error("Core error: {0}")]
    Core(#[from] CoreError),

    #[error("Registry authentication failed")]
    AuthFailed,

    #[error("Blob not found: {0}")]
    BlobNotFound(String),

    #[error("Blob check failed with status: {0}")]
    BlobCheckFailed(StatusCode),

    #[error("Blob upload failed with status: {0}")]
    BlobUploadFailed(StatusCode),

    #[error("Blob download failed with status: {0}")]
    BlobDownloadFailed(StatusCode),

    #[error("Manifest push failed with status: {0}")]
    ManifestPushFailed(StatusCode),

    #[error("Manifest fetch failed with status: {0}")]
    ManifestFetchFailed(StatusCode),

    #[error("CAS optimistic concurrency conflict on tag {0}")]
    CasConflict(String),

    #[error("Monolithic upload rejected by registry (status: {0}), falling back to chunked")]
    MonolithicUploadRejected(StatusCode),

    #[error("Upload session expired or invalid: {0}")]
    UploadSessionExpired(String),

    #[error("Upload range mismatch: expected {expected}, registry reported {actual}")]
    UploadRangeMismatch { expected: u64, actual: u64 },

    #[error("Chunk upload failed after {attempts} attempts: {last_error}")]
    ResumableUploadFailed { attempts: usize, last_error: String },

    #[error("Digest mismatch after stream upload: expected {expected}, computed {computed}")]
    StreamDigestMismatch { expected: String, computed: String },

    #[error(
        "Unsupported or legacy layer media type: '{0}' (only Schema v5 media types are supported)"
    )]
    UnsupportedMediaType(String),

    #[error("Zstd compression/decompression error: {0}")]
    CompressionError(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Other error: {0}")]
    Other(String),
}
