use crate::{error::OciError, manifest::CacheLayerMediaType};
use bytes::Bytes;
use nixcache_core::FastBlockedBloomFilter;
pub use nixcache_utils::DEFAULT_ZSTD_COMPRESSION_LEVEL;
use nixcache_utils::ZstdCodec;
use serde::{Serialize, de::DeserializeOwned};

/// 强类型索引与清单编解码器 (Schema v5)
pub struct IndexCodec;

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
    ) -> Result<T, OciError> {
        // 1. 严格校验媒体类型
        let _media_type = CacheLayerMediaType::parse(media_type_str)
            .ok_or_else(|| OciError::UnsupportedMediaType(media_type_str.to_string()))?;

        // 2. 统一底层跨平台解压并反序列化
        let uncompressed = ZstdCodec::decompress(raw_bytes)?;
        let parsed: T = serde_json::from_slice(&uncompressed)?;
        Ok(parsed)
    }

    /// 将布隆过滤器紧凑二进制序列化并使用 Zstd 压缩
    pub fn encode_bloom_filter(
        filter: &FastBlockedBloomFilter,
        level: i32,
    ) -> Result<Bytes, OciError> {
        let raw_bytes = filter.to_bytes();
        let compressed = ZstdCodec::compress(&raw_bytes, level)?;
        Ok(compressed)
    }

    /// 严格通过 Zstd 解压并反序列化布隆过滤器
    pub fn decode_bloom_filter(
        raw_bytes: &[u8],
        num_entries: u64,
        num_hashes: u8,
    ) -> Result<FastBlockedBloomFilter, OciError> {
        let uncompressed = ZstdCodec::decompress(raw_bytes)?;
        let filter = FastBlockedBloomFilter::from_bytes(&uncompressed, num_entries, num_hashes)?;
        Ok(filter)
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
    use nixcache_core::{FastBlockedBloomFilter, StoreHash};
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
            CacheLayerMediaType::parse("application/vnd.nix.cache.root.v5+zstd"),
            Some(CacheLayerMediaType::RootIndexV5Zstd)
        );
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.nix.cache.shard.v5+zstd"),
            Some(CacheLayerMediaType::ShardDataV5Zstd)
        );
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.nix.cache.delta.v5+zstd"),
            Some(CacheLayerMediaType::DeltaPatchV5Zstd)
        );
        assert_eq!(
            CacheLayerMediaType::parse("application/vnd.nix.cache.bloom.v5+zstd"),
            Some(CacheLayerMediaType::BloomFilterV5Zstd)
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

        let decoded: SampleData =
            IndexCodec::decode_zstd(&encoded, CacheLayerMediaType::ROOT_INDEX_V5_ZSTD)
                .expect("Decoding should succeed");
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_encode_and_decode_bloom_filter_roundtrip() {
        let mut filter = FastBlockedBloomFilter::new_with_defaults(100);
        let hash1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let hash2 = StoreHash::parse("00000000000000000000000000000000").unwrap();
        filter.insert(&hash1);
        filter.insert(&hash2);

        let encoded = IndexCodec::encode_bloom_filter(&filter, 3)
            .expect("Bloom filter encode should succeed");
        assert!(IndexCodec::is_valid_zstd_magic(&encoded));

        let decoded = IndexCodec::decode_bloom_filter(&encoded, 100u64, filter.num_hashes())
            .expect("Bloom filter decode should succeed");
        assert!(decoded.contains(&hash1));
        assert!(decoded.contains(&hash2));
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
        let err = IndexCodec::decode_zstd::<SampleData>(
            plain_json,
            CacheLayerMediaType::ROOT_INDEX_V5_ZSTD,
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
        let err =
            IndexCodec::decode_zstd::<SampleData>(empty, CacheLayerMediaType::DELTA_PATCH_V5_ZSTD)
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
            CacheLayerMediaType::ROOT_INDEX_V5_ZSTD,
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
