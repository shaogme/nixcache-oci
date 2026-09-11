use crate::{error::OciError, manifest::CacheLayerMediaType};
use bytes::Bytes;
pub use nixcache_utils::DEFAULT_ZSTD_COMPRESSION_LEVEL;
use nixcache_utils::ZstdCodec;
use serde::{Serialize, de::DeserializeOwned};

/// 强类型索引与清单编解码器 (Schema v6)
pub struct IndexCodec;

#[derive(Debug, PartialEq, Eq)]
pub struct DecodedIndex<T> {
    pub value: T,
    pub uncompressed_size: u64,
}

impl IndexCodec {
    /// 紧凑序列化并使用 Zstd 压缩为二进制 Blob
    pub fn encode_zstd<T: Serialize>(data: &T, level: i32) -> Result<Bytes, OciError> {
        // 1. 紧凑 JSON 序列化
        let json_bytes = serde_json::to_vec(data)?;

        // 2. 统一底层跨平台 Zstd 压缩
        let compressed = ZstdCodec::compress(&json_bytes, level)?;
        Ok(compressed)
    }

    /// 严格通过 Zstd 解压并反序列化
    pub fn decode_zstd<T: DeserializeOwned>(
        raw_bytes: &[u8],
        media_type_str: &str,
        max_uncompressed_bytes: u64,
    ) -> Result<DecodedIndex<T>, OciError> {
        // 1. 严格校验媒体类型
        let _media_type = CacheLayerMediaType::parse(media_type_str)
            .ok_or_else(|| OciError::UnsupportedMediaType(media_type_str.to_string()))?;

        // 2. 统一底层跨平台解压并反序列化
        let uncompressed = ZstdCodec::decompress_limited(raw_bytes, max_uncompressed_bytes)?;
        let uncompressed_size = uncompressed.len() as u64;
        let parsed: T = serde_json::from_slice(&uncompressed)?;
        Ok(DecodedIndex {
            value: parsed,
            uncompressed_size,
        })
    }

    /// 探测并校验 Zstd Magic Number
    pub fn is_valid_zstd_magic(bytes: &[u8]) -> bool {
        ZstdCodec::is_valid_magic(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::IndexCodec;
    use crate::{error::OciError, manifest::CacheLayerMediaType};
    use nixcache_utils::CompressionError;
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
            CacheLayerMediaType::parse("application/vnd.nix.cache.root.v6+zstd"),
            Some(CacheLayerMediaType::RootIndexV6Zstd)
        );
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.nix.cache.shard.v6+zstd"),
            Some(CacheLayerMediaType::ShardDataV6Zstd)
        );
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.nix.cache.delta.v6+zstd"),
            None
        );
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.oci.image.layer.v1.tar+gzip"),
            None
        );
    }

    #[test]
    fn test_encode_and_decode_zstd_roundtrip() {
        let original = SampleData {
            name: "test-node".to_string(),
            items: vec![10, 20, 30, 40, 50],
            nested: Some("inner value".to_string()),
        };

        let encoded = IndexCodec::encode_zstd(&original, 3).expect("Encoding should succeed");
        assert!(IndexCodec::is_valid_zstd_magic(&encoded));

        let decoded =
            IndexCodec::decode_zstd(&encoded, CacheLayerMediaType::ROOT_INDEX_V6_ZSTD, 1024)
                .expect("Decoding should succeed");
        assert_eq!(original, decoded.value);
    }

    #[test]
    fn test_decode_rejects_unsupported_media_type() {
        let original = SampleData {
            name: "test".to_string(),
            items: vec![1],
            nested: None,
        };
        let encoded = IndexCodec::encode_zstd(&original, 3).unwrap();

        let legacy_media_type = "application/vnd.nix.cache.index.v3+zstd";
        let err = IndexCodec::decode_zstd::<SampleData>(&encoded, legacy_media_type, 1024)
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
        let err = IndexCodec::decode_zstd::<SampleData>(
            plain_json,
            CacheLayerMediaType::ROOT_INDEX_V6_ZSTD,
            1024,
        )
        .expect_err("Should reject non-zstd plain JSON payload");

        match err {
            OciError::Compression(CompressionError::InvalidMagic { .. }) => {}
            _ => panic!("Expected CompressionError::InvalidMagic, got: {:?}", err),
        }
    }

    #[test]
    fn test_decode_rejects_short_or_empty_bytes() {
        let empty = b"";
        let err = IndexCodec::decode_zstd::<SampleData>(
            empty,
            CacheLayerMediaType::SHARD_DATA_V6_ZSTD,
            1024,
        )
        .expect_err("Should reject empty bytes");

        match err {
            OciError::Compression(CompressionError::EmptyBuffer) => {}
            _ => panic!("Expected CompressionError::EmptyBuffer, got: {:?}", err),
        }
    }

    #[test]
    fn test_decode_rejects_corrupted_payload_with_valid_magic() {
        let mut corrupt = vec![0x28, 0xB5, 0x2F, 0xFD];
        corrupt.extend_from_slice(b"completely corrupted trailing garbage bytes");

        let err = IndexCodec::decode_zstd::<SampleData>(
            &corrupt,
            CacheLayerMediaType::ROOT_INDEX_V6_ZSTD,
            1024,
        )
        .expect_err("Should reject corrupted payload");

        match err {
            OciError::Compression(
                CompressionError::ZstdDecompress { .. } | CompressionError::Io(_),
            ) => {}
            _ => panic!(
                "Expected CompressionError for corrupted payload, got: {:?}",
                err
            ),
        }
    }
}
