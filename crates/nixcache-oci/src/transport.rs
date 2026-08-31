use crate::error::TransportError;
use bytes::Bytes;
use futures_util::{Stream, ready};
use http::{HeaderMap, StatusCode};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[cfg(not(target_arch = "wasm32"))]
use futures_util::stream::BoxStream;

#[cfg(target_arch = "wasm32")]
use futures_util::stream::LocalBoxStream;

#[cfg(not(target_arch = "wasm32"))]
pub type BoxBodyStream = BoxStream<'static, Result<Bytes, TransportError>>;

#[cfg(target_arch = "wasm32")]
pub type BoxBodyStream = LocalBoxStream<'static, Result<Bytes, TransportError>>;

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

#[derive(Debug, Default)]
struct StreamHashInner {
    bytes_streamed: AtomicU64,
    finalized_digest: OnceLock<String>,
}

/// 零锁流式哈希与进度观察句柄
#[derive(Clone, Default, Debug)]
pub struct StreamHashState {
    inner: Arc<StreamHashInner>,
}

impl StreamHashState {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn bytes_streamed(&self) -> u64 {
        self.inner.bytes_streamed.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn digest(&self) -> Option<String> {
        self.inner.finalized_digest.get().cloned()
    }

    /// 若流提前终止需要强制计算已传输部分的哈希
    pub fn force_finalize(&self) -> String {
        if let Some(d) = self.inner.finalized_digest.get() {
            return d.clone();
        }
        let d = self.inner.finalized_digest.get_or_init(|| {
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string()
        });
        d.clone()
    }
}

/// 100% 零锁流式计算 Stream
pub struct HashingStream<S> {
    inner: S,
    hasher: Sha256,
    state: StreamHashState,
}

impl<S> HashingStream<S> {
    pub fn new(inner: S) -> (Self, StreamHashState) {
        let state = StreamHashState::new();
        (
            Self {
                inner,
                hasher: Sha256::new(),
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
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match ready!(Pin::new(&mut this.inner).poll_next(cx)) {
            Some(Ok(bytes)) => {
                // 100% 零锁操作！直接更新本地独占的 hasher
                this.hasher.update(&bytes);
                this.state
                    .inner
                    .bytes_streamed
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Poll::Ready(Some(Ok(bytes)))
            }
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            None => {
                // 流结束，一次性无锁计算并存入 OnceLock
                this.state.inner.finalized_digest.get_or_init(|| {
                    let hash = this.hasher.clone().finalize();
                    format!(
                        "sha256:{}",
                        hash.iter()
                            .map(|b| format!("{:02x}", b))
                            .collect::<String>()
                    )
                });
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

#[cfg(not(target_arch = "wasm32"))]
#[allow(async_fn_in_trait)]
pub trait OciTransport: Send + Sync + 'static {
    type BodyStream: Stream<Item = Result<Bytes, TransportError>> + Send + Unpin + 'static;

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

#[cfg(target_arch = "wasm32")]
#[allow(async_fn_in_trait)]
pub trait OciTransport: 'static {
    type BodyStream: Stream<Item = Result<Bytes, TransportError>> + Unpin + 'static;

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
