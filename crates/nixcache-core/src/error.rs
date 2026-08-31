use std::num::ParseIntError;
use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    #[error("StoreHash length mismatch: expected 32 base32 chars, found {actual}")]
    StoreHashInvalidLength { actual: usize },

    #[error("Invalid Nix base32 character '{char}' at index {index}")]
    StoreHashInvalidChar { char: char, index: usize },

    #[error("NarDigest missing 'sha256:' prefix in '{raw}'")]
    NarDigestMissingPrefix { raw: String },

    #[error("NarDigest hex decode failed: expected 64 hex characters, found {actual}")]
    NarDigestInvalidHexLength { actual: usize },

    #[error("Invalid hex character '{char}' in digest at index {index}")]
    NarDigestInvalidHexChar { char: char, index: usize },

    #[error("Unsupported system architecture identifier: '{raw}'")]
    UnknownSystemArch { raw: String },
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum BloomError {
    #[error("Bloom filter byte length {actual} is not a multiple of 64 bytes (512 bits)")]
    InvalidByteLength { actual: usize },

    #[error("Bloom filter hash count must be > 0, got {0}")]
    ZeroHashCount(u8),
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum NarInfoParseError {
    #[error("Missing mandatory narinfo field: '{0}'")]
    MissingRequiredField(&'static str),

    #[error("Failed to parse integer field '{field}': {source}")]
    InvalidNumber {
        field: &'static str,
        #[source]
        source: ParseIntError,
    },

    #[error("Invalid store path format in StorePath field: '{0}'")]
    InvalidStorePath(String),

    #[error("NarInfo content is empty or contains no valid key-value pairs")]
    EmptyContent,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error(transparent)]
    Type(#[from] TypeError),

    #[error(transparent)]
    Bloom(#[from] BloomError),

    #[error(transparent)]
    NarInfoParse(#[from] NarInfoParseError),

    #[error("Serialization / Deserialization error: {0}")]
    Json(String),
}
