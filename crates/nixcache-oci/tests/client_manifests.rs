use bytes::Bytes;
use http::{
    HeaderMap, StatusCode,
    header::{IF_MATCH, IF_NONE_MATCH},
};
use nixcache_oci::{
    DockerHubDriver, EMPTY_CONFIG_DIGEST, ManifestCasCondition, MockResponse, MockRouterTransport,
    OciClient, OciError,
};

#[tokio::test]
async fn get_and_put_manifest_use_manifest_client() {
    let manifest_content =
        r#"{"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json"}"#;
    let transport = MockRouterTransport::default();
    transport.add_route(
        "GET",
        "/manifests/cache-index",
        MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(manifest_content),
        },
    );
    transport.add_route(
        "PUT",
        "/manifests/fail-tag",
        MockResponse {
            status: StatusCode::FORBIDDEN,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    let client = OciClient::with_transport(
        "example.com",
        "test/repo",
        "",
        true,
        transport,
        Default::default(),
    )
    .unwrap();
    assert_eq!(
        client.manifests().get("cache-index").await.unwrap(),
        Some(manifest_content.to_string())
    );
    assert!(
        client
            .manifests()
            .put("cache-index", manifest_content)
            .await
            .is_ok()
    );
    assert!(matches!(
        client.manifests().put("fail-tag", manifest_content).await,
        Err(OciError::ManifestPushFailed(StatusCode::FORBIDDEN))
    ));
}

#[tokio::test]
async fn put_manifest_ensures_empty_config_blob() {
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"digest":"{}"}}}}"#,
        EMPTY_CONFIG_DIGEST
    );
    let transport = MockRouterTransport::default();
    transport.add_route(
        "HEAD",
        &format!("/blobs/{EMPTY_CONFIG_DIGEST}"),
        MockResponse {
            status: StatusCode::NOT_FOUND,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    transport.add_route(
        "POST",
        &format!("/blobs/uploads/?digest={EMPTY_CONFIG_DIGEST}"),
        MockResponse {
            status: StatusCode::CREATED,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    transport.add_route(
        "PUT",
        "/manifests/cache-index",
        MockResponse {
            status: StatusCode::CREATED,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    let client = OciClient::with_transport(
        "example.com",
        "test/repo",
        "",
        true,
        transport,
        Default::default(),
    )
    .unwrap();
    assert!(
        client
            .manifests()
            .put("cache-index", &manifest)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn manifest_cas_sets_the_expected_condition_header() {
    let transport = MockRouterTransport::default();
    transport.add_route(
        "PUT",
        "/manifests/run-123",
        MockResponse {
            status: StatusCode::PRECONDITION_FAILED,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    let client = OciClient::new(
        "example.com",
        "test/repo",
        "",
        true,
        DockerHubDriver,
        transport,
        Default::default(),
    )
    .unwrap();
    let error = client
        .manifests()
        .put_cas(
            "run-123",
            "{}",
            ManifestCasCondition::Match("sha256:old".to_string()),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, OciError::CasPreconditionFailed { tag, .. } if tag == "run-123"));
    let request = client.transport().put_requests.pop().unwrap();
    assert_eq!(
        request
            .headers
            .get(IF_MATCH)
            .and_then(|value| value.to_str().ok()),
        Some("sha256:old")
    );
    assert!(request.headers.get(IF_NONE_MATCH).is_none());
}

#[tokio::test]
async fn create_only_manifest_cas_uses_if_none_match() {
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
        .manifests()
        .put_cas("new-tag", "{}", ManifestCasCondition::CreateOnly)
        .await
        .unwrap();
    let request = client.transport().put_requests.pop().unwrap();
    assert_eq!(
        request
            .headers
            .get(IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok()),
        Some("*")
    );
    assert!(request.headers.get(IF_MATCH).is_none());
}

#[tokio::test]
async fn stale_manifest_cas_writer_cannot_overwrite() {
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
        .manifests()
        .put("shared", "{\"version\":0}")
        .await
        .unwrap();
    let (_, old_digest) = client
        .manifests()
        .get_with_digest("shared")
        .await
        .unwrap()
        .unwrap();
    let (_, second_digest) = client
        .manifests()
        .get_with_digest("shared")
        .await
        .unwrap()
        .unwrap();
    client
        .manifests()
        .put_cas(
            "shared",
            "{\"version\":1}",
            ManifestCasCondition::Match(old_digest),
        )
        .await
        .unwrap();
    let error = client
        .manifests()
        .put_cas(
            "shared",
            "{\"version\":2}",
            ManifestCasCondition::Match(second_digest),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, OciError::CasPreconditionFailed { .. }));
    let (body, _) = client
        .manifests()
        .get_with_digest("shared")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, "{\"version\":1}");
}

#[tokio::test]
async fn get_manifest_with_digest_rejects_invalid_utf8() {
    let transport = MockRouterTransport::default();
    transport.add_route(
        "GET",
        "/manifests/valid-tag",
        MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"{\"schemaVersion\":2}"),
        },
    );
    transport.add_route(
        "GET",
        "/manifests/invalid-utf8",
        MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(&[0xff, 0xfe, 0xfd]),
        },
    );
    let client = OciClient::with_transport(
        "example.com",
        "test/repo",
        "",
        false,
        transport,
        Default::default(),
    )
    .unwrap();
    assert!(
        client
            .manifests()
            .get_with_digest("valid-tag")
            .await
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        client
            .manifests()
            .get_with_digest("invalid-utf8")
            .await
            .unwrap_err(),
        OciError::InvalidUtf8Manifest(_)
    ));
}
