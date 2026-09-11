mod discovery;
mod operations;

use super::OciClient;
use crate::{error::OciError, transport::OciTransport};
use nixcache_core::NarDigest;

/// 单个 manifest/blob DELETE 的明确结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionOutcome {
    Deleted,
    AlreadyAbsent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletionSummary {
    pub deleted_count: usize,
    pub not_found_count: usize,
    pub failed_count: usize,
    pub freed_bytes: u64,
}

/// 一次严格包删除的可审计汇总。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageDeletionSummary {
    pub tags_discovered: usize,
    pub manifests_discovered: usize,
    pub blobs_discovered: usize,
    pub manifests_deleted: usize,
    pub blobs_deleted: usize,
    pub already_absent: usize,
}

pub struct DeletionClient<'a, T: OciTransport> {
    pub(super) client: &'a OciClient<T>,
}

impl<'a, T: OciTransport + Clone> DeletionClient<'a, T> {
    pub(super) fn new(client: &'a OciClient<T>) -> Self {
        Self { client }
    }

    pub async fn preview_package(&self) -> Result<PackageDeletionSummary, OciError> {
        Ok(self.discover_tag_reachable_graph().await?.summary())
    }

    pub async fn delete_tag(&self, tag: &str) -> Result<(), OciError> {
        self.delete_tag_strict(tag).await
    }

    pub async fn delete_manifest(&self, digest: &str) -> Result<DeletionOutcome, OciError> {
        self.delete_manifest_strict(digest).await
    }

    pub async fn delete_blob(&self, digest: &str) -> Result<DeletionOutcome, OciError> {
        self.delete_blob_strict(digest).await
    }

    pub async fn batch_delete_blobs(
        &self,
        digests: &[NarDigest],
        concurrency: usize,
        strict: bool,
    ) -> Result<DeletionSummary, OciError> {
        self.batch_delete_blobs_strict(digests, concurrency, strict)
            .await
    }

    pub async fn delete_entire_package(&self) -> Result<PackageDeletionSummary, OciError> {
        self.delete_entire_package_strict().await
    }
}
