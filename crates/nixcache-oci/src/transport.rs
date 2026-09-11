use crate::{
    error::{OciError, TransportError},
    integrity::{ContentDigest, verify_size, verify_stream_digest},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, ready};
use http::{HeaderMap, HeaderValue, StatusCode};
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

pub struct OciBlobStream<S> {
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

/// 解析并验证响应的 Content-Length。无效 header 必须失败，不能降级为 chunked。
pub fn parse_content_length(headers: &HeaderMap) -> Result<Option<u64>, TransportError> {
    let Some(value) = headers.get(http::header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let text = value.to_str().map_err(|_| TransportError::HeaderParse {
        header: "Content-Length",
    })?;
    let length = text
        .parse::<u64>()
        .map_err(|_| TransportError::HeaderParse {
            header: "Content-Length",
        })?;
    Ok(Some(length))
}

pub fn check_content_length(
    url: &str,
    headers: &HeaderMap,
    max_bytes: u64,
) -> Result<Option<u64>, TransportError> {
    let length = parse_content_length(headers)?;
    if let Some(length) = length
        && length > max_bytes
    {
        return Err(TransportError::ResponseTooLarge {
            url: url.to_string(),
            limit: max_bytes,
            actual: length,
        });
    }
    Ok(length)
}

/// 在 transport 层收集有限 body；检查发生在追加 chunk 之前。
pub async fn collect_limited<S>(
    url: &str,
    max_bytes: u64,
    stream: S,
) -> Result<Bytes, TransportError>
where
    S: Stream<Item = Result<Bytes, TransportError>>,
{
    let capacity = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let mut body = Vec::with_capacity(capacity.min(1024 * 1024));
    let mut stream = Box::pin(stream);
    while let Some(chunk) = stream.as_mut().next().await {
        let chunk = chunk?;
        let current = body.len() as u64;
        let chunk_len = chunk.len() as u64;
        let total = current.saturating_add(chunk_len);
        if total > max_bytes {
            return Err(TransportError::ResponseTooLarge {
                url: url.to_string(),
                limit: max_bytes,
                actual: total,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

#[derive(Debug, Clone)]
pub struct UploadChunkResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub location: Option<String>,
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone)]
enum StreamHashTerminal {
    Complete(ContentDigest),
    Failed,
    Aborted,
}

#[derive(Debug, Default)]
struct StreamHashInner {
    bytes_streamed: AtomicU64,
    terminal: OnceLock<StreamHashTerminal>,
}

/// 流式哈希的终态快照。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamHashStatus {
    /// 尚未观察到 EOF，也没有发生错误或提前终止。
    InProgress,
    /// 已观察到 EOF，digest 覆盖完整输入。
    Complete,
    /// 底层流返回了错误；该状态不可恢复。
    Failed,
    /// `HashingStream` 在观察到 EOF 前被丢弃。
    Aborted,
}

/// 零锁流式哈希与进度观察句柄。
///
/// `digest` 只在关联的 [`HashingStream`] 观察到底层流的 EOF 后返回完整输入
/// 的 digest。它不会返回已读前缀的 digest；流尚未结束、返回错误或在 EOF 前
/// 被丢弃时均返回 `None`。
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
    pub fn digest(&self) -> Option<ContentDigest> {
        match self.inner.terminal.get() {
            Some(StreamHashTerminal::Complete(digest)) => Some(digest.clone()),
            Some(StreamHashTerminal::Failed | StreamHashTerminal::Aborted) | None => None,
        }
    }

    /// 返回当前终态，但不会暴露不完整输入的 digest。
    #[inline]
    pub fn status(&self) -> StreamHashStatus {
        match self.inner.terminal.get() {
            Some(StreamHashTerminal::Complete(_)) => StreamHashStatus::Complete,
            Some(StreamHashTerminal::Failed) => StreamHashStatus::Failed,
            Some(StreamHashTerminal::Aborted) => StreamHashStatus::Aborted,
            None => StreamHashStatus::InProgress,
        }
    }
}

/// 计算完整输入 SHA-256 的流包装器。
///
/// 只有底层流返回 `None`（EOF）后，关联的 [`StreamHashState::digest`] 才会
/// 变为可用。底层错误会永久进入失败终态；在 EOF 前丢弃此包装器会进入提前
/// 终止终态。这两种情况都不会产生 digest，也不会在后续 poll 中被转化为成功。
pub struct HashingStream<S> {
    inner: S,
    hasher: Sha256,
    state: StreamHashState,
}

/// NAR 等大 blob 的零拷贝完整性验证包装器。
pub struct VerifiedBlobStream<S> {
    inner: S,
    target: String,
    expected_digest: String,
    header_digest: Option<HeaderValue>,
    content_length: Option<u64>,
    max_bytes: u64,
    bytes_seen: u64,
    hasher: Sha256,
    finished: bool,
}

impl<S> VerifiedBlobStream<S> {
    pub fn new(
        inner: S,
        target: impl Into<String>,
        expected_digest: impl Into<String>,
        headers: &HeaderMap,
        max_bytes: u64,
    ) -> Result<Self, OciError> {
        let target = target.into();
        let expected_digest = expected_digest.into();
        ContentDigest::parse(&expected_digest)?;
        let content_length = parse_content_length(headers).map_err(OciError::Transport)?;
        if let Some(content_length) = content_length
            && content_length > max_bytes
        {
            return Err(OciError::SizeLimitExceeded {
                target: target.clone(),
                limit: max_bytes,
                actual: content_length,
            });
        }
        Ok(Self {
            inner,
            target,
            expected_digest,
            header_digest: headers.get("Docker-Content-Digest").cloned(),
            content_length,
            max_bytes,
            bytes_seen: 0,
            hasher: Sha256::new(),
            finished: false,
        })
    }
}

impl<S> Stream for VerifiedBlobStream<S>
where
    S: Stream<Item = Result<Bytes, TransportError>> + Unpin,
{
    type Item = Result<Bytes, OciError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        match ready!(Pin::new(&mut this.inner).poll_next(cx)) {
            Some(Err(error)) => {
                this.finished = true;
                Poll::Ready(Some(Err(OciError::Transport(error))))
            }
            Some(Ok(bytes)) => {
                let chunk_len = bytes.len() as u64;
                let total = this.bytes_seen.saturating_add(chunk_len);
                if total > this.max_bytes {
                    this.finished = true;
                    return Poll::Ready(Some(Err(OciError::SizeLimitExceeded {
                        target: this.target.clone(),
                        limit: this.max_bytes,
                        actual: total,
                    })));
                }
                this.bytes_seen = total;
                this.hasher.update(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            None => {
                this.finished = true;
                let actual = ContentDigest::from_hasher(std::mem::take(&mut this.hasher));
                let result = this
                    .content_length
                    .map(|length| verify_size(&this.target, length, this.bytes_seen))
                    .unwrap_or(Ok(()))
                    .and_then(|_| {
                        verify_stream_digest(
                            &this.target,
                            &this.expected_digest,
                            this.header_digest.as_ref(),
                            &actual,
                        )
                    });
                match result {
                    Ok(()) => Poll::Ready(None),
                    Err(error) => Poll::Ready(Some(Err(error))),
                }
            }
        }
    }
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

impl<S> Drop for HashingStream<S> {
    fn drop(&mut self) {
        let _ = self.state.inner.terminal.set(StreamHashTerminal::Aborted);
    }
}

impl<S, E> Stream for HashingStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.state.inner.terminal.get().is_some() {
            return Poll::Ready(None);
        }
        match ready!(Pin::new(&mut this.inner).poll_next(cx)) {
            Some(Ok(bytes)) => {
                // hasher 由当前 stream 独占；共享句柄只观察进度和终态。
                this.hasher.update(&bytes);
                this.state
                    .inner
                    .bytes_streamed
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Poll::Ready(Some(Ok(bytes)))
            }
            Some(Err(e)) => {
                let _ = this.state.inner.terminal.set(StreamHashTerminal::Failed);
                Poll::Ready(Some(Err(e)))
            }
            None => {
                let digest = ContentDigest::from_hasher(this.hasher.clone());
                let _ = this
                    .state
                    .inner
                    .terminal
                    .set(StreamHashTerminal::Complete(digest));
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

#[allow(async_fn_in_trait)]
pub trait OciTransport: 'static {
    type BodyStream: Stream<Item = Result<Bytes, TransportError>> + Unpin + 'static;

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError>;

    async fn head_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError>;

    async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
        max_bytes: u64,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError>;

    async fn stream(
        &self,
        url: &str,
        headers: HeaderMap,
        max_bytes: u64,
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

    /// 完成分块上传 (PUT finish，可带尾部数据或为空 Body)
    async fn put_chunk_finish(
        &self,
        url: &str,
        headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<StatusCode, TransportError>;

    /// 完成分块上传并保留 response headers，以便处理空 body PUT 的 challenge。
    async fn put_chunk_finish_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.put_chunk_finish(url, headers, final_chunk)
            .await
            .map(|status| (status, HeaderMap::new()))
    }

    async fn put_bytes(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<StatusCode, TransportError>;

    /// 与 `put_bytes` 相同，但保留 response headers 以便处理 Bearer challenge。
    async fn put_bytes_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.put_bytes(url, headers, body)
            .await
            .map(|status| (status, HeaderMap::new()))
    }

    async fn put_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<StatusCode, TransportError>;

    /// 流式 body 已经可能被消费，返回 headers 仅用于把 401 映射为不可重放错误。
    async fn put_stream_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.put_stream(url, headers, stream, content_len)
            .await
            .map(|status| (status, HeaderMap::new()))
    }

    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError>;

    /// 与 `delete` 相同，但保留 response headers 以便处理 Bearer challenge。
    async fn delete_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        self.delete(url, headers)
            .await
            .map(|status| (status, HeaderMap::new()))
    }

    async fn sleep(&self, duration: Duration);
}

#[cfg(test)]
mod tests {
    use super::{VerifiedBlobStream, check_content_length, collect_limited, parse_content_length};
    use crate::{ContentDigest, OciError, TransportError};
    use bytes::Bytes;
    use futures_util::{StreamExt, stream};
    use http::{HeaderMap, HeaderValue};

    #[tokio::test]
    async fn collect_limited_rejects_chunked_body_before_append() {
        let chunks = stream::iter(vec![
            Ok(Bytes::from_static(b"123")),
            Ok(Bytes::from_static(b"45")),
        ]);
        let error = collect_limited("blob", 4, chunks)
            .await
            .expect_err("the second chunk must exceed the limit");
        assert!(matches!(
            error,
            TransportError::ResponseTooLarge {
                limit: 4,
                actual: 5,
                ..
            }
        ));
    }

    #[test]
    fn content_length_is_parsed_and_checked_before_body_reads() {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Length", HeaderValue::from_static("9"));
        assert_eq!(parse_content_length(&headers).unwrap(), Some(9));
        assert!(matches!(
            check_content_length("blob", &headers, 8),
            Err(TransportError::ResponseTooLarge {
                limit: 8,
                actual: 9,
                ..
            })
        ));

        headers.insert("Content-Length", HeaderValue::from_static("invalid"));
        assert!(matches!(
            parse_content_length(&headers),
            Err(TransportError::HeaderParse {
                header: "Content-Length"
            })
        ));
    }

    #[tokio::test]
    async fn verified_stream_only_finishes_successfully_after_eof_digest_check() {
        let body = Bytes::from_static(b"stream body");
        let digest = ContentDigest::from_bytes(&body).to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            "Content-Length",
            HeaderValue::from_str(&body.len().to_string()).unwrap(),
        );
        headers.insert(
            "Docker-Content-Digest",
            HeaderValue::from_str(&digest).unwrap(),
        );
        let mut verified = VerifiedBlobStream::new(
            stream::iter(vec![Ok::<Bytes, TransportError>(body.clone())]),
            "blob",
            &digest,
            &headers,
            1024,
        )
        .unwrap();
        assert_eq!(verified.next().await.unwrap().unwrap(), body);
        assert!(verified.next().await.is_none());

        let wrong_digest =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let mut wrong = VerifiedBlobStream::new(
            stream::iter(vec![Ok::<Bytes, TransportError>(Bytes::from_static(
                b"stream body",
            ))]),
            "blob",
            wrong_digest,
            &HeaderMap::new(),
            1024,
        )
        .unwrap();
        assert!(wrong.next().await.unwrap().is_ok());
        assert!(matches!(
            wrong.next().await,
            Some(Err(OciError::DigestMismatch { .. }))
        ));
    }

    #[tokio::test]
    async fn verified_stream_rejects_size_mismatch_and_cumulative_overflow() {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Length", HeaderValue::from_static("10"));
        let digest = ContentDigest::from_bytes(b"12345").to_string();
        let mut short = VerifiedBlobStream::new(
            stream::iter(vec![Ok::<Bytes, TransportError>(Bytes::from_static(
                b"12345",
            ))]),
            "blob",
            digest,
            &headers,
            100,
        )
        .unwrap();
        assert!(short.next().await.unwrap().is_ok());
        assert!(matches!(
            short.next().await,
            Some(Err(OciError::SizeMismatch { .. }))
        ));

        let digest = ContentDigest::from_bytes(b"1234").to_string();
        let mut oversized = VerifiedBlobStream::new(
            stream::iter(vec![Ok::<Bytes, TransportError>(Bytes::from_static(
                b"1234",
            ))]),
            "blob",
            digest,
            &HeaderMap::new(),
            3,
        )
        .unwrap();
        assert!(matches!(
            oversized.next().await,
            Some(Err(OciError::SizeLimitExceeded { .. }))
        ));
    }
}
