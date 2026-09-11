//! Blob upload orchestration and resumable upload state.

mod state;

use super::endpoint;
use crate::{
    backend::BlobUploadStrategy,
    codec::{DEFAULT_ZSTD_COMPRESSION_LEVEL, IndexCodec},
    error::{OciError, TransportError},
    manifest::EMPTY_CONFIG_DIGEST,
    transport::{HashingStream, OciTransport, parse_range_header},
    upload::UploadConfig,
};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{HeaderValue, StatusCode, header::LOCATION};
use serde::Serialize;
use std::{pin::pin, time::Duration};
use tracing::{info, warn};

use state::{invalid_upload_range, probe_next_offset, response_next_offset, retryable_error};

use super::blob::BlobClient;

impl<'a, T: OciTransport + Clone> BlobClient<'a, T> {
    async fn abort_upload_session(&self, session_url: &str) {
        match self
            .client
            .request_delete_with_auth_retry(session_url, "abort upload session")
            .await
        {
            Ok(status) if status.is_success() || status == StatusCode::NOT_FOUND => {}
            Ok(status) => warn!(
                "Best-effort upload session abort returned unexpected status {}",
                status
            ),
            Err(error) => warn!("Best-effort upload session abort failed: {error}"),
        }
    }

    async fn execute_two_step_put(&self, digest: &str, bytes: Bytes) -> Result<String, OciError> {
        let upload_init_url = endpoint::upload_url(self.client.endpoint(), self.client.repo());
        let (status, response_headers) = self
            .client
            .request_post_with_auth_retry(&upload_init_url, "initialize blob upload")
            .await?;
        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }

        let location = response_headers
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(OciError::UploadLocationMissing)?;
        let session_url = endpoint::resolved_location(self.client.endpoint(), location)?;
        let put_url = endpoint::with_digest(&session_url, digest);
        let mut headers = self.client.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static("application/octet-stream"),
        );

        let result = async {
            let put_status = self
                .client
                .request_put_bytes_with_auth_retry(&put_url, headers, bytes, "upload blob")
                .await?;
            if matches!(
                put_status,
                StatusCode::CREATED | StatusCode::ACCEPTED | StatusCode::OK
            ) {
                info!("Successfully uploaded blob via two-step PUT: {}", digest);
                Ok(digest.to_string())
            } else {
                Err(OciError::BlobUploadFailed(put_status))
            }
        }
        .await;

        if result.is_err() {
            self.abort_upload_session(&session_url).await;
        }
        result
    }

    pub async fn push_bytes_with_digest(
        &self,
        digest: &str,
        bytes: Bytes,
    ) -> Result<String, OciError> {
        if self.head(digest).await? {
            info!("Blob {} already exists, skipping upload.", digest);
            return Ok(digest.to_string());
        }

        match self.client.driver.capabilities().fixed_upload_strategy {
            BlobUploadStrategy::FixedTwoStepPut => self.execute_two_step_put(digest, bytes).await,
            BlobUploadStrategy::PreferMonolithicPost
            | BlobUploadStrategy::ResumableChunkedPatch => {
                let monolithic_url = endpoint::with_digest(
                    &endpoint::upload_url(self.client.endpoint(), self.client.repo()),
                    digest,
                );
                let mut headers = self.client.get_auth_headers().await?;
                headers.insert(
                    "Content-Type",
                    HeaderValue::from_static("application/octet-stream"),
                );

                match self
                    .client
                    .request_post_bytes_with_auth_retry(
                        &monolithic_url,
                        headers,
                        bytes.clone(),
                        "monolithic blob upload",
                    )
                    .await
                {
                    Ok(StatusCode::CREATED | StatusCode::OK) => {
                        info!(
                            "Successfully uploaded blob via 1-RTT Monolithic POST: {}",
                            digest
                        );
                        Ok(digest.to_string())
                    }
                    Ok(status) => {
                        warn!(
                            "Monolithic POST returned status {}, falling back to two-step upload for blob {}",
                            status, digest
                        );
                        self.execute_two_step_put(digest, bytes).await
                    }
                    Err(error) => {
                        warn!(
                            "Monolithic POST failed ({}), falling back to two-step upload for blob {}",
                            error, digest
                        );
                        self.execute_two_step_put(digest, bytes).await
                    }
                }
            }
        }
    }

    pub async fn push_bytes(&self, bytes: Bytes) -> Result<String, OciError> {
        let digest = endpoint::compute_sha256_digest(&bytes);
        self.push_bytes_with_digest(&digest, bytes).await
    }

    pub async fn ensure_empty_config(&self) -> Result<(), OciError> {
        if !self.head(EMPTY_CONFIG_DIGEST).await? {
            self.push_bytes_with_digest(EMPTY_CONFIG_DIGEST, Bytes::from_static(b"{}"))
                .await?;
        }
        Ok(())
    }

    pub async fn push_stream(
        &self,
        digest: &str,
        stream: T::BodyStream,
        content_len: u64,
    ) -> Result<String, OciError> {
        if self.head(digest).await? {
            info!("Blob {} already exists, skipping upload.", digest);
            return Ok(digest.to_string());
        }

        let upload_init_url = endpoint::upload_url(self.client.endpoint(), self.client.repo());
        let (status, response_headers) = self
            .client
            .request_post_with_auth_retry(&upload_init_url, "initialize blob stream upload")
            .await?;
        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }
        let location = response_headers
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(OciError::UploadLocationMissing)?;
        let session_url = endpoint::resolved_location(self.client.endpoint(), location)?;
        let put_url = endpoint::with_digest(&session_url, digest);
        let mut headers = self.client.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static("application/octet-stream"),
        );

        let result = async {
            let put_status = self
                .client
                .request_put_stream_with_auth_retry(
                    &put_url,
                    headers,
                    stream,
                    content_len,
                    "upload blob stream",
                )
                .await?;
            if matches!(
                put_status,
                StatusCode::CREATED | StatusCode::ACCEPTED | StatusCode::OK
            ) {
                info!("Successfully uploaded blob stream: {}", digest);
                Ok(digest.to_string())
            } else {
                Err(OciError::BlobUploadFailed(put_status))
            }
        }
        .await;

        if result.is_err() {
            self.abort_upload_session(&session_url).await;
        }
        result
    }

    async fn probe_upload_session(
        &self,
        session_url: &str,
    ) -> Result<(StatusCode, http::HeaderMap, Option<(u64, u64)>), OciError> {
        let (status, headers, _) = self
            .client
            .request_get_with_auth_retry(
                session_url,
                "probe upload session",
                self.client.limits().max_manifest_bytes(),
            )
            .await?;
        let range = if let Some(value) = headers.get(http::header::RANGE) {
            let text = value
                .to_str()
                .map_err(|_| TransportError::HeaderParse { header: "Range" })?;
            Some(parse_range_header(text).ok_or_else(|| {
                invalid_upload_range(format!("cannot parse Range header '{text}'"))
            })?)
        } else {
            None
        };
        Ok((status, headers, range))
    }

    async fn upload_chunk_with_retry(
        &self,
        session_url: &mut String,
        chunk: &Bytes,
        chunk_start: u64,
        config: &UploadConfig,
    ) -> Result<(), OciError> {
        let chunk_end = chunk_start
            .checked_add(chunk.len() as u64)
            .and_then(|end| end.checked_sub(1))
            .ok_or_else(|| invalid_upload_range("chunk end offset overflowed"))?;
        let mut remote_next = chunk_start;
        let mut retry_count = 0usize;
        let mut patch_attempts = 0usize;
        let mut last_error: OciError;
        let mut probed_416 = false;
        let max_patch_attempts = config.max_retry_attempts.saturating_add(1);

        loop {
            if remote_next > chunk_end {
                return Ok(());
            }
            if patch_attempts >= max_patch_attempts {
                return Err(OciError::ResumableUploadFailed {
                    attempts: patch_attempts,
                    source: Box::new(invalid_upload_range(
                        "retry budget exhausted before the logical chunk completed",
                    )),
                });
            }

            let relative_start = (remote_next - chunk_start) as usize;
            let body = chunk.slice(relative_start..);
            let byte_range = (remote_next, chunk_end);
            let headers = self.client.get_auth_headers().await?;
            patch_attempts += 1;

            match self
                .client
                .request_patch_chunk_with_auth_retry(
                    session_url,
                    headers,
                    body,
                    byte_range,
                    "upload blob chunk",
                )
                .await
            {
                Ok(response) => {
                    endpoint::update_location(
                        self.client.endpoint(),
                        session_url,
                        &response.headers,
                    )?;
                    if let Some(location) = response.location.as_deref() {
                        *session_url =
                            endpoint::resolved_location(self.client.endpoint(), location)?;
                    }

                    if response.status.is_success() {
                        remote_next = response_next_offset(
                            response.range,
                            chunk_start,
                            byte_range.0,
                            chunk_end,
                        )?;
                        continue;
                    }

                    let status_error = OciError::BlobUploadFailed(response.status);
                    let is_416 = response.status == StatusCode::RANGE_NOT_SATISFIABLE;
                    if !is_416 && !retryable_error(&status_error) {
                        return Err(status_error);
                    }
                    last_error = status_error;

                    if is_416 {
                        if probed_416 {
                            return Err(last_error);
                        }
                        probed_416 = true;
                    } else if retry_count >= config.max_retry_attempts {
                        return Err(OciError::ResumableUploadFailed {
                            attempts: patch_attempts,
                            source: Box::new(last_error),
                        });
                    }
                }
                Err(error) => {
                    if !retryable_error(&error) {
                        return Err(error);
                    }
                    last_error = error;
                    if retry_count >= config.max_retry_attempts {
                        return Err(OciError::ResumableUploadFailed {
                            attempts: patch_attempts,
                            source: Box::new(last_error),
                        });
                    }
                }
            }

            let (probe_status, probe_headers, probe_range) = self
                .probe_upload_session(session_url)
                .await
                .map_err(|probe_error| OciError::ResumableUploadFailed {
                    attempts: patch_attempts,
                    source: Box::new(probe_error),
                })?;
            endpoint::update_location(self.client.endpoint(), session_url, &probe_headers)?;
            if !probe_status.is_success() {
                if probed_416 {
                    return Err(last_error);
                }
                return Err(OciError::ResumableUploadFailed {
                    attempts: patch_attempts,
                    source: Box::new(OciError::BlobUploadFailed(probe_status)),
                });
            }
            if probed_416 && probe_range.is_none() {
                return Err(last_error);
            }

            if let Some(next) = probe_next_offset(probe_range, chunk_start, remote_next, chunk_end)?
            {
                remote_next = next;
                if remote_next > chunk_end {
                    return Ok(());
                }
            }
            if retry_count >= config.max_retry_attempts {
                return Err(OciError::ResumableUploadFailed {
                    attempts: patch_attempts,
                    source: Box::new(last_error),
                });
            }
            retry_count += 1;
            let backoff_exponent = retry_count.saturating_sub(1).min(5) as u32;
            let backoff_ms = 100u64.saturating_mul(1u64 << backoff_exponent);
            self.client
                .transport
                .sleep(Duration::from_millis(backoff_ms))
                .await;
        }
    }

    pub async fn push_resumable(
        &self,
        stream: T::BodyStream,
        config: &UploadConfig,
    ) -> Result<(String, u64), OciError> {
        let (hashing_stream, hash_state) = HashingStream::new(stream);
        let mut pinned_stream = pin!(hashing_stream);
        let capabilities = self.client.driver.capabilities();
        let strategy = capabilities.fixed_upload_strategy;
        info!(
            "Initiating deterministic streaming upload (backend: {:?}, strategy: {:?})",
            self.client.driver.kind(),
            strategy
        );

        let chunk_limit = config.chunk_size_bytes.max(1024 * 1024);
        let threshold = config.chunk_threshold_bytes.max(chunk_limit as u64) as usize;
        let mut buffer = BytesMut::with_capacity(threshold.max(chunk_limit * 2));
        let allow_chunked = capabilities.supports_chunked_patch
            && strategy == BlobUploadStrategy::ResumableChunkedPatch;

        while let Some(item) = pinned_stream.next().await {
            let bytes: Bytes = item?;
            buffer.extend_from_slice(&bytes);
            if buffer.len() >= threshold && allow_chunked {
                break;
            }
        }

        if buffer.len() < threshold || !allow_chunked {
            while let Some(item) = pinned_stream.next().await {
                let bytes: Bytes = item?;
                buffer.extend_from_slice(&bytes);
            }
            let final_digest = hash_state.force_finalize();
            let total_size = hash_state.bytes_streamed();
            if self.head(&final_digest).await? {
                info!("Blob {} already exists, skipping upload.", final_digest);
                return Ok((final_digest, total_size));
            }
            let pushed_digest = self
                .push_bytes_with_digest(&final_digest, buffer.freeze())
                .await?;
            info!(
                "Successfully uploaded streaming blob {} ({} bytes)",
                pushed_digest, total_size
            );
            return Ok((pushed_digest, total_size));
        }

        info!("Executing standard chunked resumable upload for large stream");
        let upload_init_url = endpoint::upload_url(self.client.endpoint(), self.client.repo());
        let (status, response_headers) = self
            .client
            .request_post_with_auth_retry(&upload_init_url, "initialize chunked upload")
            .await?;
        if !status.is_success() {
            return Err(OciError::BlobUploadFailed(status));
        }
        let location = response_headers
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(OciError::UploadLocationMissing)?;
        let mut session_url = endpoint::resolved_location(self.client.endpoint(), location)?;
        let mut current_offset = 0u64;
        let mut chunk_buf = buffer;
        let mut stream_ended = false;

        let upload_result = async {
            while !stream_ended {
                while chunk_buf.len() < chunk_limit {
                    if let Some(item) = pinned_stream.next().await {
                        let bytes: Bytes = item?;
                        chunk_buf.extend_from_slice(&bytes);
                    } else {
                        stream_ended = true;
                        break;
                    }
                }
                if chunk_buf.is_empty() {
                    break;
                }
                let send_len = if stream_ended {
                    chunk_buf.len()
                } else {
                    chunk_limit.min(chunk_buf.len())
                };
                let chunk_bytes = chunk_buf.split_to(send_len).freeze();
                self.upload_chunk_with_retry(
                    &mut session_url,
                    &chunk_bytes,
                    current_offset,
                    config,
                )
                .await?;
                current_offset += chunk_bytes.len() as u64;
            }

            let final_digest = hash_state.force_finalize();
            let total_size = hash_state.bytes_streamed();
            let finish_url = endpoint::with_digest(&session_url, &final_digest);
            let headers = self.client.get_auth_headers().await?;
            let finish_status = self
                .client
                .request_put_chunk_finish_with_auth_retry(
                    &finish_url,
                    headers,
                    None,
                    "finish blob upload",
                )
                .await?;
            if matches!(
                finish_status,
                StatusCode::CREATED | StatusCode::OK | StatusCode::ACCEPTED
            ) {
                info!(
                    "Successfully committed streaming blob {} ({} bytes)",
                    final_digest, total_size
                );
                Ok((final_digest, total_size))
            } else {
                Err(OciError::BlobUploadFailed(finish_status))
            }
        }
        .await;

        if upload_result.is_err() {
            self.abort_upload_session(&session_url).await;
        }
        upload_result
    }

    pub async fn push_zstd<S: Serialize>(&self, data: &S) -> Result<(String, u64, u64), OciError> {
        let raw_json = serde_json::to_vec(data)?;
        let uncompressed_size = raw_json.len() as u64;
        let compressed_bytes = IndexCodec::encode_zstd(data, DEFAULT_ZSTD_COMPRESSION_LEVEL)?;
        let compressed_size = compressed_bytes.len() as u64;
        let digest = endpoint::compute_sha256_digest(&compressed_bytes);
        if self.head(&digest).await? {
            return Ok((digest, compressed_size, uncompressed_size));
        }
        let pushed_digest = self
            .push_bytes_with_digest(&digest, compressed_bytes)
            .await?;
        Ok((pushed_digest, compressed_size, uncompressed_size))
    }
}
