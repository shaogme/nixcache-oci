use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE},
};
use nixcache_oci::{
    OciClient, OciDriver, OciTransport, RegistryCredentials, RegistryKind, TransportError,
    UploadChunkResponse, parse_range_header,
};
use reqwest::Client;
use std::{io::Error as IoError, time::Duration};
use tokio::time::sleep;

fn map_reqwest_error(err: reqwest::Error) -> TransportError {
    if err.is_timeout() {
        TransportError::Timeout {
            duration: Duration::from_secs(0),
        }
    } else if let Some(status) = err.status() {
        TransportError::HttpStatus {
            status,
            message: Some(err.to_string()),
        }
    } else if err.is_builder() || err.is_redirect() {
        TransportError::InvalidUri {
            url: err.url().map(|u| u.to_string()).unwrap_or_default(),
            reason: "Reqwest builder or redirect error",
        }
    } else {
        let endpoint = err
            .url()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        TransportError::ConnectionFailed {
            endpoint,
            source: IoError::other(err.to_string()),
        }
    }
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

impl OciTransport for ReqwestTransport {
    type BodyStream = BoxStream<'static, Result<Bytes, TransportError>>;

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        self.head_with_headers(url, headers).await.map(|(s, _)| s)
    }

    async fn head_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let resp = self
            .client
            .head(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Bytes), TransportError> {
        let resp = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.map_err(map_reqwest_error)?;
        Ok((status, headers, bytes))
    }

    async fn stream(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap, Self::BodyStream), TransportError> {
        let resp = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let stream: BoxStream<'static, Result<Bytes, TransportError>> = Box::pin(
            resp.bytes_stream()
                .map(|res| res.map_err(map_reqwest_error)),
        );
        Ok((status, headers, stream))
    }

    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let resp = self
            .client
            .post(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
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
            .await
            .map_err(map_reqwest_error)?;
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
            .await
            .map_err(map_reqwest_error)?;
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
            .await
            .map_err(map_reqwest_error)?;

        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let location = resp_headers
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let range = match resp_headers.get(RANGE) {
            Some(value) => {
                let text = value
                    .to_str()
                    .map_err(|_| TransportError::HeaderParse { header: "Range" })?;
                Some(
                    parse_range_header(text)
                        .ok_or(TransportError::HeaderParse { header: "Range" })?,
                )
            }
            None => None,
        };

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
            .await
            .map_err(map_reqwest_error)?;

        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let location = resp_headers
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let range = match resp_headers.get(RANGE) {
            Some(value) => {
                let text = value
                    .to_str()
                    .map_err(|_| TransportError::HeaderParse { header: "Range" })?;
                Some(
                    parse_range_header(text)
                        .ok_or(TransportError::HeaderParse { header: "Range" })?,
                )
            }
            None => None,
        };

        Ok(UploadChunkResponse {
            status,
            headers: resp_headers,
            location,
            range,
        })
    }

    async fn put_chunk_finish(
        &self,
        url: &str,
        headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<StatusCode, TransportError> {
        self.put_chunk_finish_with_headers(url, headers, final_chunk)
            .await
            .map(|(status, _)| status)
    }

    async fn put_chunk_finish_with_headers(
        &self,
        url: &str,
        mut headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
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
                .await
                .map_err(map_reqwest_error)?;
            Ok((resp.status(), resp.headers().clone()))
        } else {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(0u64));
            let resp = self
                .client
                .put(url)
                .headers(headers)
                .send()
                .await
                .map_err(map_reqwest_error)?;
            Ok((resp.status(), resp.headers().clone()))
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
            .await
            .map_err(map_reqwest_error)?;
        Ok(resp.status())
    }

    async fn put_bytes_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let resp = self
            .client
            .put(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn put_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<StatusCode, TransportError> {
        self.put_stream_with_headers(url, headers, stream, content_len)
            .await
            .map(|(status, _)| status)
    }

    async fn put_stream_with_headers(
        &self,
        url: &str,
        mut headers: HeaderMap,
        stream: Self::BodyStream,
        content_len: u64,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(content_len));
        let body = reqwest::Body::wrap_stream(stream);
        let resp = self
            .client
            .put(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<StatusCode, TransportError> {
        let resp = self
            .client
            .delete(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok(resp.status())
    }

    async fn delete_with_headers(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(StatusCode, HeaderMap), TransportError> {
        let resp = self
            .client
            .delete(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Ok((resp.status(), resp.headers().clone()))
    }

    async fn sleep(&self, duration: Duration) {
        sleep(duration).await;
    }
}

/// 自动根据 registry 域名探测驱动并创建 Tokio Reqwest OCI 客户端
pub fn create_tokio_reqwest_client(
    registry: &str,
    repo: &str,
    credentials: impl Into<RegistryCredentials>,
    write_access: bool,
) -> OciClient<ReqwestTransport> {
    let transport = ReqwestTransport::default();
    OciClient::with_transport(registry, repo, credentials, write_access, transport)
}

/// 基于指定 Driver 创建 Tokio Reqwest OCI 客户端
pub fn create_tokio_reqwest_client_with_driver(
    registry: &str,
    repo: &str,
    credentials: impl Into<RegistryCredentials>,
    write_access: bool,
    driver: impl Into<OciDriver>,
) -> OciClient<ReqwestTransport> {
    let transport = ReqwestTransport::default();
    OciClient::new(registry, repo, credentials, write_access, driver, transport)
}

/// 基于指定 RegistryKind 创建 Tokio Reqwest OCI 客户端
pub fn create_tokio_reqwest_client_from_kind(
    kind: RegistryKind,
    registry: &str,
    repo: &str,
    credentials: impl Into<RegistryCredentials>,
    write_access: bool,
) -> OciClient<ReqwestTransport> {
    let transport = ReqwestTransport::default();
    OciClient::from_kind(kind, registry, repo, credentials, write_access, transport)
}

#[cfg(test)]
mod tests {
    use super::create_tokio_reqwest_client;
    use bytes::Bytes;
    use futures_util::stream::BoxStream;
    use nixcache_oci::{
        BearerChallenge, GenericOciDriver, OciClient, RegistryCredentials, TransportError,
        UploadConfig,
    };
    use serde_json::json;
    use std::time::Duration;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };

    fn stream_for(data: Bytes) -> BoxStream<'static, Result<Bytes, TransportError>> {
        Box::pin(futures_util::stream::iter(vec![Ok(data)]))
    }

    fn chunked_config() -> UploadConfig {
        UploadConfig {
            chunk_threshold_bytes: 1024 * 1024,
            chunk_size_bytes: 1024 * 1024,
            max_retry_attempts: 1,
        }
    }

    #[tokio::test]
    async fn test_bearer_challenge_uses_registry_realm_and_replays_request() {
        let server = MockServer::start().await;
        let host = server.address().to_string();
        let realm = format!("http://{host}/auth/exchange?existing=1");
        let challenge = format!(
            "Bearer realm=\"{realm}\", service=\"{host}\", scope=\"repository:test/repo/nix-cache:pull\""
        );

        Mock::given(method("GET"))
            .and(path("/v2/test/repo/nix-cache/manifests/cache-index"))
            .respond_with(ResponseTemplate::new(401).insert_header("WWW-Authenticate", challenge))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/auth/exchange"))
            .and(query_param("existing", "1"))
            .and(query_param("service", &host))
            .and(query_param("scope", "repository:test/repo/nix-cache:pull"))
            .and(header("Authorization", "Basic Y3VzdG9tOnNlY3JldA=="))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "realm-token", "expires_in": 120})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/test/repo/nix-cache/manifests/cache-index"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","size":2},"layers":[]}"#,
            ))
            .mount(&server)
            .await;

        let credentials = nixcache_oci::RegistryCredentials::with_username("custom", "secret");
        let client = super::create_tokio_reqwest_client(&host, "test/repo", credentials, false);
        let artifact = client.manifests().get("cache-index").await.unwrap();
        assert!(artifact.is_some());
    }

    #[tokio::test]
    async fn test_reqwest_transport_token_exchange_mock() {
        let server = MockServer::start().await;
        let host = server.address().to_string();

        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param(
                "scope",
                "repository:test/repo/nix-cache:pull,push",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "token": "mocked-jwt-token" })),
            )
            .mount(&server)
            .await;

        let credentials = RegistryCredentials::with_username("custom", "secret-gh-token");
        let client = create_tokio_reqwest_client(&host, "test/repo", credentials, true);
        let challenge = BearerChallenge::new(
            format!("http://{host}/token"),
            Some(host.clone()),
            Some("repository:test/repo/nix-cache:pull,push".to_string()),
        )
        .unwrap();
        let token = client
            .get_token(&challenge)
            .await
            .expect("Failed to fetch token");
        assert_eq!(token.as_ref(), "mocked-jwt-token");

        let cached_token = client
            .get_token(&challenge)
            .await
            .expect("Failed to get cached token");
        assert_eq!(cached_token.as_ref(), "mocked-jwt-token");
    }

    #[tokio::test]
    async fn test_reqwest_streaming_upload_recovers_from_503_and_updates_location() {
        let server = MockServer::start().await;
        let host = server.address().to_string();
        let session = "/v2/test/repo/nix-cache/blobs/uploads/stream-session";
        let relocated_session = "/v2/test/repo/nix-cache/blobs/uploads/relocated-session";

        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .respond_with(ResponseTemplate::new(202).insert_header("Location", session))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(session))
            .and(header("Content-Range", "0-1048575"))
            .respond_with(ResponseTemplate::new(503).insert_header("Location", relocated_session))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(relocated_session))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(relocated_session))
            .and(header("Content-Range", "0-1048575"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(relocated_session))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let transport = super::ReqwestTransport::default();
        let client = OciClient::new(&host, "test/repo", "", true, GenericOciDriver, transport);
        let result = client
            .blobs()
            .push_resumable(
                stream_for(Bytes::from(vec![0x42; 1024 * 1024])),
                &chunked_config(),
            )
            .await
            .unwrap();

        assert_eq!(result.1, 1024 * 1024);
    }

    #[tokio::test]
    async fn test_reqwest_streaming_upload_recovers_after_patch_timeout() {
        let server = MockServer::start().await;
        let host = server.address().to_string();
        let session = "/v2/test/repo/nix-cache/blobs/uploads/timeout-session";

        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/test/repo/nix-cache/blobs/uploads/"))
            .respond_with(ResponseTemplate::new(202).insert_header("Location", session))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(session))
            .and(header("Content-Range", "0-1048575"))
            .respond_with(ResponseTemplate::new(202).set_delay(Duration::from_millis(100)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(session))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(session))
            .and(header("Content-Range", "0-1048575"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(session))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        let transport = super::ReqwestTransport::new(http_client);
        let client = OciClient::new(&host, "test/repo", "", true, GenericOciDriver, transport);
        let result = client
            .blobs()
            .push_resumable(
                stream_for(Bytes::from(vec![0x43; 1024 * 1024])),
                &chunked_config(),
            )
            .await
            .unwrap();

        assert_eq!(result.1, 1024 * 1024);
    }
}
