mod discovery;
mod operations;

use super::OciClient;
use crate::{error::OciError, transport::OciTransport};
use nixcache_core::NarDigest;

/// 带有已知大小的 Blob 删除目标。大小来自发现阶段或本地索引，仅用于审计统计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobDeletionTarget {
    pub digest: NarDigest,
    pub size: u64,
}

/// 单个 manifest DELETE 的明确结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestDeletionOutcome {
    Deleted,
    AlreadyAbsent,
}

/// 单个 blob DELETE 的明确结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobDeletionOutcome {
    Deleted { bytes: u64 },
    AlreadyAbsent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionObjectType {
    Manifest,
    Blob,
    Tag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionFailureKind {
    PermissionDenied,
    NotSupported,
    HttpStatus,
    Transport,
    Verification,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionFailure {
    pub digest: String,
    pub object_type: DeletionObjectType,
    pub kind: DeletionFailureKind,
    pub details: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletionSummary {
    pub requested_count: usize,
    pub deleted_count: usize,
    pub already_absent_count: usize,
    pub failed_count: usize,
    pub deleted_bytes: u64,
    pub failures: Vec<DeletionFailure>,
}

impl DeletionSummary {
    pub fn is_complete(&self) -> bool {
        self.failed_count == 0
            && self.failed_count == self.failures.len()
            && self.requested_count == self.deleted_count.saturating_add(self.already_absent_count)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletionBatchResult {
    Complete(DeletionSummary),
    Partial(DeletionSummary),
}

/// 一次包删除的可审计汇总。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDeletionSummary {
    pub scope: PackageDeletionScope,
    pub counts_known: bool,
    pub tags_discovered: usize,
    pub manifests_discovered: usize,
    pub blobs_discovered: usize,
    pub manifests_deleted: usize,
    pub blobs_deleted: usize,
    pub manifests_already_absent: usize,
    pub blobs_already_absent: usize,
    pub deleted_blob_bytes: u64,
    pub failures: Vec<DeletionFailure>,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PackageDeletionScope {
    #[default]
    TagReachableGraph,
    NativePackageApi,
}

impl Default for PackageDeletionSummary {
    fn default() -> Self {
        Self {
            scope: PackageDeletionScope::TagReachableGraph,
            counts_known: true,
            tags_discovered: 0,
            manifests_discovered: 0,
            blobs_discovered: 0,
            manifests_deleted: 0,
            blobs_deleted: 0,
            manifests_already_absent: 0,
            blobs_already_absent: 0,
            deleted_blob_bytes: 0,
            failures: Vec::new(),
            complete: true,
        }
    }
}

impl PackageDeletionSummary {
    pub fn failed_count(&self) -> usize {
        self.failures.len()
    }
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

    pub async fn delete_manifest(&self, digest: &str) -> Result<ManifestDeletionOutcome, OciError> {
        self.delete_manifest_strict(digest).await
    }

    pub async fn delete_blob(
        &self,
        target: &BlobDeletionTarget,
    ) -> Result<BlobDeletionOutcome, OciError> {
        self.delete_blob_strict(target).await
    }

    pub async fn batch_delete_blobs(
        &self,
        targets: &[BlobDeletionTarget],
        concurrency: usize,
    ) -> Result<DeletionBatchResult, OciError> {
        self.batch_delete_blobs_strict(targets, concurrency).await
    }

    pub async fn delete_entire_package(&self) -> Result<PackageDeletionSummary, OciError> {
        self.delete_entire_package_strict().await
    }
}
