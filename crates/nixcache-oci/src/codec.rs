use crate::{error::OciError, manifest::CacheLayerMediaType};
use bytes::Bytes;
pub use nixcache_utils::DEFAULT_ZSTD_COMPRESSION_LEVEL;
use nixcache_utils::ZstdCodec;
use serde::{Serialize, de::DeserializeOwned};

/// 强类型索引与清单编解码器
pub struct IndexCodec;

impl IndexCodec {
    /// 紧凑序列化并使用 Zstd 压缩为二进制 Blob
    pub fn encode_zstd<T: Serialize>(data: &T, level: i32) -> Result<Bytes, OciError> {
        // 1. 紧凑 JSON 序列化
        let json_bytes = serde_json::to_vec(data)?;

        // 2. 统一底层跨平台 Zstd 压缩
        ZstdCodec::compress(&json_bytes, level)
            .map_err(|e| OciError::CompressionError(e.to_string()))
    }

    /// 严格通过 Zstd 解压并反序列化
    pub fn decode_zstd<T: DeserializeOwned>(
        raw_bytes: &[u8],
        media_type_str: &str,
    ) -> Result<T, OciError> {
        // 1. 严格校验媒体类型
        let media_type = CacheLayerMediaType::parse(media_type_str)
            .ok_or_else(|| OciError::UnsupportedMediaType(media_type_str.to_string()))?;

        // 2. 严格校验 Zstd Magic Number (Little-Endian: 0x28, 0xB5, 0x2F, 0xFD)
        if !Self::is_valid_zstd_magic(raw_bytes) {
            return Err(OciError::CompressionError(format!(
                "Invalid Zstd header magic for layer type {}",
                media_type.as_str()
            )));
        }

        // 3. 统一底层跨平台解压并反序列化
        let uncompressed = ZstdCodec::decompress(raw_bytes)
            .map_err(|e| OciError::CompressionError(e.to_string()))?;
        let parsed: T = serde_json::from_slice(&uncompressed)?;
        Ok(parsed)
    }

    /// 探测并校验 Zstd Magic Number
    pub fn is_valid_zstd_magic(bytes: &[u8]) -> bool {
        ZstdCodec::is_valid_magic(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec};
    use crate::{error::OciError, manifest::CacheLayerMediaType};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct SampleData {
        name: String,
        items: Vec<u32>,
        nested: Option<String>,
    }

    #[test]
    fn test_cache_layer_media_type_parsing_and_helpers() {
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.nix.cache.index.v3+zstd"),
            Some(CacheLayerMediaType::IndexV3Zstd)
        );
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.nix.cache.session.v3+zstd"),
            Some(CacheLayerMediaType::SessionV3Zstd)
        );
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.nix.cache.index.v1+json"),
            None
        );
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.nix.cache.session.v1+json"),
            None
        );
        assert_eq!(CacheLayerMediaType::parse("application/json"), None);
        assert_eq!(CacheLayerMediaType::parse(""), None);

        let index_type = CacheLayerMediaType::IndexV3Zstd;
        assert_eq!(
            index_type.as_str(),
            "application/vnd.nix.cache.index.v3+zstd"
        );
        assert!(index_type.is_index());
        assert!(!index_type.is_session());

        let session_type = CacheLayerMediaType::SessionV3Zstd;
        assert_eq!(
            session_type.as_str(),
            "application/vnd.nix.cache.session.v3+zstd"
        );
        assert!(!session_type.is_index());
        assert!(session_type.is_session());
    }

    #[test]
    fn test_encode_and_decode_zstd_roundtrip() {
        let original = SampleData {
            name: "test-package-entry".to_string(),
            items: (0..100).collect(),
            nested: Some("metadata-payload".to_string()),
        };

        let encoded = IndexCodec::encode_zstd(&original, DEFAULT_ZSTD_COMPRESSION_LEVEL)
            .expect("Encoding should succeed");
        assert!(IndexCodec::is_valid_zstd_magic(&encoded));

        let decoded: SampleData =
            IndexCodec::decode_zstd(&encoded, CacheLayerMediaType::INDEX_V3_ZSTD)
                .expect("Decoding should succeed");

        assert_eq!(original, decoded);
    }

    #[test]
    fn test_decode_rejects_unsupported_media_type() {
        let original = SampleData {
            name: "test".to_string(),
            items: vec![1, 2, 3],
            nested: None,
        };
        let encoded = IndexCodec::encode_zstd(&original, 3).unwrap();

        let legacy_media_type = "application/vnd.nix.cache.index.v1+json";
        let err = IndexCodec::decode_zstd::<SampleData>(&encoded, legacy_media_type)
            .expect_err("Should reject legacy media type");

        match err {
            OciError::UnsupportedMediaType(mt) => {
                assert_eq!(mt, legacy_media_type);
            }
            _ => panic!("Expected UnsupportedMediaType, got: {:?}", err),
        }
    }

    #[test]
    fn test_decode_rejects_invalid_magic() {
        let plain_json = br#"{"name":"test","items":[1,2,3],"nested":null}"#;
        let err =
            IndexCodec::decode_zstd::<SampleData>(plain_json, CacheLayerMediaType::INDEX_V3_ZSTD)
                .expect_err("Should reject non-zstd plain JSON payload");

        match err {
            OciError::CompressionError(msg) => {
                assert!(msg.contains("Invalid Zstd header magic"));
            }
            _ => panic!(
                "Expected CompressionError for invalid magic, got: {:?}",
                err
            ),
        }
    }

    #[test]
    fn test_decode_rejects_short_or_empty_bytes() {
        let empty = b"";
        let err =
            IndexCodec::decode_zstd::<SampleData>(empty, CacheLayerMediaType::SESSION_V3_ZSTD)
                .expect_err("Should reject empty bytes");

        match err {
            OciError::CompressionError(msg) => {
                assert!(msg.contains("Invalid Zstd header magic"));
            }
            _ => panic!("Expected CompressionError for empty bytes, got: {:?}", err),
        }
    }

    #[test]
    fn test_decode_rejects_corrupted_payload_with_valid_magic() {
        let mut corrupt = vec![0x28, 0xB5, 0x2F, 0xFD];
        corrupt.extend_from_slice(b"completely corrupted trailing garbage bytes");

        let err =
            IndexCodec::decode_zstd::<SampleData>(&corrupt, CacheLayerMediaType::INDEX_V3_ZSTD)
                .expect_err("Should reject corrupted payload");

        match err {
            OciError::CompressionError(_) => {}
            _ => panic!(
                "Expected CompressionError for corrupted payload, got: {:?}",
                err
            ),
        }
    }
}
