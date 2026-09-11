use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};
use nixcache_core::{
    IndexEntry, NarDigest, ShardDataPayload, ShardDescriptor, ShardedArchCacheIndexData, StoreHash,
    SystemArch,
};
use nixcache_oci::{
    CacheLayerMediaType, GenericOciDriver, IndexCodec, MockResponse, MockRouterTransport,
    OciClient, OciError,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

fn digest(body: &str) -> String {
    digest_bytes(body.as_bytes())
}

fn digest_bytes(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    let hash = hasher.finalize();
    format!(
        "sha256:{}",
        hash.iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn store_manifest(transport: &MockRouterTransport, reference: &str, body: &str) -> String {
    let manifest_digest = digest(body);
    let _ = transport.stored_manifests.upsert_sync(
        reference.to_string(),
        (Bytes::from(body.to_string()), manifest_digest.clone()),
    );
    let _ = transport.stored_manifests.upsert_sync(
        manifest_digest.clone(),
        (Bytes::from(body.to_string()), manifest_digest.clone()),
    );
    manifest_digest
}

#[tokio::test]
async fn test_generic_oci_two_stage_tag_deletion() {
    let transport = MockRouterTransport::default();
    let manifest_body =
        r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let mut get_headers = HeaderMap::new();
    get_headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_static("sha256:manifestdigest123"),
    );

    // Stage 1: GET manifest for tag
    transport.add_route(
        "GET",
        "/manifests/run-100",
        MockResponse {
            status: StatusCode::OK,
            headers: get_headers,
            body: Bytes::from_static(manifest_body.as_bytes()),
        },
    );

    // Stage 2: DELETE manifest by digest
    transport.add_route(
        "DELETE",
        "/manifests/sha256:manifestdigest123",
        MockResponse {
            status: StatusCode::ACCEPTED,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );

    let client = OciClient::new(
        "registry.local:5000",
        "myorg/nix-cache",
        "token",
        true,
        GenericOciDriver,
        transport,
    );

    let del_res = client.delete_tag_strict("run-100").await;
    assert!(del_res.is_ok());
}

#[tokio::test]
async fn test_generic_oci_manifest_delete_405_rejected() {
    let transport = MockRouterTransport::default();
    transport.add_route(
        "DELETE",
        "/manifests/sha256:failmanifest",
        MockResponse {
            status: StatusCode::METHOD_NOT_ALLOWED,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );

    let client = OciClient::new(
        "registry.local:5000",
        "myorg/nix-cache",
        "token",
        true,
        GenericOciDriver,
        transport,
    );

    let err = client
        .delete_manifest_strict("sha256:failmanifest")
        .await
        .unwrap_err();
    assert!(matches!(err, OciError::OperationNotSupported { .. }));
}

#[tokio::test]
async fn test_generic_oci_batch_delete_blobs_strict_vs_lenient() {
    let transport = MockRouterTransport::default();
    transport.add_route(
        "DELETE",
        "/blobs/sha256:b1",
        MockResponse {
            status: StatusCode::ACCEPTED,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    transport.add_route(
        "DELETE",
        "/blobs/sha256:b2",
        MockResponse {
            status: StatusCode::NOT_FOUND,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    transport.add_route(
        "DELETE",
        "/blobs/sha256:b3",
        MockResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );

    let client = OciClient::new(
        "registry.local:5000",
        "myorg/nix-cache",
        "token",
        true,
        GenericOciDriver,
        transport,
    );

    let digests = vec![
        NarDigest::new_unchecked("sha256:b1"),
        NarDigest::new_unchecked("sha256:b2"),
        NarDigest::new_unchecked("sha256:b3"),
    ];

    // Non-strict mode accumulates failures without aborting
    let summary = client
        .batch_delete_blobs_strict(&digests, 4, false)
        .await
        .unwrap();
    assert_eq!(summary.deleted_count, 1); // b1 (202)
    assert_eq!(summary.not_found_count, 1); // b2 (404 idempotent)
    assert_eq!(summary.failed_count, 1); // b3 (500)

    // Strict mode aborts on non-404 error
    let strict_err = client
        .batch_delete_blobs_strict(&digests, 4, true)
        .await
        .unwrap_err();
    assert!(matches!(strict_err, OciError::DeletionFailed { .. }));
}

#[tokio::test]
async fn test_generic_oci_deletes_complete_tag_reachable_graph() {
    let transport = MockRouterTransport::default();
    let manifest_one = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":2},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222","size":10}]}"#;
    let manifest_two = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:3333333333333333333333333333333333333333333333333333333333333333","size":2},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"sha256:4444444444444444444444444444444444444444444444444444444444444444","size":10}]}"#;
    let child_one = store_manifest(&transport, "sha256:child-one", manifest_one);
    let child_two = store_manifest(&transport, "sha256:child-two", manifest_two);
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":1}},{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":1}},{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":1}}]}}"#,
        child_one, child_two, child_one
    );
    let index_digest = store_manifest(&transport, "cache-index", &index);
    let _ = transport.stored_manifests.upsert_sync(
        "run-1".to_string(),
        (Bytes::from(index.clone()), index_digest.clone()),
    );
    for blob in [
        "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        "sha256:2222222222222222222222222222222222222222222222222222222222222222",
        "sha256:3333333333333333333333333333333333333333333333333333333333333333",
        "sha256:4444444444444444444444444444444444444444444444444444444444444444",
    ] {
        let _ = transport
            .stored_blobs
            .upsert_sync(blob.to_string(), Bytes::from_static(b"blob"));
    }

    let client = OciClient::new(
        "registry.local:5000",
        "myorg/repo",
        "",
        true,
        GenericOciDriver,
        transport,
    );
    let summary = client.delete_entire_package_strict().await.unwrap();
    assert_eq!(summary.tags_discovered, 2);
    assert_eq!(summary.manifests_discovered, 3);
    assert_eq!(summary.blobs_discovered, 4);
    assert_eq!(summary.manifests_deleted, 3);
    assert_eq!(summary.blobs_deleted, 4);
    assert_eq!(summary.already_absent, 0);
}

#[tokio::test]
async fn test_generic_oci_deletes_root_shard_and_nar_blobs() {
    let transport = MockRouterTransport::default();
    let nar_digest =
        NarDigest::new_sha256("0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0")
            .unwrap();
    let store_hash = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
    let entry = IndexEntry {
        nar_digest: nar_digest.clone(),
        nar_size: 2048,
        ..Default::default()
    };
    let shard_payload = ShardDataPayload::with_entries(42, HashMap::from([(store_hash, entry)]));
    let shard_bytes = IndexCodec::encode_zstd(&shard_payload, 3).unwrap();
    let shard_digest = digest_bytes(&shard_bytes);

    let mut root =
        ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "myorg/repo", "registry.local");
    root.shards[42] = ShardDescriptor::new(
        42,
        &shard_digest,
        shard_bytes.len() as u64,
        2048,
        shard_payload.len(),
        shard_payload.compute_merkle_hash(),
    );
    root.recalculate_merkle_root();
    let root_bytes = IndexCodec::encode_zstd(&root, 3).unwrap();
    let root_digest = digest_bytes(&root_bytes);
    let manifest_body = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","size":2}},"layers":[{{"mediaType":"{}","digest":"{}","size":{}}}]}}"#,
        CacheLayerMediaType::ROOT_INDEX_V6_ZSTD,
        root_digest,
        root_bytes.len()
    );
    store_manifest(&transport, "cache-index", &manifest_body);

    for (digest, bytes) in [
        (root_digest, root_bytes),
        (shard_digest, shard_bytes),
        (nar_digest.to_string(), Bytes::from_static(b"nar")),
        (
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a".to_string(),
            Bytes::from_static(b"{}"),
        ),
    ] {
        let _ = transport.stored_blobs.upsert_sync(digest, bytes);
    }

    let client = OciClient::new(
        "registry.local:5000",
        "myorg/repo",
        "",
        true,
        GenericOciDriver,
        transport,
    );
    let summary = client.delete_entire_package_strict().await.unwrap();
    assert_eq!(summary.tags_discovered, 1);
    assert_eq!(summary.manifests_discovered, 1);
    assert_eq!(summary.blobs_discovered, 4);
    assert_eq!(summary.manifests_deleted, 1);
    assert_eq!(summary.blobs_deleted, 4);
    assert_eq!(summary.already_absent, 0);
}
