use crate::{
    error::TransportError,
    transport::{BoxBodyStream, OciTransport, UploadChunkResponse},
};
use async_trait::async_trait;
use bytes::Bytes;
use crossbeam_queue::SegQueue;
use http::{HeaderMap, StatusCode};
use scc::HashMap as SccHashMap;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

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

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl OciTransport for MockRouterTransport {
    type BodyStream = BoxBodyStream;

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
        Ok(found.unwrap_or(StatusCode::NOT_FOUND))
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
        Ok(found.unwrap_or((StatusCode::NOT_FOUND, HeaderMap::new(), Bytes::new())))
    }

    async fn stream(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError> {
        let (status, headers, bytes) = self.get(url, headers).await?;
        let stream: BoxBodyStream = Box::pin(futures_util::stream::once(async move { Ok(bytes) }));
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
        Ok(found.unwrap_or((StatusCode::ACCEPTED, HeaderMap::new())))
    }

    async fn post_bytes(
        &self,
        url: &str,
        _headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.posted_bodies.push((url.to_string(), body.clone()));
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
        _url: &str,
        _headers: HeaderMap,
        _chunk: Bytes,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(UploadChunkResponse {
            status: StatusCode::ACCEPTED,
            headers: HeaderMap::new(),
            location: None,
            range: Some(byte_range),
        })
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
        _body: Bytes,
    ) -> Result<StatusCode, TransportError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let path = url.split_once('?').map(|(p, _)| p).unwrap_or(url);
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
