pub mod auth;
pub mod backend;
pub mod client;
pub mod codec;
pub mod error;
pub mod integrity;
pub mod limits;
pub mod manifest;
pub mod mock;
pub mod token;
pub mod transport;
pub mod upload;

pub use auth::{
    BearerChallenge, RegistryCredentials, parse_www_authenticate, parse_www_authenticate_value,
};
pub use backend::{
    AwsEcrDriver, AzureAcrDriver, BlobUploadStrategy, DockerHubDriver, GcpArtifactRegistryDriver,
    GenericOciDriver, GhcrDriver, GitHubContainerMetadata, GitHubPackageVersion,
    GitHubPackageVersionMetadata, GitHubPackagesClient, ManifestCasSupport, OciBackendDriver,
    OciDriver, PackageDeletionSupport, RegistryCapabilities, RegistryDeletionStrategy,
    RegistryEndpoint, RegistryEndpointError, RegistryKind, RegistryScheme, detect_driver,
    driver_for_kind,
};
pub use client::{
    BlobClient, BlobDeletionOutcome, BlobDeletionTarget, DeletionBatchResult, DeletionClient,
    DeletionFailure, DeletionFailureKind, DeletionObjectType, DeletionSummary, FetchedOciArtifact,
    IndexClient, ManifestCasCondition, ManifestClient, ManifestDeletionOutcome, OciClient,
    PackageDeletionScope, PackageDeletionSummary, TagList,
};
pub use codec::{DEFAULT_ZSTD_COMPRESSION_LEVEL, DecodedIndex, IndexCodec};
pub use error::{OciError, TokenError, TransportError};
pub use integrity::{ContentDigest, verify_buffered_body, verify_size};
pub use limits::{OciReadLimits, ReadLimitsError};
pub use manifest::{
    CacheLayerMediaType, CacheLayerMediaTypeV7, CacheLayerMediaTypeV8, EMPTY_CONFIG_DIGEST,
    EMPTY_CONFIG_SIZE, OCI_IMAGE_CONFIG_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE,
    OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest, OciDescriptor, OciImageIndex,
    OciImageManifest, OciPlatform, ShardedArchIndexManifestParams, build_image_index,
    build_sharded_arch_index_manifest,
};
#[cfg(not(target_arch = "wasm32"))]
pub use mock::MockTokenGate;
pub use mock::{
    MockPatchOutcome, MockPatchRequest, MockPutRequest, MockResponse, MockRouterTransport,
};
pub use nixcache_core::{
    BuildReceipt, BuildStats, CACHE_INDEX_VERSION, IndexEntry, JobSummaryMetadata, NUM_SHARDS,
    NarDigest, NarInfo, NarInfoMeta, OriginMetadata, RECEIPT_VERSION, RUN_SESSION_VERSION,
    SCHEMA_VERSION_V8, ShardDataPayload, ShardDescriptor, ShardedArchCacheIndexData, StoreHash,
    SystemArch, build_nar_lookup_map, calculate_shard_id, compute_merkle_root,
    compute_shard_merkle_hash, diff_shard_descriptors, evaluate_multi_arch_gc,
    extract_nar_basename, extract_store_hash, extract_store_hash_str, partition_entries_by_shard,
    shard_id_to_prefix,
};
pub use token::TokenManager;
/// 流式哈希状态只会在底层流观察到 EOF 后提供完整 body digest；错误、提前
/// drop 或尚未结束的流都不会暴露可用于 OCI 身份的部分 digest。
pub use transport::{
    HashingStream, OciBlobStream, OciTransport, StreamHashState, StreamHashStatus,
    UploadChunkResponse, VerifiedBlobStream, check_content_length, collect_limited,
    parse_content_length, parse_range_header,
};
pub use upload::{BlobPayload, UploadConfig};
