use reqwest::StatusCode;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum OciError {
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Core error: {0}")]
    Core(#[from] nixcache_core::CoreError),

    #[error("Registry authentication failed")]
    AuthFailed,

    #[error("Blob upload failed with status: {0}")]
    UploadFailed(StatusCode),

    #[error("Manifest push failed with status: {0}")]
    ManifestPushFailed(StatusCode),

    #[error("CAS optimistic concurrency conflict on tag {0}")]
    CasConflict(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Other error: {0}")]
    Other(String),
}
