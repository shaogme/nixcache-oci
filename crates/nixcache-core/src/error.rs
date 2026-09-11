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

    #[error("Origin metadata must contain a run ID or a non-empty job ID")]
    OriginMissingFields,

    #[error("Origin job ID must not be empty")]
    EmptyOriginJobId,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum BloomError {
    #[error("Bloom filter byte length {actual} is not a multiple of 64 bytes (512 bits)")]
    InvalidByteLength { actual: usize },

    #[error("Bloom filter hash count must be in {min}..={max}, got {actual}")]
    InvalidHashCount { actual: u8, min: u8, max: u8 },

    #[error("Bloom filter false-positive rate must be finite and in [{min}, {max}], got {actual}")]
    InvalidFalsePositiveRate {
        actual: String,
        min: &'static str,
        max: &'static str,
    },

    #[error("Bloom filter block count must be greater than zero, got {actual}")]
    InvalidBlockCount { actual: u32 },

    #[error("Bloom filter block count {actual} exceeds u32::MAX ({max})")]
    BlockCountOverflow { actual: u64, max: u32 },

    #[error("Bloom filter block count {actual} exceeds the limit {limit}")]
    BlockLimitExceeded { actual: u64, limit: u32 },

    #[error("Bloom filter arithmetic overflow while calculating {operation}")]
    ArithmeticOverflow { operation: &'static str },

    #[error("Bloom filter allocation of {requested} words failed: {details}")]
    AllocationFailed { requested: usize, details: String },

    #[error("Bloom filter entry count overflow at {actual}")]
    EntryCountOverflow { actual: u64 },

    #[error(
        "Bloom filter internal structure is invalid: {num_blocks} blocks require {expected_words} words, found {actual_words}"
    )]
    InvalidStructure {
        num_blocks: u32,
        expected_words: usize,
        actual_words: usize,
    },
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

    #[error("Invalid Schema v8 root index: {details}")]
    InvalidIndex { details: String },

    #[error("Invalid Schema v8 shard payload: {details}")]
    InvalidShard { details: String },

    #[error("Invalid Schema v8 index entry or metadata: {details}")]
    InvalidEntry { details: String },

    #[error("Invalid Schema v8 Merkle structure: {details}")]
    InvalidMerkle { details: String },

    #[error("Invalid Schema v8 shard descriptor set: {details}")]
    InvalidShardSet { details: String },

    #[error("Invalid canonical digest in {field}: {details}")]
    InvalidCanonicalDigest {
        field: &'static str,
        details: String,
    },

    #[error("Schema v8 {target} exceeds limit {limit} (actual {actual})")]
    LimitExceeded {
        target: &'static str,
        limit: u64,
        actual: u64,
    },

    #[error("Build receipt origin mismatch for entry {hash}: expected {expected}, got {actual}")]
    ReceiptOriginMismatch {
        hash: String,
        expected: String,
        actual: String,
    },

    #[error("Invalid BuildReceipt: {details}")]
    InvalidReceipt { details: String },
}
