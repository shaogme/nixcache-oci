use crate::error::CompressionError;
use bytes::Bytes;
use std::io::Read;

#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;

#[cfg(target_arch = "wasm32")]
use ruzstd::decoding::StreamingDecoder;

/// Zstd 压缩默认等级
pub const DEFAULT_ZSTD_COMPRESSION_LEVEL: i32 = 3;

/// Zstd 标准 Magic Number (Little-Endian: 0x28, 0xB5, 0x2F, 0xFD)
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// 跨平台统一 Zstd 编解码器
///
/// 封装原生系统（libzstd）与 WebAssembly（纯 Rust ruzstd）的实现差异。
pub struct ZstdCodec;

impl ZstdCodec {
    /// 校验二进制切片是否以合法的 Zstd Magic Number 开头
    pub fn is_valid_magic(bytes: &[u8]) -> bool {
        bytes.len() >= 4 && bytes[0..4] == ZSTD_MAGIC
    }

    /// 压缩数据为 Zstd 格式二进制 Blob
    ///
    /// 在 Native 平台使用 libzstd 流式压缩，在 wasm32 目标下返回 Unsupported 错误。
    pub fn compress(data: &[u8], level: i32) -> Result<Bytes, CompressionError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut encoder = zstd::stream::Encoder::new(Vec::with_capacity(data.len() / 4), level)
                .map_err(|e| CompressionError::ZstdError(e.to_string()))?;
            encoder.write_all(data).map_err(CompressionError::Io)?;
            let compressed = encoder.finish().map_err(CompressionError::Io)?;
            Ok(Bytes::from(compressed))
        }

        #[cfg(target_arch = "wasm32")]
        {
            let _ = (data, level);
            Err(CompressionError::Unsupported(
                "Zstd encoding/compression is not supported on wasm32 target".to_string(),
            ))
        }
    }

    /// 严格解压 Zstd 格式二进制 Blob
    ///
    /// 在 Native 平台使用 libzstd 解码器，在 wasm32 目标下使用纯 Rust ruzstd 流式解码器。
    pub fn decompress(raw_bytes: &[u8]) -> Result<Vec<u8>, CompressionError> {
        if !Self::is_valid_magic(raw_bytes) {
            return Err(CompressionError::InvalidMagic);
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut decoder = zstd::stream::Decoder::new(raw_bytes)
                .map_err(|e| CompressionError::ZstdError(e.to_string()))?;
            let mut uncompressed = Vec::new();
            decoder
                .read_to_end(&mut uncompressed)
                .map_err(CompressionError::Io)?;
            Ok(uncompressed)
        }

        #[cfg(target_arch = "wasm32")]
        {
            let mut decoder = StreamingDecoder::new(raw_bytes)
                .map_err(|e| CompressionError::ZstdError(e.to_string()))?;
            let mut uncompressed = Vec::new();
            decoder
                .read_to_end(&mut uncompressed)
                .map_err(CompressionError::Io)?;
            Ok(uncompressed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_ZSTD_COMPRESSION_LEVEL, ZSTD_MAGIC, ZstdCodec};
    use crate::error::CompressionError;

    #[test]
    fn test_magic_detection() {
        assert!(ZstdCodec::is_valid_magic(&ZSTD_MAGIC));
        assert!(ZstdCodec::is_valid_magic(&[
            0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x01
        ]));
        assert!(!ZstdCodec::is_valid_magic(&[0x00, 0x00, 0x00, 0x00]));
        assert!(!ZstdCodec::is_valid_magic(&[0x28, 0xB5, 0x2F]));
        assert!(!ZstdCodec::is_valid_magic(&[]));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn test_compress_and_decompress_roundtrip() {
        let sample_payload = b"Hello NixCache OCI! This is an uncompressed test payload for Zstd codec verification.";
        let compressed = ZstdCodec::compress(sample_payload, DEFAULT_ZSTD_COMPRESSION_LEVEL)
            .expect("Compression should succeed");
        assert!(ZstdCodec::is_valid_magic(&compressed));

        let decompressed =
            ZstdCodec::decompress(&compressed).expect("Decompression should succeed");
        assert_eq!(sample_payload.as_slice(), decompressed.as_slice());
    }

    #[test]
    fn test_decompress_rejects_invalid_magic() {
        let invalid = b"plain text without zstd header";
        let err = ZstdCodec::decompress(invalid).expect_err("Should reject invalid magic");
        match err {
            CompressionError::InvalidMagic => {}
            _ => panic!("Expected InvalidMagic, got: {:?}", err),
        }
    }

    #[test]
    fn test_decompress_rejects_empty_payload() {
        let empty = b"";
        let err = ZstdCodec::decompress(empty).expect_err("Should reject empty bytes");
        match err {
            CompressionError::InvalidMagic => {}
            _ => panic!("Expected InvalidMagic, got: {:?}", err),
        }
    }

    #[test]
    fn test_decompress_rejects_corrupted_data_with_magic() {
        let mut corrupted = ZSTD_MAGIC.to_vec();
        corrupted.extend_from_slice(b"corrupted-invalid-stream-payload");
        let err = ZstdCodec::decompress(&corrupted).expect_err("Should reject corrupted data");
        assert!(matches!(
            err,
            CompressionError::ZstdError(_) | CompressionError::Io(_)
        ));
    }
}
