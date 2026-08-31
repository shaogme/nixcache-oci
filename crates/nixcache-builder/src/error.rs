use nixcache_cli::CliError;
use nixcache_core::{CoreError, TypeError};
use nixcache_oci::OciError;
use std::{io, num::ParseIntError, process::ExitStatus};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum NixExecError {
    #[error("Nix CLI process execution failed: {0}")]
    Process(#[from] io::Error),

    #[error("Nix command '{command}' exited with failure status {status}:\n{stderr}")]
    ExitFailure {
        command: String,
        status: ExitStatus,
        stderr: String,
    },

    #[error("Nix CLI execution error: {0}")]
    Execution(String),
}

#[derive(Error, Debug)]
pub enum NarInfoParseError {
    #[error("Invalid / unexpected store path '{0}'")]
    InvalidStorePath(String),

    #[error("Invalid integer field in narinfo: {0}")]
    InvalidNumber(#[from] ParseIntError),
}

#[derive(Error, Debug)]
pub enum CompressionError {
    #[error("Zstd compression stream failure: {0}")]
    Zstd(#[from] io::Error),

    #[error(transparent)]
    Util(#[from] nixcache_utils::CompressionError),
}

#[derive(Error, Debug)]
pub enum SigningError {
    #[error("Signing key file I/O failure: {0}")]
    Io(#[from] io::Error),

    #[error("Invalid base64 encoding in signing key: {0}")]
    Base64(#[from] base64::DecodeError),
}

#[derive(Error, Debug)]
pub enum ProxyDaemonError {
    #[error("Proxy process failed to start or bind: {0}")]
    Startup(String),

    #[error("Proxy process exited prematurely: {0}")]
    Exited(String),

    #[error("Failed to wait for proxy health check: {0}")]
    HealthCheck(String),
}

#[derive(Error, Debug)]
pub enum BuilderError {
    #[error(transparent)]
    Nix(#[from] NixExecError),

    #[error(transparent)]
    NarInfo(#[from] NarInfoParseError),

    #[error(transparent)]
    Compression(#[from] CompressionError),

    #[error(transparent)]
    Signing(#[from] SigningError),

    #[error(transparent)]
    Proxy(#[from] ProxyDaemonError),

    #[error(transparent)]
    Core(#[from] CoreError),

    #[error(transparent)]
    Oci(#[from] OciError),

    #[error(transparent)]
    Cli(#[from] CliError),

    #[error("Parallel export and upload failed for {failed_count} path(s)")]
    ParallelExportFailed { failed_count: usize },

    #[error("I/O error during builder execution: {0}")]
    Io(#[from] io::Error),

    #[error("Task join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<TypeError> for BuilderError {
    fn from(err: TypeError) -> Self {
        Self::Core(CoreError::Type(err))
    }
}

impl From<nixcache_utils::CompressionError> for BuilderError {
    fn from(err: nixcache_utils::CompressionError) -> Self {
        Self::Compression(CompressionError::Util(err))
    }
}

impl From<base64::DecodeError> for BuilderError {
    fn from(err: base64::DecodeError) -> Self {
        Self::Signing(SigningError::Base64(err))
    }
}
