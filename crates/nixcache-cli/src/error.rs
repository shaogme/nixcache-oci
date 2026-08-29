use std::{io, net::AddrParseError};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CliError {
    #[error("CLI argument parse or validation error: {0}")]
    Validation(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("Network address parse error: {0}")]
    AddrParse(#[from] AddrParseError),
}
