use nixcache_core::IndexValidationLimits;
use std::convert::TryFrom;
use thiserror::Error;

/// OCI 远端读取的资源上限。
///
/// 所有字段都是协议外的防御性限制。构造和覆盖限制时会先检查非零及
/// 当前平台上需要分配内存的限制会检查 `usize` 可表示性；streaming 上限只累计
/// `u64` 字节数，因此不要求在 32 位平台转换为 `usize`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OciReadLimits {
    max_manifest_bytes: u64,
    max_buffered_blob_bytes: u64,
    max_streamed_blob_bytes: u64,
    max_index_uncompressed_bytes: u64,
    max_image_index_descriptors: u64,
    max_manifest_layers: u64,
    max_shard_entries: u64,
    max_gc_roots: u64,
    max_parallel_index_reads: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReadLimitsError {
    #[error("OCI read limit '{field}' must be greater than zero")]
    Zero { field: &'static str },

    #[error("OCI read limit '{field}' cannot be represented on this platform")]
    NotRepresentable { field: &'static str },
}

impl Default for OciReadLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: 4 * 1024 * 1024,
            max_buffered_blob_bytes: 64 * 1024 * 1024,
            max_streamed_blob_bytes: 16 * 1024 * 1024 * 1024,
            max_index_uncompressed_bytes: 64 * 1024 * 1024,
            max_image_index_descriptors: 1024,
            max_manifest_layers: 16,
            max_shard_entries: 500_000,
            max_gc_roots: 500_000,
            max_parallel_index_reads: 16,
        }
    }
}

impl OciReadLimits {
    /// 从所有限制值构造配置。数值使用 `u64`，内存分配相关限制即使在 32 位
    /// target 上也会在这里明确拒绝无法安全转换为 `usize` 的配置。
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        max_manifest_bytes: u64,
        max_buffered_blob_bytes: u64,
        max_streamed_blob_bytes: u64,
        max_index_uncompressed_bytes: u64,
        max_image_index_descriptors: u64,
        max_manifest_layers: u64,
        max_shard_entries: u64,
        max_gc_roots: u64,
        max_parallel_index_reads: u64,
    ) -> Result<Self, ReadLimitsError> {
        let limits = Self {
            max_manifest_bytes,
            max_buffered_blob_bytes,
            max_streamed_blob_bytes,
            max_index_uncompressed_bytes,
            max_image_index_descriptors,
            max_manifest_layers,
            max_shard_entries,
            max_gc_roots,
            max_parallel_index_reads,
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(&self) -> Result<(), ReadLimitsError> {
        let values = [
            ("max_manifest_bytes", self.max_manifest_bytes),
            ("max_buffered_blob_bytes", self.max_buffered_blob_bytes),
            (
                "max_index_uncompressed_bytes",
                self.max_index_uncompressed_bytes,
            ),
            (
                "max_image_index_descriptors",
                self.max_image_index_descriptors,
            ),
            ("max_manifest_layers", self.max_manifest_layers),
            ("max_shard_entries", self.max_shard_entries),
            ("max_gc_roots", self.max_gc_roots),
            ("max_parallel_index_reads", self.max_parallel_index_reads),
        ];
        for (field, value) in values {
            if value == 0 {
                return Err(ReadLimitsError::Zero { field });
            }
            usize::try_from(value).map_err(|_| ReadLimitsError::NotRepresentable { field })?;
        }
        if self.max_streamed_blob_bytes == 0 {
            return Err(ReadLimitsError::Zero {
                field: "max_streamed_blob_bytes",
            });
        }
        Ok(())
    }

    pub fn max_manifest_bytes(&self) -> u64 {
        self.max_manifest_bytes
    }

    pub fn max_buffered_blob_bytes(&self) -> u64 {
        self.max_buffered_blob_bytes
    }

    pub fn max_streamed_blob_bytes(&self) -> u64 {
        self.max_streamed_blob_bytes
    }

    pub fn max_index_uncompressed_bytes(&self) -> u64 {
        self.max_index_uncompressed_bytes
    }

    pub fn max_image_index_descriptors(&self) -> u64 {
        self.max_image_index_descriptors
    }

    pub fn max_manifest_layers(&self) -> u64 {
        self.max_manifest_layers
    }

    pub fn max_shard_entries(&self) -> u64 {
        self.max_shard_entries
    }

    pub fn max_gc_roots(&self) -> u64 {
        self.max_gc_roots
    }

    pub fn max_parallel_index_reads(&self) -> u64 {
        self.max_parallel_index_reads
    }

    pub fn with_max_manifest_bytes(self, value: u64) -> Result<Self, ReadLimitsError> {
        Self::try_new(
            value,
            self.max_buffered_blob_bytes,
            self.max_streamed_blob_bytes,
            self.max_index_uncompressed_bytes,
            self.max_image_index_descriptors,
            self.max_manifest_layers,
            self.max_shard_entries,
            self.max_gc_roots,
            self.max_parallel_index_reads,
        )
    }

    pub fn with_max_buffered_blob_bytes(self, value: u64) -> Result<Self, ReadLimitsError> {
        Self::try_new(
            self.max_manifest_bytes,
            value,
            self.max_streamed_blob_bytes,
            self.max_index_uncompressed_bytes,
            self.max_image_index_descriptors,
            self.max_manifest_layers,
            self.max_shard_entries,
            self.max_gc_roots,
            self.max_parallel_index_reads,
        )
    }

    pub fn with_max_streamed_blob_bytes(self, value: u64) -> Result<Self, ReadLimitsError> {
        Self::try_new(
            self.max_manifest_bytes,
            self.max_buffered_blob_bytes,
            value,
            self.max_index_uncompressed_bytes,
            self.max_image_index_descriptors,
            self.max_manifest_layers,
            self.max_shard_entries,
            self.max_gc_roots,
            self.max_parallel_index_reads,
        )
    }

    pub fn with_max_index_uncompressed_bytes(self, value: u64) -> Result<Self, ReadLimitsError> {
        Self::try_new(
            self.max_manifest_bytes,
            self.max_buffered_blob_bytes,
            self.max_streamed_blob_bytes,
            value,
            self.max_image_index_descriptors,
            self.max_manifest_layers,
            self.max_shard_entries,
            self.max_gc_roots,
            self.max_parallel_index_reads,
        )
    }

    pub fn with_max_image_index_descriptors(self, value: u64) -> Result<Self, ReadLimitsError> {
        Self::try_new(
            self.max_manifest_bytes,
            self.max_buffered_blob_bytes,
            self.max_streamed_blob_bytes,
            self.max_index_uncompressed_bytes,
            value,
            self.max_manifest_layers,
            self.max_shard_entries,
            self.max_gc_roots,
            self.max_parallel_index_reads,
        )
    }

    pub fn with_max_manifest_layers(self, value: u64) -> Result<Self, ReadLimitsError> {
        Self::try_new(
            self.max_manifest_bytes,
            self.max_buffered_blob_bytes,
            self.max_streamed_blob_bytes,
            self.max_index_uncompressed_bytes,
            self.max_image_index_descriptors,
            value,
            self.max_shard_entries,
            self.max_gc_roots,
            self.max_parallel_index_reads,
        )
    }

    pub fn with_max_shard_entries(self, value: u64) -> Result<Self, ReadLimitsError> {
        Self::try_new(
            self.max_manifest_bytes,
            self.max_buffered_blob_bytes,
            self.max_streamed_blob_bytes,
            self.max_index_uncompressed_bytes,
            self.max_image_index_descriptors,
            self.max_manifest_layers,
            value,
            self.max_gc_roots,
            self.max_parallel_index_reads,
        )
    }

    pub fn with_max_gc_roots(self, value: u64) -> Result<Self, ReadLimitsError> {
        Self::try_new(
            self.max_manifest_bytes,
            self.max_buffered_blob_bytes,
            self.max_streamed_blob_bytes,
            self.max_index_uncompressed_bytes,
            self.max_image_index_descriptors,
            self.max_manifest_layers,
            self.max_shard_entries,
            value,
            self.max_parallel_index_reads,
        )
    }

    pub fn with_max_parallel_index_reads(self, value: u64) -> Result<Self, ReadLimitsError> {
        Self::try_new(
            self.max_manifest_bytes,
            self.max_buffered_blob_bytes,
            self.max_streamed_blob_bytes,
            self.max_index_uncompressed_bytes,
            self.max_image_index_descriptors,
            self.max_manifest_layers,
            self.max_shard_entries,
            self.max_gc_roots,
            value,
        )
    }
}

impl IndexValidationLimits for OciReadLimits {
    fn max_shard_entries(&self) -> u64 {
        self.max_shard_entries()
    }

    fn max_gc_roots(&self) -> u64 {
        self.max_gc_roots()
    }

    fn max_uncompressed_bytes(&self) -> u64 {
        self.max_index_uncompressed_bytes()
    }

    fn max_nar_size(&self) -> u64 {
        self.max_streamed_blob_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::{OciReadLimits, ReadLimitsError};

    #[test]
    fn defaults_are_bounded_and_stream_limit_is_large() {
        let limits = OciReadLimits::default();
        assert_eq!(limits.max_manifest_bytes(), 4 * 1024 * 1024);
        assert_eq!(limits.max_buffered_blob_bytes(), 64 * 1024 * 1024);
        assert_eq!(limits.max_streamed_blob_bytes(), 16 * 1024 * 1024 * 1024);
        assert!(limits.validate().is_ok());
    }

    #[test]
    fn zero_overrides_are_rejected() {
        let error = OciReadLimits::default()
            .with_max_parallel_index_reads(0)
            .expect_err("zero concurrency must not be accepted");
        assert_eq!(
            error,
            ReadLimitsError::Zero {
                field: "max_parallel_index_reads"
            }
        );
    }
}
