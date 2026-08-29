use http::StatusCode;
use nixcache_core::CoreError;
use serde_json::Error as JsonError;
use std::io::Error as IoError;
use thiserror::Error;

#[cfg(feature = "reqwest")]
use reqwest::Error as ReqwestError;

#[derive(Error, Debug)]
pub enum TransportError {
    #[cfg(feature = "reqwest")]
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] ReqwestError),

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
    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),

    #[cfg(feature = "reqwest")]
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] ReqwestError),

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

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Other error: {0}")]
    Other(String),
}
