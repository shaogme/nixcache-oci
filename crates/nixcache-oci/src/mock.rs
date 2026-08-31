use crate::{
    error::TransportError,
    transport::{OciTransport, UploadChunkResponse},
};
use bytes::Bytes;
use crossbeam_queue::SegQueue;
use http::{HeaderMap, HeaderValue, StatusCode};
use scc::HashMap as SccHashMap;
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

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

#[derive(Clone, Default)]
pub struct MockRouterTransport {
    pub call_count: Arc<AtomicUsize>,
    pub responses: Arc<SccHashMap<(String, String), MockResponse>>,
    pub posted_bodies: Arc<SegQueue<(String, Bytes)>>,
    pub stored_blobs: Arc<SccHashMap<String, Bytes>>,
    pub stored_manifests: Arc<SccHashMap<String, (Bytes, String)>>,
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
}

impl OciTransport for MockRouterTransport {
    #[cfg(not(target_arch = "wasm32"))]
    type BodyStream = BoxStream<'static, Result<Bytes, TransportError>>;

    #[cfg(target_arch = "wasm32")]
    type BodyStream = LocalBoxStream<'static, Result<Bytes, TransportError>>;

    async fn head(&self, url: &str, _headers: HeaderMap) -> Result<StatusCode, TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);
        let mut found = None;
        self.responses.iter_sync(|(m, suffix), resp| {
            if m == "HEAD" && path.ends_with(suffix) {
                found = Some(resp.status);
                false
            } else {
                true
            }
        });
        if let Some(st) = found {
            return Ok(st);
        }

        if let Some(idx) = path.rfind("/blobs/") {
            let digest = &path[idx + 7..];
            if self.stored_blobs.contains_sync(digest) {
                return Ok(StatusCode::OK);
            }
        }

        if let Some(idx) = path.rfind("/manifests/") {
            let tag = &path[idx + 11..];
            if self.stored_manifests.contains_sync(tag) {
                return Ok(StatusCode::OK);
            }
        }

        Ok(StatusCode::NOT_FOUND)
    }

    async fn get(
        &self,
        url: &str,
        _headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);
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
        _headers: HeaderMap,
        body: Bytes,
    ) -> Result<StatusCode, TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);

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
                found = Some(resp.status);
                false
            } else {
                true
            }
        });
        Ok(found.unwrap_or(StatusCode::CREATED))
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

    async fn delete(&self, url: &str, _headers: HeaderMap) -> Result<StatusCode, TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);

        if let Some(idx) = path.rfind("/blobs/") {
            let digest = &path[idx + 7..];
            let _ = self.stored_blobs.remove_sync(&digest.to_string());
        }

        if let Some(idx) = path.rfind("/manifests/") {
            let tag = &path[idx + 11..];
            let _ = self.stored_manifests.remove_sync(&tag.to_string());
        }

        let mut found = None;
        self.responses.iter_sync(|(m, suffix), resp| {
            if m == "DELETE" && path.ends_with(suffix) {
                found = Some(resp.status);
                false
            } else {
                true
            }
        });
        Ok(found.unwrap_or(StatusCode::ACCEPTED))
    }

    async fn sleep(&self, _duration: Duration) {}
}
