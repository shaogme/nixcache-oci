use nixcache_oci::{
    CacheLayerMediaType, DockerHubDriver, GenericOciDriver, IndexEntry, MockRouterTransport,
    OciClient, OciDescriptor, OciImageIndex, OciPlatform, ShardDataPayload, ShardDescriptor,
    ShardedArchCacheIndexData, StoreHash, SystemArch, build_image_index,
};

#[tokio::test]
async fn sharded_root_and_shard_data_round_trip() {
    let client = OciClient::with_transport(
        "example.com",
        "test/repo",
        "",
        true,
        MockRouterTransport::default(),
        Default::default(),
    )
    .unwrap();
    let hash = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
    let shard_id = hash.shard_id();
    let mut shard = ShardDataPayload::new(shard_id);
    shard.entries.insert(
        hash,
        IndexEntry {
            system: Some(SystemArch::X86_64Linux),
            nar_digest: nixcache_core::NarDigest::new_sha256(
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap(),
            ..Default::default()
        },
    );
    let (shard_digest, compressed_size, uncompressed_size) =
        client.indexes().push_shard_data(&shard).await.unwrap();
    let descriptor = ShardDescriptor::new(
        shard_id,
        shard_digest.clone(),
        compressed_size,
        uncompressed_size,
        shard.len(),
        shard.compute_merkle_hash(),
    );
    let fetched_shard = client
        .indexes()
        .get_shard_data(&descriptor, &SystemArch::X86_64Linux)
        .await
        .unwrap();
    assert_eq!(fetched_shard.shard_id, shard_id);
    assert_eq!(fetched_shard.entries.len(), 1);

    let mut root =
        ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "example.com");
    root.shards[shard_id as usize].blob_digest = shard_digest.clone();
    root.shards[shard_id as usize].compressed_size = compressed_size;
    root.shards[shard_id as usize].uncompressed_size = uncompressed_size;
    root.shards[shard_id as usize].entry_count = 1;
    root.shards[shard_id as usize].merkle_hash = shard.compute_merkle_hash();
    root.recalculate_merkle_root();
    let manifest_digest = client
        .indexes()
        .push_sharded_root("cache-index-x86_64-linux", &root)
        .await
        .unwrap();
    let (fetched_root, digest) = client
        .indexes()
        .get_sharded_root("cache-index", &SystemArch::X86_64Linux)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(digest, manifest_digest);
    assert_eq!(
        fetched_root.shards[shard_id as usize].blob_digest,
        shard_digest
    );
}

#[tokio::test]
async fn image_index_routes_to_the_requested_architecture() {
    let client = OciClient::with_transport(
        "example.com",
        "test/repo",
        "",
        true,
        MockRouterTransport::default(),
        Default::default(),
    )
    .unwrap();
    let root_x86 =
        ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "example.com");
    let root_arm =
        ShardedArchCacheIndexData::new(SystemArch::Aarch64Linux, "test/repo", "example.com");
    let digest_x86 = client
        .indexes()
        .push_sharded_root("sub-manifest-x86", &root_x86)
        .await
        .unwrap();
    let digest_arm = client
        .indexes()
        .push_sharded_root("sub-manifest-arm", &root_arm)
        .await
        .unwrap();
    let manifest_size_x86 = client
        .manifests()
        .get("sub-manifest-x86")
        .await
        .unwrap()
        .unwrap()
        .len() as u64;
    let manifest_size_arm = client
        .manifests()
        .get("sub-manifest-arm")
        .await
        .unwrap()
        .unwrap()
        .len() as u64;
    let index = build_image_index(
        vec![
            OciDescriptor {
                media_type: nixcache_oci::OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
                digest: digest_x86,
                size: manifest_size_x86,
                platform: Some(OciPlatform::from_system(&SystemArch::X86_64Linux)),
                annotations: None,
            },
            OciDescriptor {
                media_type: nixcache_oci::OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
                digest: digest_arm,
                size: manifest_size_arm,
                platform: Some(OciPlatform::from_system(&SystemArch::Aarch64Linux)),
                annotations: None,
            },
        ],
        "Multi-Arch Baseline Index",
    );
    client.indexes().put("cache-index", &index).await.unwrap();
    assert_eq!(
        client
            .indexes()
            .get_sharded_root("cache-index", &SystemArch::X86_64Linux)
            .await
            .unwrap()
            .unwrap()
            .0
            .system,
        SystemArch::X86_64Linux
    );
    assert_eq!(
        client
            .indexes()
            .get_sharded_root("cache-index", &SystemArch::Aarch64Linux)
            .await
            .unwrap()
            .unwrap()
            .0
            .system,
        SystemArch::Aarch64Linux
    );
    assert!(
        client
            .indexes()
            .get_sharded_root("cache-index", &SystemArch::Aarch64Darwin)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn sharded_root_cas_update_is_exposed_by_index_client() {
    let client = OciClient::new(
        "example.com",
        "test/repo",
        "",
        true,
        DockerHubDriver,
        MockRouterTransport::default(),
        Default::default(),
    )
    .unwrap();
    client
        .indexes()
        .update_sharded_cas("cache-index", &SystemArch::X86_64Linux, 3, |existing| {
            let mut root = existing.unwrap_or_else(|| {
                ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "example.com")
            });
            root.shards[42] = ShardDescriptor::new(
                42,
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                1,
                1,
                5,
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            );
            root.recalculate_merkle_root();
            Ok(root)
        })
        .await
        .unwrap();
    let (root, _) = client
        .indexes()
        .get_sharded_root("cache-index", &SystemArch::X86_64Linux)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.shards[42].entry_count, 5);
}

#[test]
fn index_data_types_remain_public() {
    let index = OciImageIndex::new();
    assert!(index.media_type.is_empty() || index.media_type.contains("oci"));
    assert_eq!(
        CacheLayerMediaType::ROOT_INDEX_V6_ZSTD,
        "application/vnd.nix.cache.root.v6+zstd"
    );
    let _ = GenericOciDriver;
}
