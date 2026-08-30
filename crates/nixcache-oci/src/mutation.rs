use chrono::Utc;
use nixcache_core::{DeltaPatchData, IndexEntry, JobSummaryMetadata, StoreHash, SystemArch};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct SessionMutationRequest {
    pub run_id: u64,
    pub job_id: String,
    pub system: SystemArch,
    pub new_entries: HashMap<StoreHash, IndexEntry>,
    pub new_roots: Vec<StoreHash>,
    pub head_sha: Option<String>,
    pub ref_name: Option<String>,
    pub public_key: Option<String>,
    pub uploaded_blobs: usize,
    pub uploaded_bytes: u64,
    pub max_retries: usize,
}

impl SessionMutationRequest {
    pub fn new(run_id: u64, job_id: impl Into<String>, system: impl Into<SystemArch>) -> Self {
        Self {
            run_id,
            job_id: job_id.into(),
            system: system.into(),
            new_entries: HashMap::new(),
            new_roots: Vec::new(),
            head_sha: None,
            ref_name: None,
            public_key: None,
            uploaded_blobs: 0,
            uploaded_bytes: 0,
            max_retries: 5,
        }
    }

    pub fn with_entries(mut self, entries: HashMap<StoreHash, IndexEntry>) -> Self {
        self.new_entries = entries;
        self
    }

    pub fn with_roots(mut self, roots: Vec<StoreHash>) -> Self {
        self.new_roots = roots;
        self
    }

    pub fn with_git_info(mut self, head_sha: Option<String>, ref_name: Option<String>) -> Self {
        self.head_sha = head_sha;
        self.ref_name = ref_name;
        self
    }

    pub fn with_public_key(mut self, public_key: Option<String>) -> Self {
        self.public_key = public_key;
        self
    }

    pub fn with_upload_stats(mut self, uploaded_blobs: usize, uploaded_bytes: u64) -> Self {
        self.uploaded_blobs = uploaded_blobs;
        self.uploaded_bytes = uploaded_bytes;
        self
    }

    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// 转换为可独立持久化的 DeltaPatchData
    pub fn to_delta_patch(&self) -> DeltaPatchData {
        let mut delta = DeltaPatchData::new(self.run_id, &self.job_id, self.system);
        delta.new_entries = self.new_entries.clone();
        delta.active_gc_roots = self.new_roots.clone();
        delta.active_gc_roots.sort_unstable();
        delta.active_gc_roots.dedup();
        delta
    }

    /// 合并变更至已有的 DeltaPatchData
    pub fn apply_to_delta(&self, delta: &mut DeltaPatchData) {
        delta.new_entries.extend(self.new_entries.clone());
        delta.active_gc_roots.extend_from_slice(&self.new_roots);
        delta.active_gc_roots.sort_unstable();
        delta.active_gc_roots.dedup();
        delta.timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    }

    /// 提取当前请求的 JobSummaryMetadata
    pub fn to_job_summary(&self) -> JobSummaryMetadata {
        JobSummaryMetadata {
            job_id: self.job_id.clone(),
            system: self.system,
            uploaded_blobs: self.uploaded_blobs,
            uploaded_bytes: self.uploaded_bytes,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionMutationRequest;
    use nixcache_core::{DeltaPatchData, IndexEntry, StoreHash, SystemArch};
    use std::collections::HashMap;

    fn h(id: u8) -> StoreHash {
        format!("{:032x}", id).parse().unwrap()
    }

    #[test]
    fn test_delta_patch_generation_and_merge() {
        let mut entries = HashMap::new();
        entries.insert(h(1), IndexEntry::default());

        let req = SessionMutationRequest::new(100, "job-vm", SystemArch::X86_64Linux)
            .with_entries(entries)
            .with_roots(vec![h(3), h(2)]);

        let delta = req.to_delta_patch();
        assert_eq!(delta.run_id, 100);
        assert_eq!(delta.job_id, "job-vm");
        assert_eq!(delta.system, SystemArch::X86_64Linux);
        assert_eq!(delta.new_entries.len(), 1);
        assert_eq!(delta.active_gc_roots, vec![h(2), h(3)]);

        let mut existing_delta = DeltaPatchData::new(100, "job-prev", SystemArch::X86_64Linux);
        existing_delta.active_gc_roots = vec![h(1), h(3)];

        req.apply_to_delta(&mut existing_delta);
        assert_eq!(existing_delta.new_entries.len(), 1);
        assert_eq!(existing_delta.active_gc_roots, vec![h(1), h(2), h(3)]);
    }
}
