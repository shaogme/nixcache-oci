use super::{OciClient, endpoint};
use crate::{
    backend::ManifestCasSupport,
    error::{OciError, TransportError},
    integrity::{ContentDigest, verify_buffered_body},
    manifest::{EMPTY_CONFIG_DIGEST, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest},
    transport::{OciTransport, parse_content_length},
};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header::IF_MATCH};
use serde::{Deserialize, Deserializer};
use std::str::from_utf8;
use tracing::info;

/// Manifest 发布时使用的 CAS 前置条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestCasCondition {
    CreateOnly,
    Match(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedOciArtifact {
    pub manifest: OciArtifactManifest,
    pub digest: String,
}

#[derive(Deserialize)]
struct OciTagsListResponse {
    #[serde(deserialize_with = "deserialize_nullable_tags")]
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagList {
    pub tags: Vec<String>,
    pub pages_fetched: usize,
}

fn deserialize_nullable_tags<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Manifest 和 Tag 领域客户端。
pub struct ManifestClient<'a, T: OciTransport> {
    client: &'a OciClient<T>,
}

impl<'a, T: OciTransport + Clone> ManifestClient<'a, T> {
    pub(super) fn new(client: &'a OciClient<T>) -> Self {
        Self { client }
    }

    pub async fn get_with_digest(&self, tag: &str) -> Result<Option<(String, String)>, OciError> {
        self.get_with_expected_size(tag, None).await
    }

    /// descriptor-aware manifest GET，用于校验 image index 中声明的 manifest size。
    pub async fn get_with_digest_and_size(
        &self,
        reference: &str,
        expected_size: u64,
    ) -> Result<Option<(String, String)>, OciError> {
        self.get_with_expected_size(reference, Some(expected_size))
            .await
    }

    async fn get_with_expected_size(
        &self,
        tag: &str,
        expected_size: Option<u64>,
    ) -> Result<Option<(String, String)>, OciError> {
        if let Some(expected_size) = expected_size
            && expected_size > self.client.limits().max_manifest_bytes()
        {
            return Err(OciError::SizeLimitExceeded {
                target: tag.to_string(),
                limit: self.client.limits().max_manifest_bytes(),
                actual: expected_size,
            });
        }
        let _permit = self.client.acquire_index_read().await;
        let url = endpoint::manifest_url(self.client.endpoint(), self.client.repo(), tag);
        let (status, response_headers, bytes) = self
            .client
            .request_get_with_auth_retry(
                &url,
                "get manifest",
                self.client.limits().max_manifest_bytes(),
            )
            .await?;

        if status == StatusCode::OK {
            let request_digest = tag
                .starts_with("sha256:")
                .then(|| ContentDigest::parse(tag))
                .transpose()?;
            let content_length =
                parse_content_length(&response_headers).map_err(OciError::Transport)?;
            let digest = verify_buffered_body(
                &url,
                request_digest.as_ref().map(ContentDigest::as_str),
                response_headers.get("Docker-Content-Digest"),
                expected_size,
                content_length,
                &bytes,
            )?;
            let body = from_utf8(&bytes)?;
            Ok(Some((body.to_string(), digest.to_string())))
        } else if status == StatusCode::NOT_FOUND {
            Ok(None)
        } else {
            Err(OciError::ManifestFetchFailed(status))
        }
    }

    pub async fn get(&self, tag: &str) -> Result<Option<String>, OciError> {
        self.get_with_digest(tag)
            .await
            .map(|manifest| manifest.map(|(body, _)| body))
    }

    pub async fn head(&self, tag: &str) -> Result<Option<String>, OciError> {
        let url = endpoint::manifest_url(self.client.endpoint(), self.client.repo(), tag);
        let (status, response_headers) = self
            .client
            .request_head_with_auth_retry(&url, "head manifest")
            .await?;
        if status == StatusCode::OK {
            Ok(response_headers
                .get("Docker-Content-Digest")
                .or_else(|| response_headers.get("ETag"))
                .and_then(|value| value.to_str().ok())
                .map(|value| {
                    value
                        .trim_matches('"')
                        .trim_start_matches("W/\"")
                        .trim_end_matches('"')
                        .to_string()
                }))
        } else if status == StatusCode::NOT_FOUND {
            Ok(None)
        } else {
            Err(OciError::ManifestFetchFailed(status))
        }
    }

    pub async fn list_tags(&self) -> Result<TagList, OciError> {
        let mut all_tags = Vec::new();
        let mut last_tag: Option<String> = None;
        let mut pages_fetched = 0usize;
        const PAGE_SIZE: usize = 100;
        const MAX_TAG_PAGES: usize = 10_000;
        const MAX_TAGS: usize = 2_000_000;
        loop {
            if pages_fetched >= MAX_TAG_PAGES {
                return Err(OciError::PaginationFailed {
                    repository: self.client.repo().to_string(),
                    cursor: last_tag.clone(),
                    pages_fetched,
                    collected_tags: all_tags.len(),
                    details: format!("tag pagination exceeded the {MAX_TAG_PAGES}-page limit"),
                });
            }
            let base_url = endpoint::tags_url(self.client.endpoint(), self.client.repo());
            let url = match last_tag.as_deref() {
                Some(cursor) => endpoint::with_last_cursor(&base_url, cursor),
                None => format!("{base_url}?n=100"),
            };
            let _permit = self.client.acquire_index_read().await;
            let (status, _, body) = self
                .client
                .request_get_with_auth_retry(
                    &url,
                    "list tags",
                    self.client.limits().max_manifest_bytes(),
                )
                .await
                .map_err(|error| OciError::PaginationFailed {
                    repository: self.client.repo().to_string(),
                    cursor: last_tag.clone(),
                    pages_fetched,
                    collected_tags: all_tags.len(),
                    details: error.to_string(),
                })?;
            if !status.is_success() {
                return Err(OciError::PaginationFailed {
                    repository: self.client.repo().to_string(),
                    cursor: last_tag.clone(),
                    pages_fetched,
                    collected_tags: all_tags.len(),
                    details: format!("HTTP {status} while requesting {url}"),
                });
            }
            let response: OciTagsListResponse =
                serde_json::from_slice(&body).map_err(|error| OciError::PaginationFailed {
                    repository: self.client.repo().to_string(),
                    cursor: last_tag.clone(),
                    pages_fetched,
                    collected_tags: all_tags.len(),
                    details: error.to_string(),
                })?;
            pages_fetched += 1;
            if response.tags.is_empty() {
                if last_tag.is_none() {
                    return Ok(TagList {
                        tags: all_tags,
                        pages_fetched,
                    });
                }
                return Err(OciError::PaginationFailed {
                    repository: self.client.repo().to_string(),
                    cursor: last_tag.clone(),
                    pages_fetched,
                    collected_tags: all_tags.len(),
                    details: "registry returned an empty page after a full page".to_string(),
                });
            }
            let count = response.tags.len();
            let new_last = response.tags.last().cloned();
            if all_tags.len().saturating_add(count) > MAX_TAGS {
                return Err(OciError::PaginationFailed {
                    repository: self.client.repo().to_string(),
                    cursor: last_tag.clone(),
                    pages_fetched,
                    collected_tags: all_tags.len(),
                    details: format!("tag pagination exceeded the {MAX_TAGS}-tag limit"),
                });
            }
            let previous_count = all_tags.len();
            all_tags.extend(response.tags);
            all_tags.sort();
            all_tags.dedup();
            if last_tag.is_some() && all_tags.len() == previous_count {
                return Err(OciError::PaginationFailed {
                    repository: self.client.repo().to_string(),
                    cursor: last_tag.clone(),
                    pages_fetched,
                    collected_tags: all_tags.len(),
                    details: "registry returned a duplicate page with no new tags".to_string(),
                });
            }
            if count < PAGE_SIZE {
                break;
            }
            if new_last == last_tag || new_last.is_none() {
                return Err(OciError::PaginationFailed {
                    repository: self.client.repo().to_string(),
                    cursor: last_tag.clone(),
                    pages_fetched,
                    collected_tags: all_tags.len(),
                    details: "registry returned a non-advancing pagination cursor".to_string(),
                });
            }
            last_tag = new_last;
        }
        Ok(TagList {
            tags: all_tags,
            pages_fetched,
        })
    }

    pub async fn fetch_artifact(
        &self,
        tag_or_digest: &str,
    ) -> Result<Option<FetchedOciArtifact>, OciError> {
        let Some((body, digest)) = self.get_with_digest(tag_or_digest).await? else {
            return Ok(None);
        };
        let manifest = serde_json::from_str::<OciArtifactManifest>(&body)?;
        manifest.validate_for(self.client.limits(), tag_or_digest)?;
        Ok(Some(FetchedOciArtifact { manifest, digest }))
    }

    pub async fn put(&self, tag: &str, manifest: &str) -> Result<(), OciError> {
        if manifest.contains(EMPTY_CONFIG_DIGEST) {
            self.client.blobs().ensure_empty_config().await?;
        }
        let url = endpoint::manifest_url(self.client.endpoint(), self.client.repo(), tag);
        let mut headers = self.client.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static(OCI_IMAGE_MANIFEST_MEDIA_TYPE),
        );
        let status = self
            .client
            .request_put_bytes_with_auth_retry(
                &url,
                headers,
                Bytes::copy_from_slice(manifest.as_bytes()),
                "push manifest",
            )
            .await?;
        if matches!(
            status,
            StatusCode::OK | StatusCode::CREATED | StatusCode::ACCEPTED
        ) {
            info!("Successfully pushed manifest for tag {}", tag);
            Ok(())
        } else {
            Err(OciError::ManifestPushFailed(status))
        }
    }

    pub async fn put_cas(
        &self,
        tag: &str,
        manifest: &str,
        condition: ManifestCasCondition,
    ) -> Result<(), OciError> {
        self.ensure_cas_supported(tag)?;
        if manifest.contains(EMPTY_CONFIG_DIGEST) {
            self.client.blobs().ensure_empty_config().await?;
        }
        let url = endpoint::manifest_url(self.client.endpoint(), self.client.repo(), tag);
        let mut headers = self.client.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static(OCI_IMAGE_MANIFEST_MEDIA_TYPE),
        );
        let expected = insert_cas_condition(&mut headers, &condition)?;
        let status = self
            .client
            .request_put_bytes_with_auth_retry(
                &url,
                headers,
                Bytes::copy_from_slice(manifest.as_bytes()),
                "push manifest with CAS",
            )
            .await?;
        if matches!(
            status,
            StatusCode::PRECONDITION_FAILED | StatusCode::CONFLICT
        ) {
            return Err(OciError::CasPreconditionFailed {
                tag: tag.to_string(),
                expected,
                actual: None,
            });
        }
        if matches!(
            status,
            StatusCode::OK | StatusCode::CREATED | StatusCode::ACCEPTED
        ) {
            info!("Successfully pushed manifest for tag {}", tag);
            Ok(())
        } else {
            Err(OciError::ManifestPushFailed(status))
        }
    }

    fn ensure_cas_supported(&self, tag: &str) -> Result<(), OciError> {
        if self.client.capabilities().manifest_cas_support == ManifestCasSupport::IfMatch {
            Ok(())
        } else {
            Err(OciError::CasUnsupported {
                tag: tag.to_string(),
                backend: self.client.kind(),
            })
        }
    }
}

fn insert_cas_condition(
    headers: &mut HeaderMap,
    condition: &ManifestCasCondition,
) -> Result<Option<String>, OciError> {
    match condition {
        ManifestCasCondition::CreateOnly => {
            headers.insert("If-None-Match", HeaderValue::from_static("*"));
            Ok(None)
        }
        ManifestCasCondition::Match(expected) => {
            let value = HeaderValue::from_str(expected)
                .map_err(|_| TransportError::HeaderParse { header: "If-Match" })?;
            headers.insert(IF_MATCH, value);
            Ok(Some(expected.clone()))
        }
    }
}
