use nixcache_oci::OciError;
use std::{io, net::AddrParseError};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CliError {
    #[error("I/O error during CLI operations: {0}")]
    Io(#[from] io::Error),

    #[error("Socket address parsing failed: {0}")]
    AddrParse(#[from] AddrParseError),

    #[error(transparent)]
    Oci(#[from] OciError),
}
