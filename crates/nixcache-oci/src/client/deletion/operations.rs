use super::{DeletionClient, DeletionOutcome, DeletionSummary, PackageDeletionSummary};
use crate::{
    backend::{GitHubPackagesClient, PackageDeletionSupport, RegistryDeletionStrategy},
    client::endpoint,
    error::OciError,
    integrity::verify_buffered_body,
    transport::{OciTransport, parse_content_length},
};
use futures_util::StreamExt;
use http::StatusCode;
use nixcache_core::NarDigest;
use tracing::warn;

impl<'a, T: OciTransport + Clone> DeletionClient<'a, T> {
    pub(super) async fn delete_tag_strict(&self, tag: &str) -> Result<(), OciError> {
        match self.client.capabilities().deletion_strategy {
            RegistryDeletionStrategy::GitHubPackagesRestApi => {
                self.ghcr()?.delete_by_tag(tag).await
            }
            RegistryDeletionStrategy::StandardOciDelete
            | RegistryDeletionStrategy::DockerHubRestApi
            | RegistryDeletionStrategy::AwsEcrApi => {
                let url = endpoint::manifest_url(self.client.endpoint(), self.client.repo(), tag);
                let (status, headers, body) = self
                    .client
                    .request_get_with_auth_retry(
                        &url,
                        "get tag manifest",
                        self.client.limits().max_manifest_bytes(),
                    )
                    .await?;
                if status == StatusCode::NOT_FOUND {
                    return Ok(());
                }
                if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                    return Err(OciError::InsufficientPermission {
                        target: tag.to_string(),
                        required_scope: "pull,delete:manifest",
                        details: format!("HTTP {} when checking manifest tag {}", status, tag),
                    });
                }
                if !status.is_success() {
                    return Err(OciError::DeletionFailed {
                        target: tag.to_string(),
                        status,
                        details: format!("HTTP {} when retrieving tag manifest {}", status, tag),
                    });
                }
                let content_length = parse_content_length(&headers).map_err(OciError::Transport)?;
                let digest = verify_buffered_body(
                    &url,
                    None,
                    headers.get("Docker-Content-Digest"),
                    None,
                    content_length,
                    &body,
                )?
                .to_string();
                self.delete_manifest_strict(&digest).await?;

                let tag_status = self
                    .client
                    .request_delete_with_auth_retry(&url, "delete tag")
                    .await?;
                if tag_status != StatusCode::NOT_FOUND
                    && !tag_status.is_success()
                    && tag_status != StatusCode::ACCEPTED
                {
                    return Err(OciError::DeletionFailed {
                        target: tag.to_string(),
                        status: tag_status,
                        details: "registry rejected tag deletion".to_string(),
                    });
                }
                Ok(())
            }
            RegistryDeletionStrategy::Unsupported => Err(OciError::OperationNotSupported {
                operation: "delete_tag",
                backend: self.client.kind(),
                reason: format!(
                    "Registry backend '{}' does not support tag deletion",
                    self.client.kind()
                ),
            }),
        }
    }

    pub(super) async fn delete_manifest_strict(
        &self,
        digest: &str,
    ) -> Result<DeletionOutcome, OciError> {
        let url = endpoint::manifest_url(self.client.endpoint(), self.client.repo(), digest);
        let status = self
            .client
            .request_delete_with_auth_retry(&url, "delete manifest")
            .await?;
        if status.is_success() || status == StatusCode::ACCEPTED || status == StatusCode::NO_CONTENT
        {
            Ok(DeletionOutcome::Deleted)
        } else if status == StatusCode::NOT_FOUND {
            Ok(DeletionOutcome::AlreadyAbsent)
        } else if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            Err(OciError::InsufficientPermission {
                target: digest.to_string(),
                required_scope: "delete:manifest",
                details: format!(
                    "HTTP {} from registry when deleting manifest {}",
                    status, digest
                ),
            })
        } else if status == StatusCode::METHOD_NOT_ALLOWED {
            Err(OciError::OperationNotSupported {
                operation: "delete_manifest",
                backend: self.client.kind(),
                reason: format!(
                    "Registry returned 405 Method Not Allowed for manifest deletion on {}. Backend deletion strategy: {:?}",
                    digest,
                    self.client.capabilities().deletion_strategy
                ),
            })
        } else {
            Err(OciError::DeletionFailed {
                target: digest.to_string(),
                status,
                details: format!("HTTP {} when deleting manifest {}", status, digest),
            })
        }
    }

    pub(super) async fn delete_blob_strict(
        &self,
        digest: &str,
    ) -> Result<DeletionOutcome, OciError> {
        if !self.client.capabilities().supports_blob_physical_deletion {
            return Err(OciError::OperationNotSupported {
                operation: "delete_blob",
                backend: self.client.kind(),
                reason: format!(
                    "Backend '{}' does not support standalone physical OCI blob deletion. Blobs are automatically reclaimed with package/version removal.",
                    self.client.kind()
                ),
            });
        }
        let url = endpoint::blob_url(self.client.endpoint(), self.client.repo(), digest);
        let status = self
            .client
            .request_delete_with_auth_retry(&url, "delete blob")
            .await?;
        if status.is_success() || status == StatusCode::ACCEPTED || status == StatusCode::NO_CONTENT
        {
            Ok(DeletionOutcome::Deleted)
        } else if status == StatusCode::NOT_FOUND {
            Ok(DeletionOutcome::AlreadyAbsent)
        } else if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            Err(OciError::InsufficientPermission {
                target: digest.to_string(),
                required_scope: "delete:blob",
                details: format!(
                    "HTTP {} from registry when deleting blob {}",
                    status, digest
                ),
            })
        } else if status == StatusCode::METHOD_NOT_ALLOWED {
            Err(OciError::OperationNotSupported {
                operation: "delete_blob",
                backend: self.client.kind(),
                reason: format!(
                    "Registry returned 405 Method Not Allowed for blob deletion {}",
                    digest
                ),
            })
        } else {
            Err(OciError::DeletionFailed {
                target: digest.to_string(),
                status,
                details: format!("HTTP {} when deleting blob {}", status, digest),
            })
        }
    }

    pub(super) async fn batch_delete_blobs_strict(
        &self,
        digests: &[NarDigest],
        concurrency: usize,
        strict: bool,
    ) -> Result<DeletionSummary, OciError> {
        if digests.is_empty() {
            return Ok(DeletionSummary::default());
        }
        if !self.client.capabilities().supports_blob_physical_deletion {
            if strict {
                return Err(OciError::OperationNotSupported {
                    operation: "batch_delete_blobs",
                    backend: self.client.kind(),
                    reason: format!(
                        "Backend '{}' does not support standalone physical OCI blob deletion. Blobs are automatically reclaimed with package/version removal.",
                        self.client.kind()
                    ),
                });
            }
            return Ok(DeletionSummary {
                failed_count: digests.len(),
                ..Default::default()
            });
        }
        let concurrency = concurrency.clamp(1, 32);
        let mut stream = futures_util::stream::iter(digests)
            .map(|digest| async move { self.delete_blob_strict(digest.as_ref()).await })
            .buffer_unordered(concurrency);
        let mut summary = DeletionSummary::default();
        while let Some(result) = stream.next().await {
            match result {
                Ok(DeletionOutcome::Deleted) => summary.deleted_count += 1,
                Ok(DeletionOutcome::AlreadyAbsent) => summary.not_found_count += 1,
                Err(error) if strict => return Err(error),
                Err(error) => {
                    summary.failed_count += 1;
                    warn!("Non-fatal error deleting blob: {}", error);
                }
            }
        }
        Ok(summary)
    }

    pub(super) async fn delete_entire_package_strict(
        &self,
    ) -> Result<PackageDeletionSummary, OciError> {
        match self.client.capabilities().package_deletion_support {
            PackageDeletionSupport::NativeComplete => {
                self.ghcr()?.delete_entire_package().await?;
                Ok(PackageDeletionSummary::default())
            }
            PackageDeletionSupport::TaggedGraphOnly => {
                let plan = self.discover_tag_reachable_graph().await?;
                let mut summary = plan.summary();
                let mut manifests: Vec<_> = plan.manifests.keys().cloned().collect();
                manifests.sort();
                for digest in manifests {
                    match self.delete_manifest_strict(&digest).await? {
                        DeletionOutcome::Deleted => summary.manifests_deleted += 1,
                        DeletionOutcome::AlreadyAbsent => summary.already_absent += 1,
                    }
                }
                let mut blobs: Vec<_> = plan.blobs.keys().cloned().collect();
                blobs.sort();
                for digest in blobs {
                    match self.delete_blob_strict(&digest).await? {
                        DeletionOutcome::Deleted => summary.blobs_deleted += 1,
                        DeletionOutcome::AlreadyAbsent => summary.already_absent += 1,
                    }
                }

                let remaining_tags = self.list_tags_for_deletion().await.map_err(|error| {
                    OciError::DeletionDiscoveryFailed {
                        stage: "final_list_tags",
                        target: self.client.repo().to_string(),
                        details: error.to_string(),
                    }
                })?;
                if !remaining_tags.is_empty() {
                    return Err(OciError::DeletionVerificationFailed {
                        target: self.client.repo().to_string(),
                        details: format!(
                            "{} tag(s) remain after deleting the discovered graph",
                            remaining_tags.len()
                        ),
                    });
                }
                for digest in plan.manifests.keys() {
                    self.verify_manifest_absent(digest).await?;
                }
                for digest in plan.blobs.keys() {
                    if self.client.blobs().head(digest).await? {
                        return Err(OciError::DeletionVerificationFailed {
                            target: digest.clone(),
                            details: "blob is still present after DELETE".to_string(),
                        });
                    }
                }
                Ok(summary)
            }
            PackageDeletionSupport::Unsupported => Err(OciError::OperationNotSupported {
                operation: "delete_package",
                backend: self.client.kind(),
                reason: format!(
                    "Registry backend '{}' cannot prove complete package deletion",
                    self.client.kind()
                ),
            }),
        }
    }

    async fn verify_manifest_absent(&self, digest: &str) -> Result<(), OciError> {
        let url = endpoint::manifest_url(self.client.endpoint(), self.client.repo(), digest);
        let (status, _) = self
            .client
            .request_head_with_auth_retry(&url, "verify manifest deletion")
            .await?;
        if status == StatusCode::NOT_FOUND {
            Ok(())
        } else if status == StatusCode::OK {
            Err(OciError::DeletionVerificationFailed {
                target: digest.to_string(),
                details: "manifest is still present after DELETE".to_string(),
            })
        } else {
            Err(OciError::DeletionVerificationFailed {
                target: digest.to_string(),
                details: format!("manifest verification returned HTTP {status}"),
            })
        }
    }

    fn ghcr(&self) -> Result<GitHubPackagesClient<T>, OciError> {
        Ok(GitHubPackagesClient::new(
            self.client.transport().clone(),
            self.client.token_manager().auth_token(),
            self.client.repo(),
        ))
    }
}
