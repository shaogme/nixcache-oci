use nixcache_core::{
    BloomFilterManifest, DeltaPatchData, FastBlockedBloomFilter, IndexEntry, NUM_SHARDS, NarDigest,
    NarInfoMeta, ShardDataPayload, ShardedArchCacheIndexData, StoreHash, SystemArch,
};
use nixcache_oci::{
    CacheLayerMediaType, EMPTY_CONFIG_DIGEST, EMPTY_CONFIG_SIZE, IndexCodec,
    OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciDescriptor, OciError, OciImageManifest, OciPlatform,
    SessionMutationRequest, ShardedArchIndexManifestParams, build_delta_patch_manifest,
    build_image_index, build_sharded_arch_index_manifest,
};
use nixcache_oci_backend::create_tokio_reqwest_client;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn compute_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    format!(
        "sha256:{}",
        hash.iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    )
}

fn sample_sharded_arch_index_data(
    system: SystemArch,
) -> (
    ShardedArchCacheIndexData,
    FastBlockedBloomFilter,
    ShardDataPayload,
) {
    let mut shard_payload = ShardDataPayload::new(42);
    let hash_str = if system == SystemArch::X86_64Linux {
        "s66mzxpvicwk07gjbjfw9izjfa797vsw"
    } else {
        "s66mzxpvicwk07gjbjfw9izjfa797vsa"
    };
    let hash = StoreHash::parse(hash_str).unwrap();
    shard_payload.entries.insert(
        hash.clone(),
        IndexEntry {
            name: format!("pkg-{}", system.as_str()),
            system: Some(system),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash),
                nar_basename: format!("pkg-{}.nar.xz", system.as_str()),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 2048,
            added: "2026-08-29T12:00:00Z".to_string(),
            origin_job: Some("job:ci-build".to_string()),
        },
    );

    let mut bloom = FastBlockedBloomFilter::new_with_defaults(100);
    bloom.insert(&hash);

    let mut root_index = ShardedArchCacheIndexData::new(system, "test/repo", "ghcr.io");
    root_index.public_key = "cache.example.com-1:key123".to_string();
    root_index.last_promoted_run = Some(42);
    root_index.gc_roots = vec![hash];

    (root_index, bloom, shard_payload)
}

fn sample_delta_patch_data(run_id: u64, system: SystemArch) -> DeltaPatchData {
    let mut delta = DeltaPatchData::new(run_id, "job:workflow-step", system);
    let hash = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
    delta.new_entries.insert(
        hash.clone(),
        IndexEntry {
            name: format!("session-pkg-{}", system.as_str()),
            system: Some(system),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-session-pkg", hash),
                nar_basename: format!("session-pkg-{}.nar.xz", system.as_str()),
                nar_hash: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap(),
            nar_size: 4096,
            added: "2026-08-29T12:00:00Z".to_string(),
            origin_job: Some("job:workflow-step".to_string()),
        },
    );
    delta.active_gc_roots.push(hash);
    delta
}

#[test]
fn test_manifest_builder_generates_v5_zstd_descriptors() {
    let system = SystemArch::X86_64Linux;
    let index_manifest = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
        root_blob_digest: "sha256:rootblob123",
        root_blob_size: 500,
        bloom_blob_digest: "sha256:bloomblob456",
        bloom_blob_size: 120,
        config_digest: "sha256:config123",
        config_size: 2,
        system: &system,
        merkle_root: "sha256:merkle123",
    });

    assert_eq!(index_manifest.schema_version, 2);
    assert_eq!(index_manifest.media_type, OCI_IMAGE_MANIFEST_MEDIA_TYPE);
    assert_eq!(index_manifest.layers.len(), 2);

    let root_layer = &index_manifest.layers[0];
    assert_eq!(
        root_layer.media_type,
        CacheLayerMediaType::ROOT_INDEX_V5_ZSTD
    );
    assert_eq!(root_layer.digest, "sha256:rootblob123");
    assert_eq!(root_layer.size, 500);

    let annotations = root_layer.annotations.as_ref().unwrap();
    assert_eq!(
        annotations
            .get("org.nixos.nixcache.schema")
            .map(|s| s.as_str()),
        Some("5")
    );
    assert_eq!(
        annotations
            .get("org.nixos.nixcache.system")
            .map(|s| s.as_str()),
        Some("x86_64-linux")
    );
    assert_eq!(
        annotations
            .get("org.nixos.nixcache.merkle_root")
            .map(|s| s.as_str()),
        Some("sha256:merkle123")
    );

    let bloom_layer = &index_manifest.layers[1];
    assert_eq!(
        bloom_layer.media_type,
        CacheLayerMediaType::BLOOM_FILTER_V5_ZSTD
    );
    assert_eq!(bloom_layer.digest, "sha256:bloomblob456");
    assert_eq!(bloom_layer.size, 120);

    let b_ann = bloom_layer.annotations.as_ref().unwrap();
    assert_eq!(
        b_ann.get("org.nixos.nixcache.type").map(|s| s.as_str()),
        Some("bloom_filter")
    );
    assert_eq!(
        b_ann.get("org.nixos.nixcache.schema").map(|s| s.as_str()),
        Some("5")
    );

    let delta_manifest = build_delta_patch_manifest(
        "sha256:deltablob789",
        600,
        "sha256:config123",
        2,
        999,
        "job:build-worker",
        &system,
    );

    assert_eq!(delta_manifest.schema_version, 2);
    assert_eq!(delta_manifest.layers.len(), 1);

    let d_layer = &delta_manifest.layers[0];
    assert_eq!(d_layer.media_type, CacheLayerMediaType::DELTA_PATCH_V5_ZSTD);
    assert_eq!(d_layer.digest, "sha256:deltablob789");
    assert_eq!(d_layer.size, 600);

    let d_ann = d_layer.annotations.as_ref().unwrap();
    assert_eq!(
        d_ann.get("org.nixos.nixcache.schema").map(|s| s.as_str()),
        Some("5")
    );
    assert_eq!(
        d_ann.get("org.nixos.nixcache.run_id").map(|s| s.as_str()),
        Some("999")
    );
    assert_eq!(
        d_ann.get("org.nixos.nixcache.job_id").map(|s| s.as_str()),
        Some("job:build-worker")
    );
    assert_eq!(
        d_ann.get("org.nixos.nixcache.system").map(|s| s.as_str()),
        Some("x86_64-linux")
    );
}

#[tokio::test]
async fn test_push_zstd_blob_and_fetch_sharded_arch_cache_index() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let (mut arch_data, bloom, shard_payload) =
        sample_sharded_arch_index_data(SystemArch::X86_64Linux);
    let bloom_bytes = IndexCodec::encode_bloom_filter(&bloom, 3).unwrap();
    let bloom_digest = compute_sha256(&bloom_bytes);

    let shard_bytes = IndexCodec::encode_zstd(&shard_payload, 3).unwrap();
    let shard_digest = compute_sha256(&shard_bytes);

    arch_data.bloom_filter = BloomFilterManifest::new(
        bloom.num_entries(),
        bloom.num_bits(),
        bloom.num_hashes(),
        &bloom_digest,
        bloom_bytes.len() as u64,
    );
    arch_data.shards[42] = nixcache_core::ShardDescriptor::new(
        42,
        &shard_digest,
        shard_bytes.len() as u64,
        1500,
        shard_payload.len(),
        shard_payload.compute_merkle_hash(),
    );
    arch_data.recalculate_merkle_root();

    let compressed_bytes = IndexCodec::encode_zstd(&arch_data, 3).unwrap();
    let blob_digest = compute_sha256(&compressed_bytes);
    let blob_size = compressed_bytes.len() as u64;

    let sub_manifest = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
        root_blob_digest: &blob_digest,
        root_blob_size: blob_size,
        bloom_blob_digest: &bloom_digest,
        bloom_blob_size: bloom_bytes.len() as u64,
        config_digest: EMPTY_CONFIG_DIGEST,
        config_size: EMPTY_CONFIG_SIZE,
        system: &SystemArch::X86_64Linux,
        merkle_root: &arch_data.merkle_root,
    });
    let manifest_json = sub_manifest.to_json_string().unwrap();

    // 1. HEAD blob (not exist)
    Mock::given(method("HEAD"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/blobs/{}",
            blob_digest
        )))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // 2. POST upload 1-RTT
    Mock::given(method("POST"))
        .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
        .respond_with(
            ResponseTemplate::new(201).insert_header("Docker-Content-Digest", blob_digest.as_str()),
        )
        .mount(&server)
        .await;

    // 3. GET manifest
    Mock::given(method("GET"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/cache-index-x86_64-linux",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(&manifest_json)
                .insert_header("Docker-Content-Digest", "sha256:submanifestdigest"),
        )
        .mount(&server)
        .await;

    // 4. GET blob
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/blobs/{}",
            blob_digest
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(compressed_bytes.to_vec()))
        .mount(&server)
        .await;

    let client = create_tokio_reqwest_client(&host, "test/repo", "token123", true);

    // Push blob
    let (pushed_digest, comp_size, uncomp_size) = client.push_zstd_blob(&arch_data).await.unwrap();
    assert_eq!(pushed_digest, blob_digest);
    assert_eq!(comp_size, blob_size);
    assert!(uncomp_size > comp_size);

    // Fetch sharded arch index
    let fetched = client
        .get_sharded_root_index("cache-index", &SystemArch::X86_64Linux)
        .await
        .unwrap();
    assert!(fetched.is_some());

    let (fetched_data, digest) = fetched.unwrap();
    assert_eq!(digest, "sha256:submanifestdigest");
    assert_eq!(fetched_data.system, SystemArch::X86_64Linux);
    assert_eq!(fetched_data.public_key, arch_data.public_key);
    assert_eq!(fetched_data.shards.len(), NUM_SHARDS);
    assert_eq!(fetched_data.total_entries(), 1);
    assert_eq!(fetched_data.shards[42].blob_digest, shard_digest);
}

#[tokio::test]
async fn test_push_zstd_blob_and_fetch_delta_patch_manifest() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let delta_data = sample_delta_patch_data(101, SystemArch::Aarch64Linux);
    let compressed_bytes = IndexCodec::encode_zstd(&delta_data, 3).unwrap();
    let blob_digest = compute_sha256(&compressed_bytes);
    let blob_size = compressed_bytes.len() as u64;

    let sub_manifest = build_delta_patch_manifest(
        &blob_digest,
        blob_size,
        EMPTY_CONFIG_DIGEST,
        EMPTY_CONFIG_SIZE,
        101,
        "job:workflow-step",
        &SystemArch::Aarch64Linux,
    );
    let manifest_json = sub_manifest.to_json_string().unwrap();

    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(201).insert_header("Docker-Content-Digest", blob_digest.as_str()),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/run-101-aarch64-linux",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(&manifest_json)
                .insert_header("Docker-Content-Digest", "sha256:sessionsubdigest"),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/blobs/{}",
            blob_digest
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(compressed_bytes.to_vec()))
        .mount(&server)
        .await;

    let client = create_tokio_reqwest_client(&host, "test/repo", "token123", true);

    let (pushed_digest, comp_size, _) = client.push_zstd_blob(&delta_data).await.unwrap();
    assert_eq!(pushed_digest, blob_digest);
    assert_eq!(comp_size, blob_size);

    let fetched = client
        .get_delta_patch_manifest("run-101-aarch64-linux")
        .await
        .unwrap();
    assert!(fetched.is_some());

    let (fetched_delta, digest) = fetched.unwrap();
    assert_eq!(digest, "sha256:sessionsubdigest");
    assert_eq!(fetched_delta.run_id, 101);
    assert_eq!(fetched_delta.system, SystemArch::Aarch64Linux);
    assert_eq!(fetched_delta.new_entries.len(), 1);
    assert_eq!(fetched_delta.active_gc_roots.len(), 1);
}

#[tokio::test]
async fn test_get_multi_arch_sharded_index_routing() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let (mut data_x86, bloom_x86, shard_x86) =
        sample_sharded_arch_index_data(SystemArch::X86_64Linux);
    let bloom_bytes_x86 = IndexCodec::encode_bloom_filter(&bloom_x86, 3).unwrap();
    let bloom_digest_x86 = compute_sha256(&bloom_bytes_x86);
    let shard_bytes_x86 = IndexCodec::encode_zstd(&shard_x86, 3).unwrap();
    let shard_digest_x86 = compute_sha256(&shard_bytes_x86);

    data_x86.bloom_filter = BloomFilterManifest::new(
        bloom_x86.num_entries(),
        bloom_x86.num_bits(),
        bloom_x86.num_hashes(),
        &bloom_digest_x86,
        bloom_bytes_x86.len() as u64,
    );
    data_x86.shards[42] = nixcache_core::ShardDescriptor::new(
        42,
        &shard_digest_x86,
        shard_bytes_x86.len() as u64,
        1500,
        shard_x86.len(),
        shard_x86.compute_merkle_hash(),
    );
    data_x86.recalculate_merkle_root();

    let bytes_x86 = IndexCodec::encode_zstd(&data_x86, 3).unwrap();
    let digest_x86 = compute_sha256(&bytes_x86);

    let (mut data_arm, bloom_arm, shard_arm) =
        sample_sharded_arch_index_data(SystemArch::Aarch64Linux);
    let bloom_bytes_arm = IndexCodec::encode_bloom_filter(&bloom_arm, 3).unwrap();
    let bloom_digest_arm = compute_sha256(&bloom_bytes_arm);
    let shard_bytes_arm = IndexCodec::encode_zstd(&shard_arm, 3).unwrap();
    let shard_digest_arm = compute_sha256(&shard_bytes_arm);

    data_arm.bloom_filter = BloomFilterManifest::new(
        bloom_arm.num_entries(),
        bloom_arm.num_bits(),
        bloom_arm.num_hashes(),
        &bloom_digest_arm,
        bloom_bytes_arm.len() as u64,
    );
    data_arm.shards[42] = nixcache_core::ShardDescriptor::new(
        42,
        &shard_digest_arm,
        shard_bytes_arm.len() as u64,
        1500,
        shard_arm.len(),
        shard_arm.compute_merkle_hash(),
    );
    data_arm.recalculate_merkle_root();

    let bytes_arm = IndexCodec::encode_zstd(&data_arm, 3).unwrap();
    let digest_arm = compute_sha256(&bytes_arm);

    let manifest_x86 = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
        root_blob_digest: &digest_x86,
        root_blob_size: bytes_x86.len() as u64,
        bloom_blob_digest: &bloom_digest_x86,
        bloom_blob_size: bloom_bytes_x86.len() as u64,
        config_digest: EMPTY_CONFIG_DIGEST,
        config_size: EMPTY_CONFIG_SIZE,
        system: &SystemArch::X86_64Linux,
        merkle_root: &data_x86.merkle_root,
    });
    let json_x86 = manifest_x86.to_json_string().unwrap();
    let sub_digest_x86 = compute_sha256(json_x86.as_bytes());

    let manifest_arm = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
        root_blob_digest: &digest_arm,
        root_blob_size: bytes_arm.len() as u64,
        bloom_blob_digest: &bloom_digest_arm,
        bloom_blob_size: bloom_bytes_arm.len() as u64,
        config_digest: EMPTY_CONFIG_DIGEST,
        config_size: EMPTY_CONFIG_SIZE,
        system: &SystemArch::Aarch64Linux,
        merkle_root: &data_arm.merkle_root,
    });
    let json_arm = manifest_arm.to_json_string().unwrap();
    let sub_digest_arm = compute_sha256(json_arm.as_bytes());

    let desc_x86 = OciDescriptor {
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        digest: sub_digest_x86.clone(),
        size: json_x86.len() as u64,
        platform: Some(OciPlatform::from_system(&SystemArch::X86_64Linux)),
        annotations: Some(HashMap::from([(
            "org.nixos.nixcache.system".to_string(),
            "x86_64-linux".to_string(),
        )])),
    };
    let desc_arm = OciDescriptor {
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        digest: sub_digest_arm.clone(),
        size: json_arm.len() as u64,
        platform: Some(OciPlatform::from_system(&SystemArch::Aarch64Linux)),
        annotations: Some(HashMap::from([(
            "org.nixos.nixcache.system".to_string(),
            "aarch64-linux".to_string(),
        )])),
    };

    let image_index = build_image_index(vec![desc_x86, desc_arm], "Multi-Arch Baseline Index");
    let index_json = image_index.to_json_string().unwrap();

    // 0. GET individual arch tags return 404
    Mock::given(method("GET"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/cache-index-x86_64-linux",
        ))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/cache-index-aarch64-linux",
        ))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // 1. GET Image Index (cache-index)
    Mock::given(method("GET"))
        .and(path("/v2/test/repo/nix-cache/manifests/cache-index"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(&index_json)
                .insert_header("Docker-Content-Digest", "sha256:topindexdigest"),
        )
        .mount(&server)
        .await;

    // 2. GET Sub-Manifest x86
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/manifests/{}",
            sub_digest_x86
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(&json_x86))
        .mount(&server)
        .await;

    // 3. GET Sub-Manifest arm
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/manifests/{}",
            sub_digest_arm
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(&json_arm))
        .mount(&server)
        .await;

    // 4. GET Blobs
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/blobs/{}",
            digest_x86
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes_x86.to_vec()))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/blobs/{}",
            digest_arm
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes_arm.to_vec()))
        .mount(&server)
        .await;

    let client = create_tokio_reqwest_client(&host, "test/repo", "token123", true);

    let (fetched_x86, digest) = client
        .get_sharded_root_index("cache-index", &SystemArch::X86_64Linux)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(digest, "sha256:topindexdigest");
    assert_eq!(fetched_x86.system, SystemArch::X86_64Linux);
    assert_eq!(fetched_x86.total_entries(), 1);

    let (fetched_arm, digest) = client
        .get_sharded_root_index("cache-index", &SystemArch::Aarch64Linux)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(digest, "sha256:topindexdigest");
    assert_eq!(fetched_arm.system, SystemArch::Aarch64Linux);
    assert_eq!(fetched_arm.total_entries(), 1);
}

#[tokio::test]
async fn test_get_sharded_root_index_rejects_unsupported_media_type() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let legacy_sub_manifest = OciImageManifest {
        schema_version: 2,
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        config: OciDescriptor {
            media_type: "application/vnd.oci.image.config.v1+json".to_string(),
            digest: "sha256:cfg123".to_string(),
            size: 2,
            platform: None,
            annotations: None,
        },
        layers: vec![OciDescriptor {
            media_type: "application/vnd.nix.cache.index.v1+json".to_string(),
            digest: "sha256:blob123".to_string(),
            size: 100,
            platform: Some(OciPlatform::from_system(&SystemArch::X86_64Linux)),
            annotations: None,
        }],
        annotations: None,
    };
    let manifest_json = legacy_sub_manifest.to_json_string().unwrap();

    Mock::given(method("GET"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/cache-index-x86_64-linux",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(&manifest_json))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/test/repo/nix-cache/blobs/sha256:blob123"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"{}"))
        .mount(&server)
        .await;

    let client = create_tokio_reqwest_client(&host, "test/repo", "token123", true);
    let err = client
        .get_sharded_root_index("cache-index", &SystemArch::X86_64Linux)
        .await
        .expect_err("Should reject legacy v1+json media type");

    assert!(matches!(err, OciError::UnsupportedMediaType(_)));
}

#[tokio::test]
async fn test_get_sharded_root_index_rejects_corrupted_blob_data() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let sub_manifest = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
        root_blob_digest: "sha256:corruptblob",
        root_blob_size: 100,
        bloom_blob_digest: "sha256:bloomblob",
        bloom_blob_size: 100,
        config_digest: EMPTY_CONFIG_DIGEST,
        config_size: EMPTY_CONFIG_SIZE,
        system: &SystemArch::X86_64Linux,
        merkle_root: "sha256:merkle123",
    });
    let manifest_json = sub_manifest.to_json_string().unwrap();

    Mock::given(method("GET"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/cache-index-x86_64-linux",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(&manifest_json))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/test/repo/nix-cache/blobs/sha256:corruptblob"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"not a valid zstd stream"))
        .mount(&server)
        .await;

    let client = create_tokio_reqwest_client(&host, "test/repo", "token123", true);
    let err = client
        .get_sharded_root_index("cache-index", &SystemArch::X86_64Linux)
        .await
        .expect_err("Should reject invalid zstd magic blob");

    assert!(matches!(err, OciError::Compression(_)));
}

#[tokio::test]
async fn test_get_shard_data_and_bloom_filter_roundtrip() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let (_root_data, bloom, shard_payload) =
        sample_sharded_arch_index_data(SystemArch::X86_64Linux);
    let shard_bytes = IndexCodec::encode_zstd(&shard_payload, 3).unwrap();
    let shard_digest = compute_sha256(&shard_bytes);

    let bloom_bytes = IndexCodec::encode_bloom_filter(&bloom, 3).unwrap();
    let bloom_digest = compute_sha256(&bloom_bytes);

    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("Docker-Content-Digest", shard_digest.as_str()),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/blobs/{}",
            shard_digest
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(shard_bytes.to_vec()))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/blobs/{}",
            bloom_digest
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bloom_bytes.to_vec()))
        .mount(&server)
        .await;

    let client = create_tokio_reqwest_client(&host, "test/repo", "token123", true);

    let (pushed_digest, comp_size, _) = client.push_shard_data(&shard_payload).await.unwrap();
    assert_eq!(pushed_digest, shard_digest);
    assert_eq!(comp_size, shard_bytes.len() as u64);

    let retrieved_shard = client.get_shard_data(&shard_digest).await.unwrap();
    assert_eq!(retrieved_shard.shard_id, 42);
    assert_eq!(retrieved_shard.entries.len(), 1);

    let retrieved_bloom = client
        .get_bloom_filter(&bloom_digest, bloom.num_entries(), bloom.num_hashes())
        .await
        .unwrap();
    let h1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
    assert!(retrieved_bloom.contains(&h1));
}

#[tokio::test]
async fn test_update_arch_session_with_cas_zstd_roundtrip() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(202).insert_header(
            "Location",
            "/v2/test/repo/nix-cache/blobs/uploads/upload-session-cas",
        ))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(
            "/v2/test/repo/nix-cache/blobs/uploads/upload-session-cas",
        ))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/run-555-x86_64-linux",
        ))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/run-555-x86_64-linux",
        ))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let client = create_tokio_reqwest_client(&host, "test/repo", "token123", true);
    let mut entries = HashMap::new();
    let hash = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
    entries.insert(
        hash.clone(),
        IndexEntry {
            name: "pkg-cas".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg-cas", hash),
                nar_basename: "pkg-cas.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 1024,
            added: "2026-08-29T12:00:00Z".to_string(),
            origin_job: Some("job:cas-test".to_string()),
        },
    );

    let req = SessionMutationRequest::new(555, "cas-test", SystemArch::X86_64Linux)
        .with_entries(entries)
        .with_roots(vec![hash])
        .with_git_info(
            Some("sha-123".to_string()),
            Some("refs/heads/main".to_string()),
        )
        .with_public_key(Some("pubkey:123".to_string()))
        .with_upload_stats(1, 1024);

    let res = client.update_arch_session_with_cas(req).await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn test_update_sharded_arch_index_cas_flow() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    Mock::given(method("GET"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/cache-index-x86_64-linux",
        ))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
        .respond_with(ResponseTemplate::new(202).insert_header(
            "Location",
            "/v2/test/repo/nix-cache/blobs/uploads/upload-sharded-root-cas",
        ))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(
            "/v2/test/repo/nix-cache/blobs/uploads/upload-sharded-root-cas",
        ))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(
            "/v2/test/repo/nix-cache/manifests/cache-index-x86_64-linux",
        ))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let client = create_tokio_reqwest_client(&host, "test/repo", "token123", true);

    let updated_digest = client
        .update_sharded_arch_index_cas("cache-index", &SystemArch::X86_64Linux, 3, |existing| {
            let mut root = existing.unwrap_or_else(|| {
                ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io")
            });
            root.public_key = "cache.example.com-1:key123".to_string();
            Ok((root, "sha256:bloomblob123".to_string(), 120))
        })
        .await
        .unwrap();

    assert!(updated_digest.starts_with("sha256:"));
}
