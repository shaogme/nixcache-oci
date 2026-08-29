use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("NarInfo parse error: {0}")]
    NarInfoParse(#[from] NarInfoParseError),

    #[error("GC error: {0}")]
    Gc(#[from] GcError),

    #[error("Invalid schema version: expected {expected}, found {found}")]
    InvalidSchemaVersion { expected: u32, found: u32 },

    #[error("General core error: {0}")]
    Other(String),
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum NarInfoParseError {
    #[error("Missing required field: {0}")]
    MissingRequiredField(&'static str),

    #[error("Invalid integer field '{field}': {value}")]
    InvalidNumber { field: &'static str, value: String },

    #[error("Empty or invalid narinfo content")]
    EmptyContent,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum GcError {
    #[error("Invalid timestamp: {0}")]
    InvalidTimestamp(String),
}
