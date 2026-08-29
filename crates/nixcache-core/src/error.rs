use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    #[error("Invalid StoreHash (expected 32 base32 chars): {0}")]
    InvalidStoreHash(String),

    #[error("Invalid NarDigest (expected sha256:...): {0}")]
    InvalidNarDigest(String),

    #[error("Invalid SystemArch: {0}")]
    InvalidSystemArch(String),
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("Type error: {0}")]
    Type(#[from] TypeError),

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

    #[error("Invalid store hash in references or path: {0}")]
    InvalidStoreHash(#[from] TypeError),

    #[error("Empty or invalid narinfo content")]
    EmptyContent,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum GcError {
    #[error("Invalid timestamp: {0}")]
    InvalidTimestamp(String),
}
