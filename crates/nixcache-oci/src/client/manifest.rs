use super::{OciClient, endpoint};
use crate::{
    backend::ManifestCasSupport,
    error::{OciError, TransportError},
    manifest::{EMPTY_CONFIG_DIGEST, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest},
    transport::OciTransport,
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
        let url = endpoint::manifest_url(
            self.client.url_scheme(),
            self.client.registry(),
            self.client.repo(),
            tag,
        );
        let (status, response_headers, bytes) = self
            .client
            .request_get_with_auth_retry(&url, "get manifest")
            .await?;

        if status == StatusCode::OK {
            let digest_header = response_headers
                .get("Docker-Content-Digest")
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_string)
                        .map_err(|_| TransportError::HeaderParse {
                            header: "Docker-Content-Digest",
                        })
                })
                .transpose()?;
            let body = from_utf8(&bytes)?;
            let digest = digest_header.unwrap_or_else(|| endpoint::compute_sha256_digest(&bytes));
            Ok(Some((body.to_string(), digest)))
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
        let url = endpoint::manifest_url(
            self.client.url_scheme(),
            self.client.registry(),
            self.client.repo(),
            tag,
        );
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

    pub async fn list_tags(&self) -> Result<Vec<String>, OciError> {
        let mut all_tags = Vec::new();
        let mut last_tag: Option<String> = None;
        loop {
            let base_url = endpoint::tags_url(
                self.client.url_scheme(),
                self.client.registry(),
                self.client.repo(),
            );
            let url = match last_tag.as_deref() {
                Some(cursor) => endpoint::with_last_cursor(&base_url, cursor),
                None => format!("{base_url}?n=100"),
            };
            let (status, _, body) = self
                .client
                .request_get_with_auth_retry(&url, "list tags")
                .await?;
            if status == StatusCode::NOT_FOUND {
                if !all_tags.is_empty() {
                    return Err(OciError::DeletionDiscoveryFailed {
                        stage: "list_tags",
                        target: self.client.repo().to_string(),
                        details: "registry returned 404 after a partial tag listing".to_string(),
                    });
                }
                all_tags.sort();
                all_tags.dedup();
                return Ok(all_tags);
            }
            if !status.is_success() {
                if last_tag.is_none() {
                    let (plain_status, _, plain_body) = self
                        .client
                        .request_get_with_auth_retry(&base_url, "list tags fallback")
                        .await?;
                    if plain_status.is_success() {
                        let response = serde_json::from_slice::<OciTagsListResponse>(&plain_body)
                            .map_err(OciError::Json)?;
                        all_tags.extend(response.tags);
                        all_tags.sort();
                        all_tags.dedup();
                        return Ok(all_tags);
                    }
                }
                return Err(OciError::Transport(TransportError::HttpStatus {
                    status,
                    message: Some(format!("Failed to list tags from {url}")),
                }));
            }
            let response: OciTagsListResponse = serde_json::from_slice(&body)?;
            if response.tags.is_empty() {
                break;
            }
            let count = response.tags.len();
            let new_last = response.tags.last().cloned();
            all_tags.extend(response.tags);
            if count < 100 {
                break;
            }
            if new_last == last_tag || new_last.is_none() {
                return Err(OciError::DeletionDiscoveryFailed {
                    stage: "list_tags",
                    target: self.client.repo().to_string(),
                    details: "registry returned a non-advancing pagination cursor".to_string(),
                });
            }
            last_tag = new_last;
        }
        all_tags.sort();
        all_tags.dedup();
        Ok(all_tags)
    }

    pub async fn fetch_artifact(
        &self,
        tag_or_digest: &str,
    ) -> Result<Option<FetchedOciArtifact>, OciError> {
        let Some((body, digest)) = self.get_with_digest(tag_or_digest).await? else {
            return Ok(None);
        };
        let manifest = serde_json::from_str::<OciArtifactManifest>(&body)?;
        Ok(Some(FetchedOciArtifact { manifest, digest }))
    }

    pub async fn put(&self, tag: &str, manifest: &str) -> Result<(), OciError> {
        if manifest.contains(EMPTY_CONFIG_DIGEST) {
            self.client.blobs().ensure_empty_config().await?;
        }
        let url = endpoint::manifest_url(
            self.client.url_scheme(),
            self.client.registry(),
            self.client.repo(),
            tag,
        );
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
        let url = endpoint::manifest_url(
            self.client.url_scheme(),
            self.client.registry(),
            self.client.repo(),
            tag,
        );
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
