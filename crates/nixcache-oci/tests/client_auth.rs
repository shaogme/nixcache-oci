use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use nixcache_oci::{
    AwsEcrDriver, BearerChallenge, BlobUploadStrategy, DockerHubDriver, GenericOciDriver,
    GhcrDriver, MockResponse, MockRouterTransport, OciClient, OciError, RegistryDeletionStrategy,
    RegistryKind,
};

#[test]
fn driver_capabilities_and_canonicalization() {
    let ghcr = GhcrDriver;
    assert_eq!(ghcr.kind(), RegistryKind::Ghcr);
    assert!(!ghcr.capabilities().supports_chunked_patch);
    assert_eq!(
        ghcr.capabilities().fixed_upload_strategy,
        BlobUploadStrategy::FixedTwoStepPut
    );
    assert_eq!(
        ghcr.capabilities().deletion_strategy,
        RegistryDeletionStrategy::GitHubPackagesRestApi
    );
    assert_eq!(ghcr.canonicalize_endpoint("  GHCR.IO "), "ghcr.io");
    assert_eq!(ghcr.canonicalize_repository("/Owner/Repo/"), "owner/repo");

    let docker = DockerHubDriver;
    assert_eq!(docker.kind(), RegistryKind::DockerHub);
    assert!(docker.capabilities().supports_chunked_patch);
    assert_eq!(
        docker.capabilities().fixed_upload_strategy,
        BlobUploadStrategy::PreferMonolithicPost
    );
    assert_eq!(
        docker.canonicalize_endpoint("docker.io"),
        "registry-1.docker.io"
    );
    assert_eq!(docker.canonicalize_repository("ubuntu"), "library/ubuntu");

    let generic = GenericOciDriver;
    assert_eq!(generic.kind(), RegistryKind::GenericOci);
    assert_eq!(
        generic.capabilities().deletion_strategy,
        RegistryDeletionStrategy::StandardOciDelete
    );
    assert!(generic.capabilities().supports_blob_physical_deletion);

    let aws = AwsEcrDriver;
    assert_eq!(aws.kind(), RegistryKind::AwsEcr);
    assert!(!aws.capabilities().supports_chunked_patch);
}

#[tokio::test]
async fn token_exchange_failure_is_visible() {
    let transport = MockRouterTransport::default();
    transport.add_route(
        "GET",
        "/token",
        MockResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    let client = OciClient::with_transport(
        "example.com",
        "test/repo",
        "fallback-token",
        true,
        transport,
    );
    let challenge = BearerChallenge::new(
        "https://auth.example.test/token",
        Some("example.com".to_string()),
        Some("repository:test/repo/nix-cache:pull,push".to_string()),
    )
    .unwrap();
    assert!(client.get_token(&challenge).await.is_err());
}

#[test]
fn oci_error_display_includes_status_and_target() {
    assert!(format!("{}", OciError::BlobUploadFailed(StatusCode::BAD_REQUEST)).contains("400"));
    assert!(
        format!(
            "{}",
            OciError::BlobDownloadFailed(StatusCode::INTERNAL_SERVER_ERROR)
        )
        .contains("500")
    );
    assert_eq!(
        format!(
            "{}",
            OciError::BlobNotFound {
                digest: "sha256:123".to_string()
            }
        ),
        "Target blob 'sha256:123' not found on registry"
    );
}
