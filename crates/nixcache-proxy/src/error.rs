use nixcache_cli::CliError;
use nixcache_core::CoreError;
use nixcache_oci::OciError;
use std::{io, net::AddrParseError};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProxyIndexError {
    #[error(transparent)]
    Oci(#[from] OciError),

    #[error(transparent)]
    Core(#[from] CoreError),

    #[error("Force refresh encountered aggregated failures: {failures:?}")]
    AggregatedRefreshFailure { failures: Vec<String> },
}

#[derive(Error, Debug)]
pub enum ProxyError {
    #[error(transparent)]
    Index(#[from] ProxyIndexError),

    #[error(transparent)]
    Cli(#[from] CliError),

    #[error(transparent)]
    Oci(#[from] OciError),

    #[error("TCP socket bind error: {0}")]
    Bind(#[from] io::Error),

    #[error("Socket address resolution error: {0}")]
    AddrParse(#[from] AddrParseError),

    #[error("Axum HTTP server execution error: {0}")]
    Server(#[source] io::Error),
}
