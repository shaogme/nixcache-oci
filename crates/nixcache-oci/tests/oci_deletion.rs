use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};
use nixcache_core::NarDigest;
use nixcache_oci::{GenericOciDriver, MockResponse, MockRouterTransport, OciClient, OciError};

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
    assert_eq!(summary.deleted_count, 2); // b1 (202) + b2 (404 idempotent)
    assert_eq!(summary.failed_count, 1); // b3 (500)

    // Strict mode aborts on non-404 error
    let strict_err = client
        .batch_delete_blobs_strict(&digests, 4, true)
        .await
        .unwrap_err();
    assert!(matches!(strict_err, OciError::DeletionFailed { .. }));
}
