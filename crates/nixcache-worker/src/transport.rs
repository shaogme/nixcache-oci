use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use http::{HeaderMap, StatusCode};
use nixcache_oci::{OciTransport, TransportError};
use std::{pin::Pin, time::Duration};

#[derive(Clone, Default)]
pub struct WorkerFetchTransport;

#[cfg(target_arch = "wasm32")]
use worker::{Fetch, Headers, Method, Request, RequestInit};

#[cfg(target_arch = "wasm32")]
fn convert_to_worker_headers(headers: &HeaderMap) -> Result<Headers, TransportError> {
    let worker_headers = Headers::new();
    for (key, val) in headers {
        if let Ok(val_str) = val.to_str() {
            worker_headers
                .set(key.as_str(), val_str)
                .map_err(|e| TransportError::Other(e.to_string()))?;
        }
    }
    Ok(worker_headers)
}

#[cfg(target_arch = "wasm32")]
fn convert_from_worker_headers(headers: &Headers) -> Result<HeaderMap, TransportError> {
    let mut http_headers = HeaderMap::new();
    for (key, val) in headers {
        if let (Ok(k), Ok(v)) = (
            http::header::HeaderName::from_bytes(key.as_bytes()),
            http::header::HeaderValue::from_str(&val),
        ) {
            http_headers.insert(k, v);
        }
    }
    Ok(http_headers)
}

#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
impl OciTransport for WorkerFetchTransport {
    type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + 'static>>;

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        let worker_headers = convert_to_worker_headers(&headers)?;
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Head);
        req_init.with_headers(worker_headers);

        let req = Request::new_with_init(url, &req_init)
            .map_err(|e| TransportError::Network(e.to_string()))?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| TransportError::Network(e.to_string()))?;
        StatusCode::from_u16(resp.status_code()).map_err(|e| TransportError::Other(e.to_string()))
    }

    async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError> {
        let worker_headers = convert_to_worker_headers(&headers)?;
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Get);
        req_init.with_headers(worker_headers);

        let req = Request::new_with_init(url, &req_init)
            .map_err(|e| TransportError::Network(e.to_string()))?;
        let mut resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| TransportError::Network(e.to_string()))?;

        let status = StatusCode::from_u16(resp.status_code())
            .map_err(|e| TransportError::Other(e.to_string()))?;
        let resp_headers = convert_from_worker_headers(resp.headers())?;
        let bytes = Bytes::from(
            resp.bytes()
                .await
                .map_err(|e| TransportError::Network(e.to_string()))?,
        );

        Ok((status, resp_headers, bytes))
    }

    async fn stream(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError> {
        let (status, resp_headers, bytes) = self.get(url, headers).await?;
        let stream: Self::BodyStream =
            Box::pin(futures_util::stream::once(async move { Ok(bytes) }));
        Ok((status, resp_headers, stream))
    }

    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let worker_headers = convert_to_worker_headers(&headers)?;
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Post);
        req_init.with_headers(worker_headers);

        let req = Request::new_with_init(url, &req_init)
            .map_err(|e| TransportError::Network(e.to_string()))?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| TransportError::Network(e.to_string()))?;

        let status = StatusCode::from_u16(resp.status_code())
            .map_err(|e| TransportError::Other(e.to_string()))?;
        let resp_headers = convert_from_worker_headers(resp.headers())?;
        Ok((status, resp_headers))
    }

    async fn put_bytes(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<StatusCode, TransportError> {
        let worker_headers = convert_to_worker_headers(&headers)?;
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Put);
        req_init.with_headers(worker_headers);
        req_init.with_body(Some(worker::wasm_bindgen::JsValue::from(body.to_vec())));

        let req = Request::new_with_init(url, &req_init)
            .map_err(|e| TransportError::Network(e.to_string()))?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| TransportError::Network(e.to_string()))?;
        StatusCode::from_u16(resp.status_code()).map_err(|e| TransportError::Other(e.to_string()))
    }

    async fn put_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        _content_len: u64,
    ) -> Result<StatusCode, TransportError> {
        use futures_util::TryStreamExt;
        let bytes = stream
            .try_collect::<Vec<Bytes>>()
            .await?
            .into_iter()
            .flat_map(|b| b.to_vec())
            .collect::<Vec<u8>>();
        self.put_bytes(url, headers, Bytes::from(bytes)).await
    }

    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        let worker_headers = convert_to_worker_headers(&headers)?;
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Delete);
        req_init.with_headers(worker_headers);

        let req = Request::new_with_init(url, &req_init)
            .map_err(|e| TransportError::Network(e.to_string()))?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| TransportError::Network(e.to_string()))?;
        StatusCode::from_u16(resp.status_code()).map_err(|e| TransportError::Other(e.to_string()))
    }

    async fn sleep(&self, duration: Duration) {
        worker::Delay::from(duration).await;
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl OciTransport for WorkerFetchTransport {
    type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send + 'static>>;

    async fn head(&self, _url: &str, _headers: HeaderMap) -> Result<StatusCode, TransportError> {
        unimplemented!("WorkerFetchTransport only runs in Wasm / Cloudflare Workers environment")
    }

    async fn get(
        &self,
        _url: &str,
        _headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError> {
        unimplemented!("WorkerFetchTransport only runs in Wasm / Cloudflare Workers environment")
    }

    async fn stream(
        &self,
        _url: &str,
        _headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError> {
        unimplemented!("WorkerFetchTransport only runs in Wasm / Cloudflare Workers environment")
    }

    async fn post(
        &self,
        _url: &str,
        _headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        unimplemented!("WorkerFetchTransport only runs in Wasm / Cloudflare Workers environment")
    }

    async fn put_bytes(
        &self,
        _url: &str,
        _headers: HeaderMap,
        _body: Bytes,
    ) -> Result<StatusCode, TransportError> {
        unimplemented!("WorkerFetchTransport only runs in Wasm / Cloudflare Workers environment")
    }

    async fn put_stream(
        &self,
        _url: &str,
        _headers: HeaderMap,
        _stream: Self::BodyStream,
        _content_len: u64,
    ) -> Result<StatusCode, TransportError> {
        unimplemented!("WorkerFetchTransport only runs in Wasm / Cloudflare Workers environment")
    }

    async fn delete(&self, _url: &str, _headers: HeaderMap) -> Result<StatusCode, TransportError> {
        unimplemented!("WorkerFetchTransport only runs in Wasm / Cloudflare Workers environment")
    }

    async fn sleep(&self, _duration: Duration) {
        unimplemented!("WorkerFetchTransport only runs in Wasm / Cloudflare Workers environment")
    }
}
