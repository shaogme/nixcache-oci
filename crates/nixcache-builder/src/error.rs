use nixcache_core::CoreError;
use nixcache_oci::OciError;
use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BuilderError {
    #[error("Nix CLI error: {0}")]
    NixCli(String),

    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("OCI client error: {0}")]
    Oci(#[from] OciError),

    #[error("Core error: {0}")]
    Core(#[from] CoreError),

    #[error("Proxy daemon error: {0}")]
    Proxy(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("General builder error: {0}")]
    Other(String),
}
