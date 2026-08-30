use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    #[error("Invalid StoreHash (expected 32 base32 chars): {0}")]
    InvalidStoreHash(String),

    #[error("Invalid NarDigest (expected sha256:...): {0}")]
    InvalidNarDigest(String),

    #[error("Invalid SystemArch: {0}")]
    InvalidSystemArch(String),

    #[error("Invalid Nix Base32 character: '{0}'")]
    InvalidBase32Char(char),
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum BloomError {
    #[error("Invalid Bloom filter byte length: expected multiple of 64 bytes (512 bits), got {0}")]
    InvalidByteLength(usize),

    #[error("Invalid Bloom filter bit count: expected {expected}, got {actual}")]
    InvalidBitsLength { expected: u64, actual: u64 },
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ShardingError {
    #[error("Invalid shard ID: {0} (must be in 0..1023)")]
    InvalidShardId(u16),

    #[error("Invalid shard prefix: {0} (must be 2 nix base32 characters)")]
    InvalidPrefix(String),

    #[error("StoreHash too short for sharding: length {0} < 2")]
    HashTooShort(usize),
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

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("Type error: {0}")]
    Type(#[from] TypeError),

    #[error("Bloom filter error: {0}")]
    Bloom(#[from] BloomError),

    #[error("Sharding error: {0}")]
    Sharding(#[from] ShardingError),

    #[error("NarInfo parse error: {0}")]
    NarInfoParse(#[from] NarInfoParseError),

    #[error("GC error: {0}")]
    Gc(#[from] GcError),

    #[error("Invalid schema version: expected {expected}, found {found}")]
    InvalidSchemaVersion { expected: u32, found: u32 },

    #[error("General core error: {0}")]
    Other(String),
}
