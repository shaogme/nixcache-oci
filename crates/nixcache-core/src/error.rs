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

    #[error("Unsupported NarDigest algorithm '{algorithm}'; only sha256 is accepted")]
    NarDigestInvalidAlgorithm { algorithm: String },

    #[error("NarDigest hex decode failed: expected 64 hex characters, found {actual}")]
    NarDigestInvalidHexLength { actual: usize },

    #[error("Invalid hex character '{char}' in digest at index {index}")]
    NarDigestInvalidHexChar { char: char, index: usize },

    #[error("Unsupported Nix hash algorithm '{algorithm}'; only sha256 is accepted")]
    NixHashInvalidAlgorithm { algorithm: String },

    #[error("Nix hash length mismatch: expected {expected}, found {actual}")]
    NixHashInvalidLength {
        expected: &'static str,
        actual: usize,
    },

    #[error("Invalid Nix hash character '{char}' at index {index}")]
    NixHashInvalidChar { char: char, index: usize },

    #[error("Invalid StorePath format: '{raw}'")]
    InvalidStorePathFormat { raw: String },

    #[error("Invalid NAR basename: '{raw}'")]
    InvalidNarBasename { raw: String },

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

    #[error("Invalid URL or NAR basename: '{0}'")]
    InvalidUrl(String),

    #[error("Invalid reference: '{0}'")]
    InvalidReference(String),

    #[error("Invalid field '{field}': {details}")]
    InvalidField {
        field: &'static str,
        details: String,
    },

    #[error("Field '{0}' must not be empty")]
    EmptyField(&'static str),

    #[error("Field '{0}' may only occur once")]
    DuplicateField(&'static str),

    #[error("Invalid hash in field '{field}': {source}")]
    InvalidHash {
        field: &'static str,
        #[source]
        source: TypeError,
    },

    #[error("Field '{0}' must be greater than zero")]
    NonPositiveSize(&'static str),

    #[error("Malformed narinfo line: '{0}'")]
    MalformedLine(String),

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

    #[error("Invalid Schema v7 root index: {details}")]
    InvalidIndex { details: String },

    #[error("Invalid Schema v7 shard payload: {details}")]
    InvalidShard { details: String },

    #[error("Invalid Schema v7 index entry or metadata: {details}")]
    InvalidEntry { details: String },

    #[error("Invalid Schema v7 Merkle structure: {details}")]
    InvalidMerkle { details: String },

    #[error("Invalid Schema v7 shard descriptor set: {details}")]
    InvalidShardSet { details: String },

    #[error("Invalid canonical digest in {field}: {details}")]
    InvalidCanonicalDigest {
        field: &'static str,
        details: String,
    },

    #[error("Schema v7 {target} exceeds limit {limit} (actual {actual})")]
    LimitExceeded {
        target: &'static str,
        limit: u64,
        actual: u64,
    },
}
