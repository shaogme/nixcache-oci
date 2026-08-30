use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use nixcache_oci::{
    GitHubPackagesClient, MockResponse, MockRouterTransport, OciClient, OciError,
    backend::driver::GhcrDriver,
};

#[tokio::test]
async fn test_ghcr_packages_client_owner_routing_org_success() {
    let transport = MockRouterTransport::default();
    // Org endpoint returns 200 with versions JSON
    let versions_json = r#"[
        {
            "id": 101,
            "name": "sha256:abcd",
            "metadata": {
                "container": {
                    "tags": ["run-123", "run-123-x86_64-linux"]
                }
            }
        },
        {
            "id": 102,
            "name": "sha256:ef01",
            "metadata": {
                "container": {
                    "tags": ["run-124"]
                }
            }
        }
    ]"#;

    transport.add_route(
        "GET",
        "/orgs/my-org/packages/container/my-project%2Fnix-cache/versions",
        MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(versions_json.as_bytes()),
        },
    );

    transport.add_route(
        "DELETE",
        "/orgs/my-org/packages/container/my-project%2Fnix-cache/versions/101",
        MockResponse {
            status: StatusCode::NO_CONTENT,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );

    let client = GitHubPackagesClient::new(transport, "fake-token", "my-org/my-project/nix-cache");

    assert_eq!(client.owner(), "my-org");
    assert_eq!(client.package_name(), "my-project%2Fnix-cache");

    let version_id = client.find_version_id_by_tag("run-123").await.unwrap();
    assert_eq!(version_id, Some(101));

    let del_res = client.delete_by_tag("run-123").await;
    assert!(del_res.is_ok());

    let missing_tag = client.find_version_id_by_tag("run-999").await.unwrap();
    assert_eq!(missing_tag, None);

    let del_missing = client.delete_by_tag("run-999").await;
    assert!(del_missing.is_ok());
}

#[tokio::test]
async fn test_ghcr_packages_client_owner_routing_fallback_to_user() {
    let transport = MockRouterTransport::default();
    // Org endpoint returns 404, user endpoint returns 200
    transport.add_route(
        "GET",
        "/orgs/user1/packages/container/nix-cache/versions",
        MockResponse {
            status: StatusCode::NOT_FOUND,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );

    let versions_json = r#"[
        {
            "id": 201,
            "name": "sha256:userblob",
            "metadata": {
                "container": {
                    "tags": ["run-555"]
                }
            }
        }
    ]"#;

    transport.add_route(
        "GET",
        "/users/user1/packages/container/nix-cache/versions",
        MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(versions_json.as_bytes()),
        },
    );

    transport.add_route(
        "DELETE",
        "/users/user1/packages/container/nix-cache/versions/201",
        MockResponse {
            status: StatusCode::NO_CONTENT,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );

    let client = GitHubPackagesClient::new(transport, "fake-token", "user1/nix-cache");

    let version_id = client.find_version_id_by_tag("run-555").await.unwrap();
    assert_eq!(version_id, Some(201));

    let del_res = client.delete_by_tag("run-555").await;
    assert!(del_res.is_ok());
}

#[tokio::test]
async fn test_ghcr_packages_client_permission_denied_remedy_message() {
    let transport = MockRouterTransport::default();
    transport.add_route(
        "DELETE",
        "/orgs/test-org/packages/container/nix-cache/versions/301",
        MockResponse {
            status: StatusCode::FORBIDDEN,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"{\"message\":\"Resource not accessible by integration\"}"),
        },
    );

    let client = GitHubPackagesClient::new(
        transport,
        "token-without-delete-scope",
        "test-org/nix-cache",
    );

    let err = client.delete_package_version(301).await.unwrap_err();
    match err {
        OciError::InsufficientPermission {
            target,
            required_scope,
            details,
        } => {
            assert_eq!(target, "test-org/nix-cache");
            assert_eq!(required_scope, "delete:packages");
            assert!(details.contains("delete:packages"));
            assert!(details.contains("packages: write"));
        }
        other => panic!("Expected InsufficientPermission, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_ghcr_delete_entire_package() {
    let transport = MockRouterTransport::default();
    transport.add_route(
        "DELETE",
        "/orgs/my-org/packages/container/my-repo%2Fnix-cache",
        MockResponse {
            status: StatusCode::NO_CONTENT,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );

    let client = GitHubPackagesClient::new(transport, "token", "my-org/my-repo/nix-cache");

    assert!(client.delete_entire_package().await.is_ok());
}

#[tokio::test]
async fn test_ghcr_client_integration_via_oci_client() {
    let transport = MockRouterTransport::default();
    let versions_json = r#"[
        {
            "id": 501,
            "name": "sha256:tagblob",
            "metadata": {
                "container": {
                    "tags": ["run-99"]
                }
            }
        }
    ]"#;

    transport.add_route(
        "GET",
        "/orgs/org1/packages/container/nix-cache/versions",
        MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(versions_json.as_bytes()),
        },
    );

    transport.add_route(
        "DELETE",
        "/orgs/org1/packages/container/nix-cache/versions/501",
        MockResponse {
            status: StatusCode::NO_CONTENT,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );

    let oci = OciClient::new(
        "ghcr.io",
        "org1/nix-cache",
        "gh-token",
        true,
        GhcrDriver,
        transport,
    );

    // delete_tag_strict should route to GitHubPackagesClient and succeed
    assert!(oci.delete_tag_strict("run-99").await.is_ok());

    // delete_blob_strict on GHCR must fail fast with OperationNotSupported
    let blob_err = oci.delete_blob_strict("sha256:blob").await.unwrap_err();
    assert!(matches!(blob_err, OciError::OperationNotSupported { .. }));
}
