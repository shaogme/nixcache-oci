use chrono::Utc;
use nixcache_core::{
    ArchCacheIndexData, ArchRunSessionManifest, CACHE_INDEX_VERSION, IndexEntry,
    JobSummaryMetadata, NarDigest, NarInfoMeta, StoreHash, SystemArch,
};
use nixcache_oci::{
    CacheLayerMediaType, IndexCodec, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciClient, OciDescriptor,
    OciImageManifest, OciPlatform, build_arch_index_manifest, build_arch_session_manifest,
    build_image_index,
};
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

fn sample_arch_index_data(system: SystemArch) -> ArchCacheIndexData {
    let mut entries = HashMap::new();
    let hash_str = if system == SystemArch::X86_64Linux {
        "s66mzxpvicwk07gjbjfw9izjfa797vsw"
    } else {
        "s66mzxpvicwk07gjbjfw9izjfa797vsa"
    };
    let hash = StoreHash::parse(hash_str).unwrap();
    entries.insert(
        hash.clone(),
        IndexEntry {
            name: format!("pkg-{}", system.as_str()),
            system: Some(system.clone()),
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

    ArchCacheIndexData {
        version: CACHE_INDEX_VERSION,
        system: system.clone(),
        repo: "test/repo".to_string(),
        registry: "ghcr.io".to_string(),
        generated: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        public_key: "cache.example.com-1:key123".to_string(),
        entries,
        gc_roots: vec![hash],
        last_promoted_run: Some(42),
    }
}

fn sample_arch_session_manifest(run_id: u64, system: SystemArch) -> ArchRunSessionManifest {
    let mut session = ArchRunSessionManifest::new(run_id, system.clone());
    session.head_sha = "abcd1234efgh5678".to_string();
    session.ref_name = "refs/heads/main".to_string();
    session.public_key = Some("cache.example.com-1:key123".to_string());
    let hash = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
    session.entries.insert(
        hash.clone(),
        IndexEntry {
            name: format!("session-pkg-{}", system.as_str()),
            system: Some(system.clone()),
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
    session.gc_roots.push(hash);
    session.completed_jobs.push(JobSummaryMetadata {
        job_id: "job:workflow-step".to_string(),
        system,
        uploaded_blobs: 1,
        uploaded_bytes: 4096,
        timestamp: "2026-08-29T12:00:00Z".to_string(),
    });
    session
}

#[test]
fn test_manifest_builder_generates_v3_zstd_descriptors() {
    let system = SystemArch::X86_64Linux;
    let index_manifest = build_arch_index_manifest(
        "sha256:indexblob123",
        500,
        5000,
        "sha256:config123",
        2,
        &system,
    );

    assert_eq!(index_manifest.schema_version, 2);
    assert_eq!(index_manifest.media_type, OCI_IMAGE_MANIFEST_MEDIA_TYPE);
    assert_eq!(index_manifest.layers.len(), 1);

    let layer = &index_manifest.layers[0];
    assert_eq!(layer.media_type, CacheLayerMediaType::INDEX_V3_ZSTD);
    assert_eq!(layer.digest, "sha256:indexblob123");
    assert_eq!(layer.size, 500);

    let annotations = layer.annotations.as_ref().unwrap();
    assert_eq!(
        annotations
            .get("org.nixos.nixcache.compression")
            .map(|s| s.as_str()),
        Some("zstd")
    );
    assert_eq!(
        annotations
            .get("org.nixos.nixcache.schema")
            .map(|s| s.as_str()),
        Some("5")
    );
    assert_eq!(
        annotations
            .get("org.nixos.nixcache.uncompressed_size")
            .map(|s| s.as_str()),
        Some("5000")
    );
    assert_eq!(
        annotations
            .get("org.nixos.nixcache.system")
            .map(|s| s.as_str()),
        Some("x86_64-linux")
    );

    let session_manifest = build_arch_session_manifest(
        "sha256:sessionblob456",
        600,
        6000,
        "sha256:config123",
        2,
        999,
        &system,
    );

    assert_eq!(session_manifest.schema_version, 2);
    assert_eq!(session_manifest.layers.len(), 1);

    let s_layer = &session_manifest.layers[0];
    assert_eq!(s_layer.media_type, CacheLayerMediaType::SESSION_V3_ZSTD);
    assert_eq!(s_layer.digest, "sha256:sessionblob456");
    assert_eq!(s_layer.size, 600);

    let s_ann = s_layer.annotations.as_ref().unwrap();
    assert_eq!(
        s_ann
            .get("org.nixos.nixcache.compression")
            .map(|s| s.as_str()),
        Some("zstd")
    );
    assert_eq!(
        s_ann.get("org.nixos.nixcache.schema").map(|s| s.as_str()),
        Some("5")
    );
    assert_eq!(
        s_ann.get("org.nixos.nixcache.run_id").map(|s| s.as_str()),
        Some("999")
    );
}

#[tokio::test]
async fn test_push_zstd_blob_and_fetch_arch_cache_index() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let arch_data = sample_arch_index_data(SystemArch::X86_64Linux);
    let compressed_bytes = IndexCodec::encode_zstd(&arch_data, 3).unwrap();
    let blob_digest = compute_sha256(&compressed_bytes);
    let blob_size = compressed_bytes.len() as u64;

    let sub_manifest = build_arch_index_manifest(
        &blob_digest,
        blob_size,
        1500,
        "sha256:emptycfg",
        2,
        &SystemArch::X86_64Linux,
    );
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

    let client = OciClient::new(&host, "test/repo", "token123", true);

    // Push blob
    let (pushed_digest, comp_size, uncomp_size) = client.push_zstd_blob(&arch_data).await.unwrap();
    assert_eq!(pushed_digest, blob_digest);
    assert_eq!(comp_size, blob_size);
    assert!(uncomp_size > comp_size);

    // Fetch arch index
    let fetched = client
        .get_arch_cache_index("cache-index", &SystemArch::X86_64Linux)
        .await
        .unwrap();
    assert!(fetched.is_some());

    let (fetched_data, digest) = fetched.unwrap();
    assert_eq!(digest, "sha256:submanifestdigest");
    assert_eq!(fetched_data.system, SystemArch::X86_64Linux);
    assert_eq!(fetched_data.public_key, arch_data.public_key);
    assert_eq!(fetched_data.entries.len(), arch_data.entries.len());
}

#[tokio::test]
async fn test_push_zstd_blob_and_fetch_arch_session_manifest() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let session_data = sample_arch_session_manifest(101, SystemArch::Aarch64Linux);
    let compressed_bytes = IndexCodec::encode_zstd(&session_data, 3).unwrap();
    let blob_digest = compute_sha256(&compressed_bytes);
    let blob_size = compressed_bytes.len() as u64;

    let sub_manifest = build_arch_session_manifest(
        &blob_digest,
        blob_size,
        2000,
        "sha256:emptycfg",
        2,
        101,
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

    let client = OciClient::new(&host, "test/repo", "token123", true);

    let (pushed_digest, comp_size, _) = client.push_zstd_blob(&session_data).await.unwrap();
    assert_eq!(pushed_digest, blob_digest);
    assert_eq!(comp_size, blob_size);

    let fetched = client
        .get_arch_session_manifest("run-101", &SystemArch::Aarch64Linux)
        .await
        .unwrap();
    assert!(fetched.is_some());

    let (fetched_session, digest) = fetched.unwrap();
    assert_eq!(digest, "sha256:sessionsubdigest");
    assert_eq!(fetched_session.run_id, 101);
    assert_eq!(fetched_session.system, SystemArch::Aarch64Linux);
    assert_eq!(fetched_session.entries.len(), 1);
}

#[tokio::test]
async fn test_get_multi_arch_cache_index_aggregation() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let data_x86 = sample_arch_index_data(SystemArch::X86_64Linux);
    let bytes_x86 = IndexCodec::encode_zstd(&data_x86, 3).unwrap();
    let digest_x86 = compute_sha256(&bytes_x86);

    let data_arm = sample_arch_index_data(SystemArch::Aarch64Linux);
    let bytes_arm = IndexCodec::encode_zstd(&data_arm, 3).unwrap();
    let digest_arm = compute_sha256(&bytes_arm);

    let manifest_x86 = build_arch_index_manifest(
        &digest_x86,
        bytes_x86.len() as u64,
        1500,
        "sha256:emptycfg",
        2,
        &SystemArch::X86_64Linux,
    );
    let json_x86 = manifest_x86.to_json_string().unwrap();
    let sub_digest_x86 = compute_sha256(json_x86.as_bytes());

    let manifest_arm = build_arch_index_manifest(
        &digest_arm,
        bytes_arm.len() as u64,
        1500,
        "sha256:emptycfg",
        2,
        &SystemArch::Aarch64Linux,
    );
    let json_arm = manifest_arm.to_json_string().unwrap();
    let sub_digest_arm = compute_sha256(json_arm.as_bytes());

    let desc_x86 = OciDescriptor {
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        digest: sub_digest_x86.clone(),
        size: json_x86.len() as u64,
        platform: Some(OciPlatform::from_system(&SystemArch::X86_64Linux)),
        annotations: None,
    };
    let desc_arm = OciDescriptor {
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        digest: sub_digest_arm.clone(),
        size: json_arm.len() as u64,
        platform: Some(OciPlatform::from_system(&SystemArch::Aarch64Linux)),
        annotations: None,
    };

    let image_index = build_image_index(vec![desc_x86, desc_arm], "Multi-Arch Baseline Index");
    let index_json = image_index.to_json_string().unwrap();

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

    let client = OciClient::new(&host, "test/repo", "token123", true);

    let (combined, digest) = client
        .get_cache_index("cache-index")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(digest, "sha256:topindexdigest");
    assert_eq!(combined.entries.len(), 2);
    assert_eq!(combined.gc_roots.len(), 2);
    assert!(combined.gc_roots.contains_key(&SystemArch::X86_64Linux));
    assert!(combined.gc_roots.contains_key(&SystemArch::Aarch64Linux));
}

#[tokio::test]
async fn test_get_arch_cache_index_rejects_unsupported_media_type() {
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

    let client = OciClient::new(&host, "test/repo", "token123", true);
    let err = client
        .get_arch_cache_index("cache-index", &SystemArch::X86_64Linux)
        .await
        .expect_err("Should reject legacy v1+json media type");

    assert!(matches!(
        err,
        nixcache_oci::OciError::UnsupportedMediaType(_)
    ));
}

#[tokio::test]
async fn test_get_arch_cache_index_rejects_corrupted_blob_data() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let sub_manifest = build_arch_index_manifest(
        "sha256:corruptblob",
        100,
        1000,
        "sha256:cfg123",
        2,
        &SystemArch::X86_64Linux,
    );
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

    let client = OciClient::new(&host, "test/repo", "token123", true);
    let err = client
        .get_arch_cache_index("cache-index", &SystemArch::X86_64Linux)
        .await
        .expect_err("Should reject invalid zstd magic blob");

    assert!(matches!(err, nixcache_oci::OciError::CompressionError(_)));
}

#[tokio::test]
async fn test_get_multi_arch_session_manifest_aggregation() {
    let server = MockServer::start().await;
    let host = server.address().to_string();

    let sess_x86 = sample_arch_session_manifest(200, SystemArch::X86_64Linux);
    let bytes_x86 = IndexCodec::encode_zstd(&sess_x86, 3).unwrap();
    let digest_x86 = compute_sha256(&bytes_x86);

    let sess_arm = sample_arch_session_manifest(200, SystemArch::Aarch64Linux);
    let bytes_arm = IndexCodec::encode_zstd(&sess_arm, 3).unwrap();
    let digest_arm = compute_sha256(&bytes_arm);

    let manifest_x86 = build_arch_session_manifest(
        &digest_x86,
        bytes_x86.len() as u64,
        2000,
        "sha256:emptycfg",
        2,
        200,
        &SystemArch::X86_64Linux,
    );
    let json_x86 = manifest_x86.to_json_string().unwrap();
    let sub_digest_x86 = compute_sha256(json_x86.as_bytes());

    let manifest_arm = build_arch_session_manifest(
        &digest_arm,
        bytes_arm.len() as u64,
        2000,
        "sha256:emptycfg",
        2,
        200,
        &SystemArch::Aarch64Linux,
    );
    let json_arm = manifest_arm.to_json_string().unwrap();
    let sub_digest_arm = compute_sha256(json_arm.as_bytes());

    let desc_x86 = OciDescriptor {
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        digest: sub_digest_x86.clone(),
        size: json_x86.len() as u64,
        platform: Some(OciPlatform::from_system(&SystemArch::X86_64Linux)),
        annotations: None,
    };
    let desc_arm = OciDescriptor {
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string(),
        digest: sub_digest_arm.clone(),
        size: json_arm.len() as u64,
        platform: Some(OciPlatform::from_system(&SystemArch::Aarch64Linux)),
        annotations: None,
    };

    let image_index = build_image_index(vec![desc_x86, desc_arm], "Multi-Arch Run Session Index");
    let index_json = image_index.to_json_string().unwrap();

    Mock::given(method("GET"))
        .and(path("/v2/test/repo/nix-cache/manifests/run-200"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(&index_json)
                .insert_header("Docker-Content-Digest", "sha256:toprunindex"),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/manifests/{}",
            sub_digest_x86
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(&json_x86))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/repo/nix-cache/manifests/{}",
            sub_digest_arm
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(&json_arm))
        .mount(&server)
        .await;

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

    let client = OciClient::new(&host, "test/repo", "token123", true);

    let (combined, digest) = client
        .get_session_manifest("run-200")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(digest, "sha256:toprunindex");
    assert_eq!(combined.run_id, 200);
    assert_eq!(combined.head_sha, "abcd1234efgh5678");
    assert_eq!(combined.completed_jobs.len(), 2);
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

    let client = OciClient::new(&host, "test/repo", "token123", true);
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

    let req = nixcache_oci::SessionMutationRequest::new(555, "cas-test", SystemArch::X86_64Linux)
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
