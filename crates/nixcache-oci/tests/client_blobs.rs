use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode};
use nixcache_oci::{
    ContentDigest, DockerHubDriver, GenericOciDriver, GhcrDriver, HashingStream, MockPatchOutcome,
    MockPatchRequest, MockResponse, MockRouterTransport, OciClient, OciError, OciReadLimits,
    StreamHashState, TransportError, UploadConfig, parse_range_header,
};
use sha2::{Digest, Sha256};
use std::time::Duration;

#[test]
fn parse_range_header_accepts_registry_formats() {
    assert_eq!(parse_range_header("0-100"), Some((0, 100)));
    assert_eq!(parse_range_header("bytes=0-100"), Some((0, 100)));
    assert_eq!(parse_range_header("bytes 0-100/500"), Some((0, 100)));
    assert_eq!(parse_range_header("invalid"), None);
}

#[tokio::test]
async fn monolithic_post_upload_is_available() {
    let client = OciClient::new(
        "docker.io",
        "test/repo",
        "token123",
        true,
        DockerHubDriver,
        MockRouterTransport::default(),
        Default::default(),
    )
    .unwrap();
    let digest = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert_eq!(
        client
            .blobs()
            .push_bytes_with_digest(digest, Bytes::from_static(b"fast monolithic payload"))
            .await
            .unwrap(),
        digest
    );
}

#[tokio::test]
async fn ghcr_resumable_upload_uses_fixed_two_step_strategy() {
    let transport = MockRouterTransport::default();
    transport.add_route(
        "PATCH",
        "/uploads/",
        MockResponse {
            status: StatusCode::RANGE_NOT_SATISFIABLE,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        },
    );
    let client = OciClient::new(
        "ghcr.io",
        "test/repo",
        "token123",
        true,
        GhcrDriver,
        transport,
        Default::default(),
    )
    .unwrap();
    let data = Bytes::from_static(b"streamed nar xz chunk data for test");
    let stream = Box::pin(futures_util::stream::iter(vec![
        Ok::<Bytes, TransportError>(data.clone()),
    ]));
    let (digest, size) = client
        .blobs()
        .push_resumable(stream, &UploadConfig::default())
        .await
        .unwrap();
    assert_eq!(size, data.len() as u64);
    assert!(digest.starts_with("sha256:"));
}

#[tokio::test]
async fn stream_hash_state_is_shared_without_losing_progress() {
    let state = StreamHashState::new();
    let input = futures_util::stream::iter(vec![
        Ok::<Bytes, TransportError>(Bytes::from_static(b"chunk1 ")),
        Ok::<Bytes, TransportError>(Bytes::from_static(b"chunk2")),
    ]);
    let (mut stream, stream_state) = HashingStream::new(input);
    let state_clone = stream_state.clone();
    while let Some(item) = stream.next().await {
        assert!(item.is_ok());
    }
    assert_eq!(stream_state.bytes_streamed(), 13);
    assert_eq!(state_clone.bytes_streamed(), 13);
    assert_eq!(state.digest(), None);
    assert!(stream_state.digest().is_some());
}

fn chunked_config(max_retry_attempts: usize) -> UploadConfig {
    UploadConfig {
        chunk_size_bytes: 1024 * 1024,
        chunk_threshold_bytes: 1024 * 1024,
        max_retry_attempts,
    }
}

fn chunked_client(transport: MockRouterTransport) -> OciClient<MockRouterTransport> {
    OciClient::new(
        "generic.registry",
        "test/repo",
        "token",
        true,
        GenericOciDriver,
        transport,
        Default::default(),
    )
    .unwrap()
}

fn drain_patch_requests(transport: &MockRouterTransport) -> Vec<MockPatchRequest> {
    let mut requests = Vec::new();
    while let Some(request) = transport.patch_requests.pop() {
        requests.push(request);
    }
    requests
}

fn response(status: StatusCode) -> MockResponse {
    MockResponse {
        status,
        headers: HeaderMap::new(),
        body: Bytes::new(),
    }
}

fn range_response(status: StatusCode, range: &str) -> MockResponse {
    let mut headers = HeaderMap::new();
    headers.insert("Range", HeaderValue::from_str(range).unwrap());
    MockResponse {
        status,
        headers,
        body: Bytes::new(),
    }
}

#[tokio::test]
async fn buffered_and_streamed_blob_reads_reject_digest_tampering() {
    let body = Bytes::from_static(b"integrity body");
    let body_digest = ContentDigest::from_bytes(&body).to_string();
    let wrong_digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let mut headers = HeaderMap::new();
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_static(wrong_digest),
    );
    let transport = MockRouterTransport::default();
    transport.add_route(
        "GET",
        &format!("/blobs/{body_digest}"),
        MockResponse {
            status: StatusCode::OK,
            headers,
            body: body.clone(),
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
    assert!(matches!(
        client.blobs().get(&body_digest).await,
        Err(OciError::HeaderDigestMismatch { .. })
    ));

    let stream_transport = MockRouterTransport::default();
    stream_transport.add_route(
        "GET",
        &format!("/blobs/{wrong_digest}"),
        MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body,
        },
    );
    let stream_client = OciClient::with_transport(
        "example.com",
        "test/repo",
        "",
        false,
        stream_transport,
        Default::default(),
    )
    .unwrap();
    let blob_stream = stream_client.blobs().stream(wrong_digest).await.unwrap();
    let mut body_stream = blob_stream.stream;
    assert!(body_stream.next().await.unwrap().is_ok());
    assert!(matches!(
        body_stream.next().await,
        Some(Err(OciError::DigestMismatch { .. }))
    ));
}

#[tokio::test]
async fn buffered_blob_reads_use_configured_limits() {
    let limits = OciReadLimits::default()
        .with_max_buffered_blob_bytes(3)
        .unwrap();
    let transport = MockRouterTransport::default();
    transport.add_route(
        "GET",
        "/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000",
        MockResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"four"),
        },
    );
    let client =
        OciClient::with_transport("example.com", "test/repo", "", false, transport, limits)
            .unwrap();
    assert!(matches!(
        client
            .blobs()
            .get("sha256:0000000000000000000000000000000000000000000000000000000000000000")
            .await,
        Err(OciError::SizeLimitExceeded { limit: 3, .. })
    ));
}

fn stream_for(
    data: Bytes,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, TransportError>> {
    Box::pin(futures_util::stream::iter(vec![
        Ok::<Bytes, TransportError>(data),
    ]))
}

#[tokio::test]
async fn chunk_retry_replays_body_and_consumes_one_retry() {
    let transport = MockRouterTransport::default();
    transport.add_patch_outcome(MockPatchOutcome::Response(response(
        StatusCode::SERVICE_UNAVAILABLE,
    )));
    transport.add_patch_outcome(MockPatchOutcome::Response(response(StatusCode::ACCEPTED)));
    transport.add_route(
        "GET",
        "/uploads/session-mock",
        response(StatusCode::NO_CONTENT),
    );
    let client = chunked_client(transport.clone());
    let data = Bytes::from(vec![0x5a; 1024 * 1024]);
    let result = client
        .blobs()
        .push_resumable(stream_for(data.clone()), &chunked_config(1))
        .await
        .unwrap();
    let requests = drain_patch_requests(&transport);
    assert_eq!(result.1, data.len() as u64);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
    assert_eq!(requests[0].byte_range, (0, 1024 * 1024 - 1));
    assert_eq!(
        transport.sleep_durations.pop(),
        Some(Duration::from_millis(100))
    );
    assert!(transport.delete_requests.pop().is_none());
}

#[tokio::test]
async fn transport_retries_use_exponential_backoff() {
    let transport = MockRouterTransport::default();
    transport.add_patch_outcome(MockPatchOutcome::Timeout);
    transport.add_patch_outcome(MockPatchOutcome::ConnectionFailed);
    transport.add_patch_outcome(MockPatchOutcome::Response(response(StatusCode::NO_CONTENT)));
    transport.add_route(
        "GET",
        "/uploads/session-mock",
        response(StatusCode::NO_CONTENT),
    );
    chunked_client(transport.clone())
        .blobs()
        .push_resumable(
            stream_for(Bytes::from(vec![0x11; 1024 * 1024])),
            &chunked_config(2),
        )
        .await
        .unwrap();
    assert_eq!(drain_patch_requests(&transport).len(), 3);
    assert_eq!(
        transport.sleep_durations.pop(),
        Some(Duration::from_millis(100))
    );
    assert_eq!(
        transport.sleep_durations.pop(),
        Some(Duration::from_millis(200))
    );
}

#[tokio::test]
async fn zero_retry_budget_aborts_without_sleep() {
    let transport = MockRouterTransport::default();
    transport.add_patch_outcome(MockPatchOutcome::Response(response(
        StatusCode::SERVICE_UNAVAILABLE,
    )));
    let error = chunked_client(transport.clone())
        .blobs()
        .push_resumable(
            stream_for(Bytes::from(vec![0x22; 1024 * 1024])),
            &chunked_config(0),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        OciError::ResumableUploadFailed { attempts: 1, .. }
    ));
    assert_eq!(drain_patch_requests(&transport).len(), 1);
    assert!(transport.sleep_durations.pop().is_none());
    assert!(transport.delete_requests.pop().is_some());
}

#[tokio::test]
async fn retry_exhaustion_reports_attempts_and_aborts() {
    let transport = MockRouterTransport::default();
    transport.add_patch_outcome(MockPatchOutcome::Response(response(
        StatusCode::SERVICE_UNAVAILABLE,
    )));
    transport.add_patch_outcome(MockPatchOutcome::Response(response(
        StatusCode::SERVICE_UNAVAILABLE,
    )));
    transport.add_route(
        "GET",
        "/uploads/session-mock",
        response(StatusCode::NO_CONTENT),
    );
    let error = chunked_client(transport.clone())
        .blobs()
        .push_resumable(
            stream_for(Bytes::from(vec![0x33; 1024 * 1024])),
            &chunked_config(1),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        OciError::ResumableUploadFailed { attempts: 2, .. }
    ));
    assert_eq!(drain_patch_requests(&transport).len(), 2);
    assert!(transport.delete_requests.pop().is_some());
}

#[tokio::test]
async fn probe_can_skip_completed_chunk() {
    let transport = MockRouterTransport::default();
    transport.add_patch_outcome(MockPatchOutcome::Response(response(
        StatusCode::SERVICE_UNAVAILABLE,
    )));
    transport.add_route(
        "GET",
        "/uploads/session-mock",
        range_response(StatusCode::NO_CONTENT, "0-1048575"),
    );
    chunked_client(transport.clone())
        .blobs()
        .push_resumable(
            stream_for(Bytes::from(vec![0x44; 1024 * 1024])),
            &chunked_config(1),
        )
        .await
        .unwrap();
    assert_eq!(drain_patch_requests(&transport).len(), 1);
    assert!(transport.sleep_durations.pop().is_none());
}

#[tokio::test]
async fn probe_resends_only_uncommitted_suffix() {
    let transport = MockRouterTransport::default();
    transport.add_patch_outcome(MockPatchOutcome::Timeout);
    transport.add_patch_outcome(MockPatchOutcome::Response(response(StatusCode::NO_CONTENT)));
    transport.add_route(
        "GET",
        "/uploads/session-mock",
        range_response(StatusCode::NO_CONTENT, "0-0"),
    );
    let data = Bytes::from(vec![0x55; 1024 * 1024]);
    let (digest, size) = chunked_client(transport.clone())
        .blobs()
        .push_resumable(stream_for(data.clone()), &chunked_config(1))
        .await
        .unwrap();
    let requests = drain_patch_requests(&transport);
    assert_eq!(size, data.len() as u64);
    assert_eq!(
        digest,
        format!(
            "sha256:{}",
            Sha256::digest(&data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    );
    assert_eq!(requests[1].body, data.slice(1..));
    assert_eq!(requests[1].byte_range, (1, data.len() as u64 - 1));
}

#[tokio::test]
async fn retry_budget_resets_for_each_chunk() {
    let transport = MockRouterTransport::default();
    for _ in 0..2 {
        transport.add_patch_outcome(MockPatchOutcome::Response(response(
            StatusCode::SERVICE_UNAVAILABLE,
        )));
        transport.add_patch_outcome(MockPatchOutcome::Response(response(StatusCode::NO_CONTENT)));
    }
    transport.add_route(
        "GET",
        "/uploads/session-mock",
        response(StatusCode::NO_CONTENT),
    );
    chunked_client(transport.clone())
        .blobs()
        .push_resumable(
            stream_for(Bytes::from(vec![0xbb; 2 * 1024 * 1024])),
            &chunked_config(1),
        )
        .await
        .unwrap();
    assert_eq!(drain_patch_requests(&transport).len(), 4);
    assert_eq!(
        transport.sleep_durations.pop(),
        Some(Duration::from_millis(100))
    );
    assert_eq!(
        transport.sleep_durations.pop(),
        Some(Duration::from_millis(100))
    );
}

#[tokio::test]
async fn range_416_is_probed_once_without_blind_retry() {
    let transport = MockRouterTransport::default();
    transport.add_patch_outcome(MockPatchOutcome::Response(response(
        StatusCode::RANGE_NOT_SATISFIABLE,
    )));
    let error = chunked_client(transport.clone())
        .blobs()
        .push_resumable(
            stream_for(Bytes::from(vec![0xcc; 1024 * 1024])),
            &chunked_config(5),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        OciError::BlobUploadFailed(StatusCode::RANGE_NOT_SATISFIABLE)
    ));
    assert_eq!(drain_patch_requests(&transport).len(), 1);
    assert!(transport.delete_requests.pop().is_some());
}

#[tokio::test]
async fn invalid_range_and_finish_or_stream_failures_abort() {
    let transport = MockRouterTransport::default();
    transport.add_patch_outcome(MockPatchOutcome::Response(range_response(
        StatusCode::NO_CONTENT,
        "0-1048576",
    )));
    let error = chunked_client(transport.clone())
        .blobs()
        .push_resumable(
            stream_for(Bytes::from(vec![0x77; 1024 * 1024])),
            &chunked_config(1),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, OciError::UploadRangeInvalid { .. }));
    assert!(transport.delete_requests.pop().is_some());

    let finish_transport = MockRouterTransport::default();
    finish_transport.add_route(
        "PUT",
        "/uploads/session-mock",
        response(StatusCode::INTERNAL_SERVER_ERROR),
    );
    let finish_error = chunked_client(finish_transport.clone())
        .blobs()
        .push_resumable(
            stream_for(Bytes::from(vec![0x88; 1024 * 1024])),
            &chunked_config(1),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        finish_error,
        OciError::BlobUploadFailed(StatusCode::INTERNAL_SERVER_ERROR)
    ));
    assert!(finish_transport.delete_requests.pop().is_some());
}

#[tokio::test]
async fn location_updates_are_used_for_retry_and_abort() {
    let transport = MockRouterTransport::default();
    let mut headers = HeaderMap::new();
    headers.insert(
        "Location",
        HeaderValue::from_static("/v2/test/repo/nix-cache/blobs/uploads/session-new"),
    );
    transport.add_patch_outcome(MockPatchOutcome::Response(MockResponse {
        status: StatusCode::SERVICE_UNAVAILABLE,
        headers,
        body: Bytes::new(),
    }));
    transport.add_patch_outcome(MockPatchOutcome::Response(response(
        StatusCode::SERVICE_UNAVAILABLE,
    )));
    transport.add_route(
        "GET",
        "/uploads/session-new",
        response(StatusCode::NO_CONTENT),
    );
    let error = chunked_client(transport.clone())
        .blobs()
        .push_resumable(
            stream_for(Bytes::from(vec![0xaa; 1024 * 1024])),
            &chunked_config(1),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        OciError::ResumableUploadFailed { attempts: 2, .. }
    ));
    let requests = drain_patch_requests(&transport);
    assert_eq!(
        requests[1].url,
        "https://generic.registry/v2/test/repo/nix-cache/blobs/uploads/session-new"
    );
    assert_eq!(
        transport.delete_requests.pop().as_deref(),
        Some(requests[1].url.as_str())
    );
}
