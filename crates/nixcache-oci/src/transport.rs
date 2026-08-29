use crate::error::TransportError;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream::BoxStream};
use http::{HeaderMap, StatusCode};
use reqwest::{Client, header::CONTENT_LENGTH};
use std::{fmt, io, time::Duration};

pub type BoxBodyStream = BoxStream<'static, Result<Bytes, io::Error>>;

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
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    }
}

#[async_trait]
pub trait OciTransport: Send + Sync + 'static {
    type BodyStream: Stream<Item = Result<Bytes, io::Error>> + Send + 'static;

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
}

#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client }
    }
}

impl ReqwestTransport {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }
}

#[async_trait]
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
                .map(|res| res.map_err(|e| io::Error::other(e.to_string()))),
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
        headers.insert(
            CONTENT_LENGTH,
            reqwest::header::HeaderValue::from(content_len),
        );
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
}
