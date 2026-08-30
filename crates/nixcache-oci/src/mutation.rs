use chrono::Utc;
use nixcache_core::{
    ArchRunSessionManifest, IndexEntry, JobSummaryMetadata, RunSessionManifest, StoreHash,
    SystemArch,
};
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

    pub fn apply_to(&self, session: &mut RunSessionManifest) {
        if session.head_sha.is_empty()
            && let Some(ref sha) = self.head_sha
        {
            session.head_sha = sha.clone();
        }
        if session.ref_name.is_empty()
            && let Some(ref rn) = self.ref_name
        {
            session.ref_name = rn.clone();
        }
        if session.public_key.is_none()
            && let Some(ref pk) = self.public_key
            && !pk.is_empty()
        {
            session.public_key = Some(pk.clone());
        }

        session.entries.extend(self.new_entries.clone());
        let roots_entry = session.gc_roots.entry(self.system).or_default();
        roots_entry.extend_from_slice(&self.new_roots);
        roots_entry.sort_unstable();
        roots_entry.dedup();
        session.updated_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        session.completed_jobs.push(JobSummaryMetadata {
            job_id: self.job_id.clone(),
            system: self.system,
            uploaded_blobs: self.uploaded_blobs,
            uploaded_bytes: self.uploaded_bytes,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        });
    }

    pub fn apply_to_arch(&self, session: &mut ArchRunSessionManifest) {
        if session.head_sha.is_empty()
            && let Some(ref sha) = self.head_sha
        {
            session.head_sha = sha.clone();
        }
        if session.ref_name.is_empty()
            && let Some(ref rn) = self.ref_name
        {
            session.ref_name = rn.clone();
        }
        if session.public_key.is_none()
            && let Some(ref pk) = self.public_key
            && !pk.is_empty()
        {
            session.public_key = Some(pk.clone());
        }

        session.entries.extend(self.new_entries.clone());
        session.gc_roots.extend_from_slice(&self.new_roots);
        session.gc_roots.sort_unstable();
        session.gc_roots.dedup();
        session.updated_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        session.completed_jobs.push(JobSummaryMetadata {
            job_id: self.job_id.clone(),
            system: self.system,
            uploaded_blobs: self.uploaded_blobs,
            uploaded_bytes: self.uploaded_bytes,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::SessionMutationRequest;
    use nixcache_core::{ArchRunSessionManifest, RunSessionManifest, StoreHash, SystemArch};

    fn h(id: u8) -> StoreHash {
        format!("{:032x}", id).parse().unwrap()
    }

    #[test]
    fn test_gc_roots_merge_and_dedup() {
        let mut manifest = RunSessionManifest::default();
        manifest
            .gc_roots
            .insert(SystemArch::X86_64Linux, vec![h(2), h(1)]);

        let req = SessionMutationRequest::new(1, "job1", SystemArch::X86_64Linux)
            .with_roots(vec![h(3), h(2)]);

        req.apply_to(&mut manifest);

        let roots = manifest.gc_roots.get(&SystemArch::X86_64Linux).unwrap();
        assert_eq!(roots, &vec![h(1), h(2), h(3)]);

        let mut arch_manifest = ArchRunSessionManifest::new(1, SystemArch::X86_64Linux);
        arch_manifest.gc_roots = vec![h(3), h(1)];

        let req_arch = SessionMutationRequest::new(1, "job1", SystemArch::X86_64Linux)
            .with_roots(vec![h(2), h(1)]);

        req_arch.apply_to_arch(&mut arch_manifest);
        assert_eq!(arch_manifest.gc_roots, vec![h(1), h(2), h(3)]);
    }
}
