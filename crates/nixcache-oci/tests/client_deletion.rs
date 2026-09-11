use nixcache_oci::{
    DeletionBatchResult, DockerHubDriver, MockRouterTransport, OciClient, OciError,
};

#[tokio::test]
async fn package_deletion_reports_unsupported_backends_explicitly() {
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
    let error = client.deletion().delete_entire_package().await.unwrap_err();
    assert!(matches!(
        error,
        OciError::OperationNotSupported {
            operation: "delete_package",
            ..
        }
    ));
}

#[tokio::test]
async fn empty_blob_deletion_batch_is_a_successful_noop() {
    let client = OciClient::with_transport(
        "example.com",
        "test/repo",
        "",
        true,
        MockRouterTransport::default(),
        Default::default(),
    )
    .unwrap();
    let result = client.deletion().batch_delete_blobs(&[], 4).await.unwrap();
    let DeletionBatchResult::Complete(summary) = result else {
        panic!("an empty batch must be complete");
    };
    assert_eq!(summary.requested_count, 0);
    assert_eq!(summary.deleted_count, 0);
    assert_eq!(summary.failed_count, 0);
}
