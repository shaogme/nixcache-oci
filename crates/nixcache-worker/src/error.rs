use nixcache_core::{BloomError, CoreError, TypeError};
use nixcache_oci::OciError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum WorkerStoreError {
    #[error("Cloudflare KV get operation failed on key '{key}': {message}")]
    KvGetFailed { key: String, message: String },

    #[error("Cloudflare KV put operation failed on key '{key}': {message}")]
    KvPutFailed { key: String, message: String },

    #[error("Base64 decoding failed for Bloom filter: {0}")]
    BloomBase64(String),

    #[error("Bloom filter decode failure: {0}")]
    Bloom(String),

    #[error("OCI registry operation error: {0}")]
    Oci(String),

    #[error("Core format / hash error: {0}")]
    Core(String),

    #[error("Aggregated refresh failed for components: {errors:?}")]
    AggregatedRefreshFailed { errors: Vec<String> },
}

impl From<base64::DecodeError> for WorkerStoreError {
    fn from(err: base64::DecodeError) -> Self {
        Self::BloomBase64(err.to_string())
    }
}

impl From<BloomError> for WorkerStoreError {
    fn from(err: BloomError) -> Self {
        Self::Bloom(err.to_string())
    }
}

impl From<TypeError> for WorkerStoreError {
    fn from(err: TypeError) -> Self {
        Self::Core(err.to_string())
    }
}

impl From<CoreError> for WorkerStoreError {
    fn from(err: CoreError) -> Self {
        Self::Core(err.to_string())
    }
}

impl From<OciError> for WorkerStoreError {
    fn from(err: OciError) -> Self {
        Self::Oci(err.to_string())
    }
}

impl From<worker::Error> for WorkerStoreError {
    fn from(err: worker::Error) -> Self {
        Self::KvGetFailed {
            key: "unknown".to_string(),
            message: err.to_string(),
        }
    }
}

impl From<WorkerStoreError> for worker::Error {
    fn from(err: WorkerStoreError) -> Self {
        worker::Error::RustError(err.to_string())
    }
}
