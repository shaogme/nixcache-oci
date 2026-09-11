//! Authenticated request primitives shared by all domain clients.

use super::OciClient;
use crate::{
    auth::{BearerChallenge, parse_www_authenticate},
    error::OciError,
    transport::{OciTransport, UploadChunkResponse},
};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};
use std::sync::Arc;

impl<T: OciTransport + Clone> OciClient<T> {
    pub async fn get_token(&self, challenge: &BearerChallenge) -> Result<Arc<str>, OciError> {
        self.token_manager
            .get_token(&self.transport, challenge)
            .await
    }

    pub async fn get_auth_headers(&self) -> Result<HeaderMap, OciError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Accept",
            HeaderValue::from_static(
                "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json",
            ),
        );
        if let Some(token) = self.token_manager.cached_token().await {
            let auth_val = format!("Bearer {}", token);
            if let Ok(value) = HeaderValue::from_str(&auth_val) {
                headers.insert("Authorization", value);
            }
        }
        Ok(headers)
    }

    fn request_auth_error(
        operation: &'static str,
        status: StatusCode,
        details: impl Into<String>,
    ) -> OciError {
        OciError::AuthenticationFailed {
            operation,
            status,
            details: details.into(),
        }
    }

    fn request_with_bearer(mut headers: HeaderMap, token: &str) -> HeaderMap {
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert("Authorization", value);
        }
        headers
    }

    async fn request_challenge_for_response(
        &self,
        operation: &'static str,
        status: StatusCode,
        headers: &HeaderMap,
    ) -> Result<(BearerChallenge, HeaderMap), OciError> {
        if status != StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                status,
                "unexpected authentication response",
            ));
        }
        let challenge = parse_www_authenticate(headers)?.ok_or_else(|| {
            Self::request_auth_error(
                operation,
                status,
                "registry returned 401 without a Bearer challenge",
            )
        })?;
        let token = self
            .token_manager
            .refresh_token(&self.transport, &challenge)
            .await?;
        let request_headers = Self::request_with_bearer(self.get_auth_headers().await?, &token);
        Ok((challenge, request_headers))
    }

    pub(super) async fn request_get_with_auth_retry(
        &self,
        url: &str,
        operation: &'static str,
        max_bytes: u64,
    ) -> Result<(StatusCode, HeaderMap, Bytes), OciError> {
        let headers = self.get_auth_headers().await?;
        let first = self.transport.get(url, headers, max_bytes).await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let (_, retry_headers) = self
            .request_challenge_for_response(operation, first.0, &first.1)
            .await?;
        let second = self.transport.get(url, retry_headers, max_bytes).await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second)
    }

    pub(super) async fn request_head_with_auth_retry(
        &self,
        url: &str,
        operation: &'static str,
    ) -> Result<(StatusCode, HeaderMap), OciError> {
        let headers = self.get_auth_headers().await?;
        let first = self.transport.head_with_headers(url, headers).await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let (_, retry_headers) = self
            .request_challenge_for_response(operation, first.0, &first.1)
            .await?;
        let second = self.transport.head_with_headers(url, retry_headers).await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second)
    }

    pub(super) async fn request_post_with_auth_retry(
        &self,
        url: &str,
        operation: &'static str,
    ) -> Result<(StatusCode, HeaderMap), OciError> {
        let headers = self.get_auth_headers().await?;
        let first = self.transport.post(url, headers).await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let (_, retry_headers) = self
            .request_challenge_for_response(operation, first.0, &first.1)
            .await?;
        let second = self.transport.post(url, retry_headers).await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second)
    }

    pub(super) async fn request_post_bytes_with_auth_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
        operation: &'static str,
    ) -> Result<StatusCode, OciError> {
        let first = self
            .transport
            .post_bytes(url, headers.clone(), body.clone())
            .await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first.0);
        }
        let (_, mut retry_headers) = self
            .request_challenge_for_response(operation, first.0, &first.1)
            .await?;
        for (name, value) in &headers {
            if name.as_str() != "authorization" {
                retry_headers.insert(name.clone(), value.clone());
            }
        }
        let second = self.transport.post_bytes(url, retry_headers, body).await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second.0)
    }

    pub(super) async fn request_put_bytes_with_auth_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
        operation: &'static str,
    ) -> Result<StatusCode, OciError> {
        let first = self
            .transport
            .put_bytes_with_headers(url, headers.clone(), body.clone())
            .await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first.0);
        }
        let (_, mut retry_headers) = self
            .request_challenge_for_response(operation, first.0, &first.1)
            .await?;
        for (name, value) in &headers {
            if name.as_str() != "authorization" {
                retry_headers.insert(name.clone(), value.clone());
            }
        }
        let second = self
            .transport
            .put_bytes_with_headers(url, retry_headers, body)
            .await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second.0)
    }

    pub(super) async fn request_patch_chunk_with_auth_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        chunk: Bytes,
        byte_range: (u64, u64),
        operation: &'static str,
    ) -> Result<UploadChunkResponse, OciError> {
        let first = self
            .transport
            .patch_chunk(url, headers.clone(), chunk.clone(), byte_range)
            .await?;
        if first.status != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let (_, mut retry_headers) = self
            .request_challenge_for_response(operation, first.status, &first.headers)
            .await?;
        for (name, value) in &headers {
            if name.as_str() != "authorization" {
                retry_headers.insert(name.clone(), value.clone());
            }
        }
        let second = self
            .transport
            .patch_chunk(url, retry_headers, chunk, byte_range)
            .await?;
        if second.status == StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                second.status,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second)
    }

    pub(super) async fn request_put_chunk_finish_with_auth_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        final_chunk: Option<(Bytes, (u64, u64))>,
        operation: &'static str,
    ) -> Result<StatusCode, OciError> {
        let first = self
            .transport
            .put_chunk_finish_with_headers(url, headers.clone(), final_chunk.clone())
            .await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first.0);
        }
        let (_, mut retry_headers) = self
            .request_challenge_for_response(operation, first.0, &first.1)
            .await?;
        for (name, value) in &headers {
            if name.as_str() != "authorization" {
                retry_headers.insert(name.clone(), value.clone());
            }
        }
        let second = self
            .transport
            .put_chunk_finish_with_headers(url, retry_headers, final_chunk)
            .await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second.0)
    }

    pub(super) async fn request_put_stream_with_auth_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        stream: T::BodyStream,
        content_len: u64,
        operation: &'static str,
    ) -> Result<StatusCode, OciError> {
        let (status, _) = self
            .transport
            .put_stream_with_headers(url, headers, stream, content_len)
            .await?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(OciError::AuthenticationNotReplayable { operation });
        }
        Ok(status)
    }

    pub(super) async fn request_delete_with_auth_retry(
        &self,
        url: &str,
        operation: &'static str,
    ) -> Result<StatusCode, OciError> {
        let headers = self.get_auth_headers().await?;
        let first = self.transport.delete_with_headers(url, headers).await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first.0);
        }
        let (_, retry_headers) = self
            .request_challenge_for_response(operation, first.0, &first.1)
            .await?;
        let second = self
            .transport
            .delete_with_headers(url, retry_headers)
            .await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second.0)
    }

    pub(super) async fn request_stream_with_auth_retry(
        &self,
        url: &str,
        operation: &'static str,
        max_bytes: u64,
    ) -> Result<(StatusCode, HeaderMap, T::BodyStream), OciError> {
        let headers = self.get_auth_headers().await?;
        let first = self.transport.stream(url, headers, max_bytes).await?;
        if first.0 != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let (_, retry_headers) = self
            .request_challenge_for_response(operation, first.0, &first.1)
            .await?;
        let second = self.transport.stream(url, retry_headers, max_bytes).await?;
        if second.0 == StatusCode::UNAUTHORIZED {
            return Err(Self::request_auth_error(
                operation,
                second.0,
                "Bearer challenge retry was rejected",
            ));
        }
        Ok(second)
    }
}
