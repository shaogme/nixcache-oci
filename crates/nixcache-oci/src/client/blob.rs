use super::{OciClient, endpoint};
use crate::{
    error::OciError,
    transport::{OciBlobStream, OciTransport},
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
        let url = endpoint::blob_url(
            self.client.url_scheme(),
            self.client.registry(),
            self.client.repo(),
            digest,
        );
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
        let url = endpoint::blob_url(
            self.client.url_scheme(),
            self.client.registry(),
            self.client.repo(),
            digest,
        );
        let (status, _, bytes) = self
            .client
            .request_get_with_auth_retry(&url, "get blob")
            .await?;

        if status.is_success() {
            Ok(bytes)
        } else if status == http::StatusCode::NOT_FOUND {
            Err(OciError::BlobNotFound {
                digest: digest.to_string(),
            })
        } else {
            Err(OciError::BlobDownloadFailed(status))
        }
    }

    pub async fn stream(&self, digest: &str) -> Result<OciBlobStream<T::BodyStream>, OciError> {
        let url = endpoint::blob_url(
            self.client.url_scheme(),
            self.client.registry(),
            self.client.repo(),
            digest,
        );
        let (status, headers, stream) = self
            .client
            .request_stream_with_auth_retry(&url, "stream blob")
            .await?;

        if status.is_success() {
            Ok(OciBlobStream::new(status, headers, stream))
        } else if status == http::StatusCode::NOT_FOUND {
            Err(OciError::BlobNotFound {
                digest: digest.to_string(),
            })
        } else {
            Err(OciError::BlobDownloadFailed(status))
        }
    }
}
