use crate::{
    error::TransportError,
    transport::{OciTransport, UploadChunkResponse},
};
use bytes::Bytes;
use crossbeam_queue::SegQueue;
use http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{IF_MATCH, IF_NONE_MATCH},
};
use scc::HashMap as SccHashMap;
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(not(target_arch = "wasm32"))]
use tokio::sync::Notify;

#[cfg(not(target_arch = "wasm32"))]
use futures_util::stream::BoxStream;

#[cfg(target_arch = "wasm32")]
use futures_util::stream::LocalBoxStream;

fn mock_sha256(bytes: &[u8]) -> String {
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

fn extract_digest_param(url: &str) -> Option<String> {
    url.split('?').nth(1).and_then(|query| {
        for param in query.split('&') {
            if let Some(digest) = param.strip_prefix("digest=") {
                return Some(digest.to_string());
            }
        }
        None
    })
}

#[derive(Clone, Default)]
pub struct MockResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// 可控的 token GET 闸门，供并发取消和代际交错测试使用。
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Default)]
pub struct MockTokenGate {
    entered: Arc<AtomicBool>,
    entered_notify: Arc<Notify>,
    released: Arc<AtomicBool>,
    release_notify: Arc<Notify>,
}

#[cfg(not(target_arch = "wasm32"))]
impl MockTokenGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn wait_until_entered(&self) {
        let notified = self.entered_notify.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        if !self.entered.load(Ordering::Acquire) {
            notified.await;
        }
    }

    pub fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.release_notify.notify_waiters();
    }

    async fn wait_until_released(&self) {
        let notified = self.release_notify.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        if !self.released.load(Ordering::Acquire) {
            notified.await;
        }
    }
}

#[derive(Clone)]
pub struct MockPutRequest {
    pub url: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Clone, Default)]
pub struct MockRouterTransport {
    pub call_count: Arc<AtomicUsize>,
    pub responses: Arc<SccHashMap<(String, String), MockResponse>>,
    pub posted_bodies: Arc<SegQueue<(String, Bytes)>>,
    pub put_requests: Arc<SegQueue<MockPutRequest>>,
    pub stored_blobs: Arc<SccHashMap<String, Bytes>>,
    pub stored_manifests: Arc<SccHashMap<String, (Bytes, String)>>,
    token_responses: Arc<SegQueue<MockResponse>>,
    #[cfg(not(target_arch = "wasm32"))]
    token_gate: Arc<std::sync::Mutex<Option<MockTokenGate>>>,
    panic_on_token: Arc<AtomicBool>,
}

impl MockRouterTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_route(&self, method: &str, url_suffix: &str, resp: MockResponse) {
        let _ = self
            .responses
            .upsert_sync((method.to_string(), url_suffix.to_string()), resp);
    }

    pub fn add_token_response(&self, response: MockResponse) {
        self.token_responses.push(response);
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_token_gate(&self, gate: MockTokenGate) {
        *self
            .token_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(gate);
    }

    pub fn set_panic_on_token(&self, panic: bool) {
        self.panic_on_token.store(panic, Ordering::Release);
    }
}

impl OciTransport for MockRouterTransport {
    #[cfg(not(target_arch = "wasm32"))]
    type BodyStream = BoxStream<'static, Result<Bytes, TransportError>>;

    #[cfg(target_arch = "wasm32")]
    type BodyStream = LocalBoxStream<'static, Result<Bytes, TransportError>>;

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        self.head_with_headers(url, headers).await.map(|(s, _)| s)
    }

    async fn head_with_headers(
        &self,
        url: &str,
        _headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);
        let mut found = None;
        self.responses.iter_sync(|(m, suffix), resp| {
            if m == "HEAD" && path.ends_with(suffix) {
                found = Some((resp.status, resp.headers.clone()));
                false
            } else {
                true
            }
        });
        if let Some(res) = found {
            return Ok(res);
        }

        if let Some(idx) = path.rfind("/blobs/") {
            let digest = &path[idx + 7..];
            if self.stored_blobs.contains_sync(digest) {
                return Ok((StatusCode::OK, HeaderMap::new()));
            }
        }

        if let Some(idx) = path.rfind("/manifests/") {
            let tag = &path[idx + 11..];
            if let Some(entry) = self.stored_manifests.get_sync(tag) {
                let (_bytes, digest) = entry.get();
                let mut headers = HeaderMap::new();
                if let Ok(val) = HeaderValue::from_str(digest) {
                    headers.insert("Docker-Content-Digest", val);
                }
                return Ok((StatusCode::OK, headers));
            }
        }

        Ok((StatusCode::NOT_FOUND, HeaderMap::new()))
    }

    async fn get(
        &self,
        url: &str,
        _headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);

        if path.ends_with("/token") {
            if self.panic_on_token.load(Ordering::Acquire) {
                panic!("mock token transport panic");
            }
            #[cfg(not(target_arch = "wasm32"))]
            let gate = self
                .token_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(gate) = gate {
                gate.entered.store(true, Ordering::Release);
                gate.entered_notify.notify_waiters();
                gate.wait_until_released().await;
            }
            if let Some(response) = self.token_responses.pop() {
                return Ok((response.status, response.headers, response.body));
            }
        }

        let mut found = None;
        self.responses.iter_sync(|(m, suffix), resp| {
            if m == "GET" && path.ends_with(suffix) {
                found = Some((resp.status, resp.headers.clone(), resp.body.clone()));
                false
            } else {
                true
            }
        });
        if let Some(res) = found {
            return Ok(res);
        }

        if let Some(idx) = path.rfind("/blobs/") {
            let digest = &path[idx + 7..];
            if let Some(entry) = self.stored_blobs.get_sync(digest) {
                return Ok((StatusCode::OK, HeaderMap::new(), entry.get().clone()));
            }
        }

        if let Some(idx) = path.rfind("/manifests/") {
            let tag = &path[idx + 11..];
            if let Some(entry) = self.stored_manifests.get_sync(tag) {
                let (bytes, digest) = entry.get();
                let mut headers = HeaderMap::new();
                if let Ok(val) = HeaderValue::from_str(digest) {
                    headers.insert("Docker-Content-Digest", val);
                }
                return Ok((StatusCode::OK, headers, bytes.clone()));
            }
        }

        if path.ends_with("/tags/list") {
            let mut tags = Vec::new();
            self.stored_manifests.iter_sync(|tag, _| {
                if !tag.starts_with("sha256:") {
                    tags.push(tag.clone());
                }
                true
            });
            tags.sort();
            tags.dedup();
            let json = serde_json::json!({
                "name": "nix-cache",
                "tags": tags,
            });
            let bytes = Bytes::from(serde_json::to_vec(&json).unwrap_or_default());
            return Ok((StatusCode::OK, HeaderMap::new(), bytes));
        }

        Ok((StatusCode::NOT_FOUND, HeaderMap::new(), Bytes::new()))
    }

    async fn stream(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError> {
        let (status, headers, bytes) = self.get(url, headers).await?;
        let stream: Self::BodyStream =
            Box::pin(futures_util::stream::once(async move { Ok(bytes) }));
        Ok((status, headers, stream))
    }

    async fn post(
        &self,
        url: &str,
        _headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);
        let mut found = None;
        self.responses.iter_sync(|(m, suffix), resp| {
            if m == "POST" && path.ends_with(suffix) {
                found = Some((resp.status, resp.headers.clone()));
                false
            } else {
                true
            }
        });
        if let Some((status, headers)) = found {
            Ok((status, headers))
        } else {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Location",
                HeaderValue::from_static("/v2/test/repo/nix-cache/blobs/uploads/session-mock"),
            );
            Ok((StatusCode::ACCEPTED, headers))
        }
    }

    async fn post_bytes(
        &self,
        url: &str,
        _headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.posted_bodies.push((url.to_string(), body.clone()));

        if let Some(digest) = extract_digest_param(url) {
            let _ = self.stored_blobs.upsert_sync(digest, body.clone());
        }

        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);
        let mut found = None;
        self.responses.iter_sync(|(m, suffix), resp| {
            if m == "POST" && path.ends_with(suffix) {
                found = Some((resp.status, resp.headers.clone()));
                false
            } else {
                true
            }
        });
        Ok(found.unwrap_or((StatusCode::CREATED, HeaderMap::new())))
    }

    async fn post_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        _stream: Self::BodyStream,
        _content_len: u64,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.post(url, headers).await
    }

    async fn patch_chunk(
        &self,
        url: &str,
        _headers: HeaderMap,
        _chunk: Bytes,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);
        let mut found = None;
        self.responses.iter_sync(|(m, suffix), resp| {
            if m == "PATCH" && path.ends_with(suffix) {
                found = Some((resp.status, resp.headers.clone()));
                false
            } else {
                true
            }
        });
        if let Some((status, headers)) = found {
            Ok(UploadChunkResponse {
                status,
                headers,
                location: None,
                range: Some(byte_range),
            })
        } else {
            Ok(UploadChunkResponse {
                status: StatusCode::ACCEPTED,
                headers: HeaderMap::new(),
                location: None,
                range: Some(byte_range),
            })
        }
    }

    async fn patch_chunk_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        _stream: Self::BodyStream,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError> {
        self.patch_chunk(url, headers, Bytes::new(), byte_range)
            .await
    }

    async fn probe_upload_session(
        &self,
        _url: &str,
        _headers: HeaderMap,
    ) -> Result<Option<u64>, TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    async fn put_chunk_finish(
        &self,
        _url: &str,
        _headers: HeaderMap,
        _last_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<StatusCode, TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(StatusCode::CREATED)
    }

    async fn put_bytes(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<StatusCode, TransportError> {
        self.put_bytes_with_headers(url, headers, body)
            .await
            .map(|(status, _)| status)
    }

    async fn put_bytes_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.put_requests.push(MockPutRequest {
            url: url.to_string(),
            headers: headers.clone(),
            body: body.clone(),
        });

        if let Some(digest) = extract_digest_param(url) {
            let _ = self.stored_blobs.upsert_sync(digest, body.clone());
        }

        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);

        if let Some(idx) = path.rfind("/blobs/") {
            let digest = &path[idx + 7..];
            if digest.starts_with("sha256:") {
                let _ = self
                    .stored_blobs
                    .upsert_sync(digest.to_string(), body.clone());
            }
        }

        if let Some(idx) = path.rfind("/manifests/") {
            let tag = &path[idx + 11..];

            let current_digest = self
                .stored_manifests
                .get_sync(tag)
                .map(|entry| entry.get().1.clone());
            if let Some(expected) = headers.get(IF_MATCH).and_then(|value| value.to_str().ok())
                && current_digest.as_deref() != Some(expected)
            {
                return Ok((StatusCode::PRECONDITION_FAILED, HeaderMap::new()));
            }
            if headers
                .get(IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok())
                == Some("*")
                && current_digest.is_some()
            {
                return Ok((StatusCode::PRECONDITION_FAILED, HeaderMap::new()));
            }

            let digest = mock_sha256(&body);
            let _ = self
                .stored_manifests
                .upsert_sync(tag.to_string(), (body.clone(), digest.clone()));
            let _ = self
                .stored_manifests
                .upsert_sync(digest, (body.clone(), mock_sha256(&body)));
        }

        let mut found = None;
        self.responses.iter_sync(|(m, suffix), resp| {
            if m == "PUT" && path.ends_with(suffix) {
                found = Some((resp.status, resp.headers.clone()));
                false
            } else {
                true
            }
        });
        Ok(found.unwrap_or((StatusCode::CREATED, HeaderMap::new())))
    }

    async fn put_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        _stream: Self::BodyStream,
        _content_len: u64,
    ) -> Result<StatusCode, TransportError> {
        self.put_bytes(url, headers, Bytes::new()).await
    }

    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        self.delete_with_headers(url, headers)
            .await
            .map(|(status, _)| status)
    }

    async fn delete_with_headers(
        &self,
        url: &str,
        _headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);

        let mut found = None;
        self.responses.iter_sync(|(m, suffix), resp| {
            if m == "DELETE" && path.ends_with(suffix) {
                found = Some((resp.status, resp.headers.clone()));
                false
            } else {
                true
            }
        });
        let (status, response_headers) = found.unwrap_or((StatusCode::ACCEPTED, HeaderMap::new()));
        if status.is_success() || status == StatusCode::NOT_FOUND {
            if let Some(idx) = path.rfind("/blobs/") {
                let digest = &path[idx + 7..];
                let _ = self.stored_blobs.remove_sync(&digest.to_string());
            }

            if let Some(idx) = path.rfind("/manifests/") {
                let reference = &path[idx + 11..];
                if reference.starts_with("sha256:") {
                    let mut tags = Vec::new();
                    self.stored_manifests.iter_sync(|tag, entry| {
                        if entry.1 == reference {
                            tags.push(tag.clone());
                        }
                        true
                    });
                    for tag in tags {
                        let _ = self.stored_manifests.remove_sync(&tag);
                    }
                } else {
                    let _ = self.stored_manifests.remove_sync(&reference.to_string());
                }
            }
        }
        Ok((status, response_headers))
    }

    async fn sleep(&self, _duration: Duration) {}
}
