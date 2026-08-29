use crate::error::TransportError;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, ready, stream::BoxStream};
use http::{HeaderMap, StatusCode};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

#[cfg(feature = "reqwest")]
use futures_util::StreamExt;
#[cfg(feature = "reqwest")]
use reqwest::{
    Client,
    header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, LOCATION, RANGE},
};

pub type BoxBodyStream = BoxStream<'static, Result<Bytes, TransportError>>;

pub struct OciBlobStream<S = BoxBodyStream> {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub stream: S,
}

impl<S> fmt::Debug for OciBlobStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OciBlobStream")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl<S> OciBlobStream<S> {
    pub fn new(status: StatusCode, headers: HeaderMap, stream: S) -> Self {
        Self {
            status,
            headers,
            stream,
        }
    }

    pub fn content_length(&self) -> Option<u64> {
        self.headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    }
}

#[derive(Debug, Clone)]
pub struct UploadChunkResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub location: Option<String>,
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone)]
pub struct UploadSessionInfo {
    pub location: String,
    pub last_range_end: Option<u64>,
}

/// 流式哈希状态共享容器
#[derive(Clone, Default, Debug)]
pub struct StreamHashState {
    inner: Arc<Mutex<InnerHashState>>,
}

#[derive(Default, Debug)]
struct InnerHashState {
    hasher: Sha256,
    bytes_streamed: u64,
    finalized_digest: Option<String>,
}

impl StreamHashState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bytes_streamed(&self) -> u64 {
        self.inner.lock().unwrap().bytes_streamed
    }

    pub fn digest(&self) -> Option<String> {
        self.inner.lock().unwrap().finalized_digest.clone()
    }

    pub fn force_finalize(&self) -> String {
        let mut guard = self.inner.lock().unwrap();
        if let Some(ref d) = guard.finalized_digest {
            return d.clone();
        }
        let hash = guard.hasher.clone().finalize();
        let digest_str = format!(
            "sha256:{}",
            hash.iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        );
        guard.finalized_digest = Some(digest_str.clone());
        digest_str
    }
}

/// 边流式传输边计算 SHA256 与字节计数的 Stream 包装器
pub struct HashingStream<S> {
    inner: S,
    state: StreamHashState,
}

impl<S> HashingStream<S> {
    pub fn new(inner: S) -> (Self, StreamHashState) {
        let state = StreamHashState::new();
        (
            Self {
                inner,
                state: state.clone(),
            },
            state,
        )
    }

    pub fn state(&self) -> &StreamHashState {
        &self.state
    }
}

impl<S, E> Stream for HashingStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let (inner, state) = unsafe {
            let this = self.get_unchecked_mut();
            (Pin::new_unchecked(&mut this.inner), &this.state)
        };
        match ready!(inner.poll_next(cx)) {
            Some(Ok(bytes)) => {
                let mut guard = state.inner.lock().unwrap();
                guard.hasher.update(&bytes);
                guard.bytes_streamed += bytes.len() as u64;
                Poll::Ready(Some(Ok(bytes)))
            }
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            None => {
                let mut guard = state.inner.lock().unwrap();
                if guard.finalized_digest.is_none() {
                    let hash = guard.hasher.clone().finalize();
                    let hex = hash
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<String>();
                    guard.finalized_digest = Some(format!("sha256:{}", hex));
                }
                Poll::Ready(None)
            }
        }
    }
}

/// 解析 Range 响应头，返回 (start, end)
pub fn parse_range_header(header_val: &str) -> Option<(u64, u64)> {
    let clean = header_val.trim();
    let val = clean
        .strip_prefix("bytes=")
        .or_else(|| clean.strip_prefix("bytes "))
        .unwrap_or(clean);

    let range_part = if let Some((r, _)) = val.split_once('/') {
        r.trim()
    } else {
        val.trim()
    };

    let (start_str, end_str) = range_part.split_once('-')?;
    let start = start_str.trim().parse::<u64>().ok()?;
    let end = end_str.trim().parse::<u64>().ok()?;
    Some((start, end))
}

/// 平台无关的 OCI 传输与环境能力特征
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait OciTransport: 'static {
    #[cfg(not(target_arch = "wasm32"))]
    type BodyStream: Stream<Item = Result<Bytes, TransportError>> + Send + 'static;

    #[cfg(target_arch = "wasm32")]
    type BodyStream: Stream<Item = Result<Bytes, TransportError>> + 'static;

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError>;

    async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError>;

    async fn stream(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError>;

    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError>;

    /// 1-RTT Monolithic POST 上传 (Bytes)
    async fn post_bytes(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError>;

    /// 1-RTT Monolithic POST 上传 (Stream)
    async fn post_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<(StatusCode, HeaderMap), TransportError>;

    /// 分块上传 PATCH (发送单个分块)
    async fn patch_chunk(
        &self,
        url: &str,
        headers: HeaderMap,
        chunk: Bytes,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError>;

    /// 分块流式 PATCH (用于零拷贝大分块推流)
    async fn patch_chunk_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError>;

    /// 探测当前断点会话状态 (GET session url 获取已接收的 Range 终止偏移量)
    async fn probe_upload_session(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<Option<u64>, TransportError>;

    /// 完成分块上传 (PUT finish，可带尾部数据或为空 Body)
    async fn put_chunk_finish(
        &self,
        url: &str,
        headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<StatusCode, TransportError>;

    async fn put_bytes(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<StatusCode, TransportError>;

    async fn put_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<StatusCode, TransportError>;

    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError>;

    async fn sleep(&self, duration: Duration);
}

#[cfg(feature = "reqwest")]
#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: Client,
}

#[cfg(feature = "reqwest")]
impl Default for ReqwestTransport {
    fn default() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client }
    }
}

#[cfg(feature = "reqwest")]
impl ReqwestTransport {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }
}

#[cfg(feature = "reqwest")]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl OciTransport for ReqwestTransport {
    type BodyStream = BoxBodyStream;

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        let resp = self.client.head(url).headers(headers).send().await?;
        Ok(resp.status())
    }

    async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError> {
        let resp = self.client.get(url).headers(headers).send().await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await?;
        Ok((status, headers, bytes))
    }

    async fn stream(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError> {
        let resp = self.client.get(url).headers(headers).send().await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let stream = Box::pin(
            resp.bytes_stream()
                .map(|res| res.map_err(TransportError::Reqwest)),
        );
        Ok((status, headers, stream))
    }

    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let resp = self.client.post(url).headers(headers).send().await?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn post_bytes(
        &self,
        url: &str,
        mut headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(body.len() as u64));
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
        let resp = self
            .client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn post_stream(
        &self,
        url: &str,
        mut headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(content_len));
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
        let body = reqwest::Body::wrap_stream(stream);
        let resp = self
            .client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn patch_chunk(
        &self,
        url: &str,
        mut headers: HeaderMap,
        chunk: Bytes,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError> {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(chunk.len() as u64));
        let range_str = format!("{}-{}", byte_range.0, byte_range.1);
        if let Ok(val) = HeaderValue::from_str(&range_str) {
            headers.insert(CONTENT_RANGE, val);
        }
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }

        let resp = self
            .client
            .patch(url)
            .headers(headers)
            .body(chunk)
            .send()
            .await?;

        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let location = resp_headers
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let range = resp_headers
            .get(RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range_header);

        Ok(UploadChunkResponse {
            status,
            headers: resp_headers,
            location,
            range,
        })
    }

    async fn patch_chunk_stream(
        &self,
        url: &str,
        mut headers: HeaderMap,
        stream: Self::BodyStream,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError> {
        let chunk_len = byte_range.1.saturating_sub(byte_range.0) + 1;
        headers.insert(CONTENT_LENGTH, HeaderValue::from(chunk_len));
        let range_str = format!("{}-{}", byte_range.0, byte_range.1);
        if let Ok(val) = HeaderValue::from_str(&range_str) {
            headers.insert(CONTENT_RANGE, val);
        }
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }

        let body = reqwest::Body::wrap_stream(stream);
        let resp = self
            .client
            .patch(url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let location = resp_headers
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let range = resp_headers
            .get(RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range_header);

        Ok(UploadChunkResponse {
            status,
            headers: resp_headers,
            location,
            range,
        })
    }

    async fn probe_upload_session(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<Option<u64>, TransportError> {
        let resp = self.client.get(url).headers(headers).send().await?;
        let headers = resp.headers();

        if let Some(range_val) = headers.get(RANGE).and_then(|v| v.to_str().ok())
            && let Some((_start, end)) = parse_range_header(range_val)
        {
            return Ok(Some(end));
        }
        Ok(None)
    }

    async fn put_chunk_finish(
        &self,
        url: &str,
        mut headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<StatusCode, TransportError> {
        if let Some((bytes, byte_range)) = final_chunk {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(bytes.len() as u64));
            let range_str = format!("{}-{}", byte_range.0, byte_range.1);
            if let Ok(val) = HeaderValue::from_str(&range_str) {
                headers.insert(CONTENT_RANGE, val);
            }
            if !headers.contains_key(CONTENT_TYPE) {
                headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
            }
            let resp = self
                .client
                .put(url)
                .headers(headers)
                .body(bytes)
                .send()
                .await?;
            Ok(resp.status())
        } else {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(0u64));
            let resp = self.client.put(url).headers(headers).send().await?;
            Ok(resp.status())
        }
    }

    async fn put_bytes(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<StatusCode, TransportError> {
        let resp = self
            .client
            .put(url)
            .headers(headers)
            .body(body)
            .send()
            .await?;
        Ok(resp.status())
    }

    async fn put_stream(
        &self,
        url: &str,
        mut headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<StatusCode, TransportError> {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(content_len));
        let body = reqwest::Body::wrap_stream(stream);
        let resp = self
            .client
            .put(url)
            .headers(headers)
            .body(body)
            .send()
            .await?;
        Ok(resp.status())
    }

    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        let resp = self.client.delete(url).headers(headers).send().await?;
        Ok(resp.status())
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}
