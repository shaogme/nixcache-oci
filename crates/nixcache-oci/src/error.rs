use crate::backend::RegistryKind;
use http::StatusCode;
use nixcache_core::CoreError;
use nixcache_utils::CompressionError;
use serde_json::Error as JsonError;
use std::{io::Error as IoError, str::Utf8Error, time::Duration};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TransportError {
    #[error("I/O transport failure: {0}")]
    Io(#[from] IoError),

    #[error("HTTP protocol / status error: {status}")]
    HttpStatus {
        status: StatusCode,
        message: Option<String>,
    },

    #[error("Network connection failed to endpoint '{endpoint}': {source}")]
    ConnectionFailed {
        endpoint: String,
        #[source]
        source: IoError,
    },

    #[error("Request timed out after {duration:?}")]
    Timeout { duration: Duration },

    #[error("Invalid request URI '{url}': {reason}")]
    InvalidUri { url: String, reason: &'static str },

    #[error("Header decode or parse error for '{header}'")]
    HeaderParse { header: &'static str },
}

#[derive(Error, Debug)]
pub enum TokenError {
    #[error("Registry bearer token missing in response body")]
    TokenMissingInBody,
}

#[derive(Error, Debug)]
pub enum OciError {
    #[error("Operation '{operation}' not supported on registry backend '{backend}': {reason}")]
    OperationNotSupported {
        operation: &'static str,
        backend: RegistryKind,
        reason: String,
    },

    #[error("Resource deletion failed for '{target}' with status {status}: {details}")]
    DeletionFailed {
        target: String,
        status: StatusCode,
        details: String,
    },

    #[error(
        "Insufficient permissions for '{target}' (required scope: '{required_scope}'): {details}"
    )]
    InsufficientPermission {
        target: String,
        required_scope: &'static str,
        details: String,
    },

    #[error("Target blob '{digest}' not found on registry")]
    BlobNotFound { digest: String },

    #[error("Blob check (HEAD) failed with HTTP {0}")]
    BlobCheckFailed(StatusCode),

    #[error("Blob upload failed with HTTP {0}")]
    BlobUploadFailed(StatusCode),

    #[error("Blob download failed with HTTP {0}")]
    BlobDownloadFailed(StatusCode),

    #[error("Manifest push failed with HTTP {0}")]
    ManifestPushFailed(StatusCode),

    #[error("Manifest fetch failed with HTTP {0}")]
    ManifestFetchFailed(StatusCode),

    #[error(
        "CAS optimistic concurrency precondition failed on tag '{tag}' (expected: {expected:?}, found: {actual:?})"
    )]
    CasPreconditionFailed {
        tag: String,
        expected: Option<String>,
        actual: Option<String>,
    },

    #[error("Upload session location missing in 202 Accepted response")]
    UploadLocationMissing,

    #[error("Resumable chunked upload failed after {attempts} attempts: {source}")]
    ResumableUploadFailed {
        attempts: usize,
        #[source]
        source: Box<OciError>,
    },

    #[error("Sub-manifest descriptor '{digest}' not found in multi-arch index")]
    SubManifestMissing { digest: String },

    #[error("Manifest missing target layer with root index / delta patch media type")]
    LayerDescriptorMissing,

    #[error("Unsupported layer media type: '{0}' (only Schema v5 Zstd media types supported)")]
    UnsupportedMediaType(String),

    #[error("Manifest JSON contains invalid UTF-8 bytes: {0}")]
    InvalidUtf8Manifest(#[from] Utf8Error),

    #[error(transparent)]
    Token(#[from] TokenError),

    #[error(transparent)]
    Transport(#[from] TransportError),

    #[error(transparent)]
    Compression(#[from] CompressionError),

    #[error(transparent)]
    Core(#[from] CoreError),

    #[error(transparent)]
    Json(#[from] JsonError),

    #[error(transparent)]
    Io(#[from] IoError),
}

impl From<nixcache_core::BloomError> for OciError {
    fn from(err: nixcache_core::BloomError) -> Self {
        Self::Core(CoreError::Bloom(err))
    }
}

impl From<nixcache_core::TypeError> for OciError {
    fn from(err: nixcache_core::TypeError) -> Self {
        Self::Core(CoreError::Type(err))
    }
}
