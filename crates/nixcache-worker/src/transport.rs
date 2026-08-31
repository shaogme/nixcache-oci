use bytes::Bytes;
use futures_util::TryStreamExt;
use http::{
    HeaderMap, StatusCode,
    header::{HeaderName, HeaderValue},
};
use nixcache_oci::{OciTransport, TransportError, UploadChunkResponse, parse_range_header};
use std::{fmt::Display, io::Error as IoError, time::Duration};
use worker::{Delay, Fetch, Headers, Method, Request, RequestInit, wasm_bindgen::JsValue};

#[cfg(not(target_arch = "wasm32"))]
use futures_util::stream::BoxStream;

#[cfg(target_arch = "wasm32")]
use futures_util::{StreamExt, stream::LocalBoxStream};

#[derive(Clone, Default)]
pub struct WorkerFetchTransport;

fn map_worker_error(url: &str, err: impl Display) -> TransportError {
    TransportError::ConnectionFailed {
        endpoint: url.to_string(),
        source: IoError::other(err.to_string()),
    }
}

fn convert_to_worker_headers(headers: &HeaderMap) -> Result<Headers, TransportError> {
    let worker_headers = Headers::new();
    for (key, val) in headers {
        if let Ok(val_str) = val.to_str() {
            worker_headers
                .set(key.as_str(), val_str)
                .map_err(|_| TransportError::HeaderParse {
                    header: "worker_header_set",
                })?;
        }
    }
    Ok(worker_headers)
}

fn convert_from_worker_headers(headers: &Headers) -> Result<HeaderMap, TransportError> {
    let mut http_headers = HeaderMap::new();
    for (key, val) in headers {
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(&val),
        ) {
            http_headers.insert(k, v);
        }
    }
    Ok(http_headers)
}

const MAX_REDIRECTS: usize = 5;

fn is_redirect_status(code: u16) -> bool {
    matches!(code, 301 | 302 | 303 | 307 | 308)
}

fn resolve_redirect_url(base_url: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        location.to_string()
    } else if let Ok(base) = worker::Url::parse(base_url)
        && let Ok(joined) = base.join(location)
    {
        joined.to_string()
    } else {
        location.to_string()
    }
}

impl OciTransport for WorkerFetchTransport {
    #[cfg(not(target_arch = "wasm32"))]
    type BodyStream = BoxStream<'static, Result<Bytes, TransportError>>;

    #[cfg(target_arch = "wasm32")]
    type BodyStream = LocalBoxStream<'static, Result<Bytes, TransportError>>;

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        let mut current_url = url.to_string();
        let mut current_headers = headers;
        let mut redirect_count = 0;

        loop {
            let worker_headers = convert_to_worker_headers(&current_headers)?;
            let mut req_init = RequestInit::new();
            req_init.with_method(Method::Head);
            req_init.with_headers(worker_headers);
            req_init.with_redirect(worker::RequestRedirect::Manual);

            let req = Request::new_with_init(&current_url, &req_init)
                .map_err(|e| map_worker_error(&current_url, e))?;
            let resp = Fetch::Request(req)
                .send()
                .await
                .map_err(|e| map_worker_error(&current_url, e))?;

            let status_code = resp.status_code();
            if is_redirect_status(status_code)
                && redirect_count < MAX_REDIRECTS
                && let Ok(Some(location)) = resp.headers().get("Location")
            {
                current_url = resolve_redirect_url(&current_url, &location);
                current_headers.remove(http::header::AUTHORIZATION);
                redirect_count += 1;
                continue;
            }

            return StatusCode::from_u16(status_code).map_err(|_| TransportError::HttpStatus {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: Some(format!("Invalid status code {}", status_code)),
            });
        }
    }

    async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError> {
        let mut current_url = url.to_string();
        let mut current_headers = headers;
        let mut redirect_count = 0;

        loop {
            let worker_headers = convert_to_worker_headers(&current_headers)?;
            let mut req_init = RequestInit::new();
            req_init.with_method(Method::Get);
            req_init.with_headers(worker_headers);
            req_init.with_redirect(worker::RequestRedirect::Manual);

            let req = Request::new_with_init(&current_url, &req_init)
                .map_err(|e| map_worker_error(&current_url, e))?;
            let mut resp = Fetch::Request(req)
                .send()
                .await
                .map_err(|e| map_worker_error(&current_url, e))?;

            let status_code = resp.status_code();
            if is_redirect_status(status_code)
                && redirect_count < MAX_REDIRECTS
                && let Ok(Some(location)) = resp.headers().get("Location")
            {
                current_url = resolve_redirect_url(&current_url, &location);
                current_headers.remove(http::header::AUTHORIZATION);
                redirect_count += 1;
                continue;
            }

            let status =
                StatusCode::from_u16(status_code).map_err(|_| TransportError::HttpStatus {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: Some(format!("Invalid status code {}", status_code)),
                })?;
            let resp_headers = convert_from_worker_headers(resp.headers())?;
            let bytes = Bytes::from(
                resp.bytes()
                    .await
                    .map_err(|e| map_worker_error(&current_url, e))?,
            );

            return Ok((status, resp_headers, bytes));
        }
    }

    #[cfg(target_arch = "wasm32")]
    async fn stream(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError> {
        let mut current_url = url.to_string();
        let mut current_headers = headers;
        let mut redirect_count = 0;

        loop {
            let worker_headers = convert_to_worker_headers(&current_headers)?;
            let mut req_init = RequestInit::new();
            req_init.with_method(Method::Get);
            req_init.with_headers(worker_headers);
            req_init.with_redirect(worker::RequestRedirect::Manual);

            let req = Request::new_with_init(&current_url, &req_init)
                .map_err(|e| map_worker_error(&current_url, e))?;
            let mut resp = Fetch::Request(req)
                .send()
                .await
                .map_err(|e| map_worker_error(&current_url, e))?;

            let status_code = resp.status_code();
            if is_redirect_status(status_code)
                && redirect_count < MAX_REDIRECTS
                && let Ok(Some(location)) = resp.headers().get("Location")
            {
                current_url = resolve_redirect_url(&current_url, &location);
                current_headers.remove(http::header::AUTHORIZATION);
                redirect_count += 1;
                continue;
            }

            let status =
                StatusCode::from_u16(status_code).map_err(|_| TransportError::HttpStatus {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: Some(format!("Invalid status code {}", status_code)),
                })?;
            let resp_headers = convert_from_worker_headers(resp.headers())?;
            let err_url = current_url.clone();
            let byte_stream = resp
                .stream()
                .map_err(|e| map_worker_error(&current_url, e))?;
            let mapped = byte_stream.map(move |res| {
                res.map(Bytes::from)
                    .map_err(|e| map_worker_error(&err_url, e))
            });
            let stream: Self::BodyStream = Box::pin(mapped);

            return Ok((status, resp_headers, stream));
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
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

        let req = Request::new_with_init(url, &req_init).map_err(|e| map_worker_error(url, e))?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| map_worker_error(url, e))?;

        let status =
            StatusCode::from_u16(resp.status_code()).map_err(|_| TransportError::HttpStatus {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: Some(format!("Invalid status code {}", resp.status_code())),
            })?;
        let resp_headers = convert_from_worker_headers(resp.headers())?;
        Ok((status, resp_headers))
    }

    async fn post_bytes(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let worker_headers = convert_to_worker_headers(&headers)?;
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Post);
        req_init.with_headers(worker_headers);
        req_init.with_body(Some(JsValue::from(body.to_vec())));

        let req = Request::new_with_init(url, &req_init).map_err(|e| map_worker_error(url, e))?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| map_worker_error(url, e))?;

        let status =
            StatusCode::from_u16(resp.status_code()).map_err(|_| TransportError::HttpStatus {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: Some(format!("Invalid status code {}", resp.status_code())),
            })?;
        let resp_headers = convert_from_worker_headers(resp.headers())?;
        Ok((status, resp_headers))
    }

    async fn post_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        _content_len: u64,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let bytes = stream
            .try_collect::<Vec<Bytes>>()
            .await?
            .into_iter()
            .flat_map(|b| b.to_vec())
            .collect::<Vec<u8>>();
        self.post_bytes(url, headers, Bytes::from(bytes)).await
    }

    async fn patch_chunk(
        &self,
        url: &str,
        mut headers: HeaderMap,
        chunk: Bytes,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError> {
        let range_str = format!("{}-{}", byte_range.0, byte_range.1);
        if let Ok(val) = HeaderValue::from_str(&range_str) {
            headers.insert("Content-Range", val);
        }
        let worker_headers = convert_to_worker_headers(&headers)?;
        let mut req_init = RequestInit::new();
        req_init.with_method(Method::Patch);
        req_init.with_headers(worker_headers);
        req_init.with_body(Some(JsValue::from(chunk.to_vec())));

        let req = Request::new_with_init(url, &req_init).map_err(|e| map_worker_error(url, e))?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| map_worker_error(url, e))?;

        let status =
            StatusCode::from_u16(resp.status_code()).map_err(|_| TransportError::HttpStatus {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: Some(format!("Invalid status code {}", resp.status_code())),
            })?;
        let resp_headers = convert_from_worker_headers(resp.headers())?;
        let location = resp_headers
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let range = resp_headers
            .get("Range")
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
        headers: HeaderMap,
        stream: Self::BodyStream,
        byte_range: (u64, u64),
    ) -> Result<UploadChunkResponse, TransportError> {
        let bytes = stream
            .try_collect::<Vec<Bytes>>()
            .await?
            .into_iter()
            .flat_map(|b| b.to_vec())
            .collect::<Vec<u8>>();
        self.patch_chunk(url, headers, Bytes::from(bytes), byte_range)
            .await
    }

    async fn probe_upload_session(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<Option<u64>, TransportError> {
        let (status, resp_headers, _) = self.get(url, headers).await?;
        if status.is_success()
            && let Some(range_val) = resp_headers.get("Range").and_then(|v| v.to_str().ok())
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
            let range_str = format!("{}-{}", byte_range.0, byte_range.1);
            if let Ok(val) = HeaderValue::from_str(&range_str) {
                headers.insert("Content-Range", val);
            }
            self.put_bytes(url, headers, bytes).await
        } else {
            self.put_bytes(url, headers, Bytes::new()).await
        }
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
        req_init.with_body(Some(JsValue::from(body.to_vec())));

        let req = Request::new_with_init(url, &req_init).map_err(|e| map_worker_error(url, e))?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| map_worker_error(url, e))?;
        StatusCode::from_u16(resp.status_code()).map_err(|_| TransportError::HttpStatus {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: Some(format!("Invalid status code {}", resp.status_code())),
        })
    }

    async fn put_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        _content_len: u64,
    ) -> Result<StatusCode, TransportError> {
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

        let req = Request::new_with_init(url, &req_init).map_err(|e| map_worker_error(url, e))?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| map_worker_error(url, e))?;
        StatusCode::from_u16(resp.status_code()).map_err(|_| TransportError::HttpStatus {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: Some(format!("Invalid status code {}", resp.status_code())),
        })
    }

    async fn sleep(&self, duration: Duration) {
        Delay::from(duration).await;
    }
}
