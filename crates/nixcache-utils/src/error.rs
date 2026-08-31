use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CompressionError {
    #[error("Zstd compression failed (code: {code})")]
    ZstdCompress { code: usize },

    #[error("Zstd decompression failed (code: {code})")]
    ZstdDecompress { code: usize },

    #[error("Invalid Zstd magic number header (found 0x{found:08X}, expected 0x28B52FFD)")]
    InvalidMagic { found: u32 },

    #[error("Empty buffer supplied for decompression")]
    EmptyBuffer,

    #[error("I/O error during compression streaming: {0}")]
    Io(#[from] io::Error),
}
