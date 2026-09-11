use super::{OciClient, endpoint};
use crate::{
    error::OciError,
    integrity::{ContentDigest, verify_buffered_body},
    manifest::OciDescriptor,
    transport::{OciBlobStream, OciTransport, VerifiedBlobStream, parse_content_length},
};
use bytes::Bytes;

/// Blob 读取和上传领域客户端。
pub struct BlobClient<'a, T: OciTransport> {
    pub(super) client: &'a OciClient<T>,
}

impl<'a, T: OciTransport + Clone> BlobClient<'a, T> {
    pub(super) fn new(client: &'a OciClient<T>) -> Self {
        Self { client }
    }

    pub async fn head(&self, digest: &str) -> Result<bool, OciError> {
        let url = endpoint::blob_url(self.client.endpoint(), self.client.repo(), digest);
        let (status, _) = self
            .client
            .request_head_with_auth_retry(&url, "head blob")
            .await?;

        if status == http::StatusCode::OK {
            Ok(true)
        } else if status == http::StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            Err(OciError::BlobCheckFailed(status))
        }
    }

    pub async fn get(&self, digest: &str) -> Result<Bytes, OciError> {
        self.get_verified(digest, None).await
    }

    /// 按完整 OCI descriptor 读取 blob；descriptor 的 size 是完整性上下文的一部分。
    pub async fn get_descriptor(&self, descriptor: &OciDescriptor) -> Result<Bytes, OciError> {
        descriptor.validate_for(
            "blob descriptor",
            self.client.limits().max_buffered_blob_bytes(),
        )?;
        self.get_verified(&descriptor.digest, Some(descriptor.size))
            .await
    }

    pub async fn get_with_descriptor(&self, descriptor: &OciDescriptor) -> Result<Bytes, OciError> {
        self.get_descriptor(descriptor).await
    }

    async fn get_verified(
        &self,
        digest: &str,
        expected_size: Option<u64>,
    ) -> Result<Bytes, OciError> {
        let _permit = self.client.acquire_index_read().await;
        ContentDigest::parse(digest)?;
        let url = endpoint::blob_url(self.client.endpoint(), self.client.repo(), digest);
        let (status, response_headers, bytes) = self
            .client
            .request_get_with_auth_retry(
                &url,
                "get blob",
                self.client.limits().max_buffered_blob_bytes(),
            )
            .await?;

        if status.is_success() {
            let content_length =
                parse_content_length(&response_headers).map_err(OciError::Transport)?;
            verify_buffered_body(
                &url,
                Some(digest),
                response_headers.get("Docker-Content-Digest"),
                expected_size,
                content_length,
                &bytes,
            )?;
            Ok(bytes)
        } else if status == http::StatusCode::NOT_FOUND {
            Err(OciError::BlobNotFound {
                digest: digest.to_string(),
            })
        } else {
            Err(OciError::BlobDownloadFailed(status))
        }
    }

    pub async fn stream(
        &self,
        digest: &str,
    ) -> Result<OciBlobStream<VerifiedBlobStream<T::BodyStream>>, OciError> {
        ContentDigest::parse(digest)?;
        let url = endpoint::blob_url(self.client.endpoint(), self.client.repo(), digest);
        let (status, headers, stream) = self
            .client
            .request_stream_with_auth_retry(
                &url,
                "stream blob",
                self.client.limits().max_streamed_blob_bytes(),
            )
            .await?;

        if status.is_success() {
            let verified = VerifiedBlobStream::new(
                stream,
                &url,
                digest,
                &headers,
                self.client.limits().max_streamed_blob_bytes(),
            )?;
            Ok(OciBlobStream::new(status, headers, verified))
        } else if status == http::StatusCode::NOT_FOUND {
            Err(OciError::BlobNotFound {
                digest: digest.to_string(),
            })
        } else {
            Err(OciError::BlobDownloadFailed(status))
        }
    }
}
