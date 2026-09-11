use super::IndexClient;
use crate::{
    backend::ManifestCasSupport, client::ManifestCasCondition, error::OciError,
    manifest::OciImageIndex, transport::OciTransport,
};
use nixcache_core::{ShardedArchCacheIndexData, SystemArch};
use nixcache_utils::get_process_id;
use std::time::Duration;
use tracing::warn;

impl<'a, T: OciTransport + Clone> IndexClient<'a, T> {
    pub async fn update_cas<F>(
        &self,
        tag: &str,
        max_retries: usize,
        mut mutator: F,
    ) -> Result<(), OciError>
    where
        F: FnMut(Option<OciImageIndex>) -> Result<OciImageIndex, OciError>,
    {
        self.ensure_cas_supported(tag)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (existing_index, condition) =
                match self.client.manifests().fetch_artifact(tag).await? {
                    Some(artifact) => (
                        artifact.manifest.as_index().cloned(),
                        ManifestCasCondition::Match(artifact.digest),
                    ),
                    None => (None, ManifestCasCondition::CreateOnly),
                };
            let updated_index = mutator(existing_index)?;
            match self.put_cas(tag, &updated_index, condition).await {
                Ok(()) => return Ok(()),
                Err(OciError::CasPreconditionFailed { .. }) if attempt <= max_retries => {
                    let pid = get_process_id();
                    let backoff_ms =
                        (500 * (1 << attempt.min(5))) + ((pid * 37 + attempt as u64 * 53) % 150);
                    warn!(
                        "CAS conflict on Image Index tag {}, retrying in {}ms (attempt {}/{})",
                        tag, backoff_ms, attempt, max_retries
                    );
                    self.client
                        .transport()
                        .sleep(Duration::from_millis(backoff_ms))
                        .await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub async fn update_single_writer<F>(&self, tag: &str, mut mutator: F) -> Result<(), OciError>
    where
        F: FnMut(Option<OciImageIndex>) -> Result<OciImageIndex, OciError>,
    {
        let existing_index = self
            .client
            .manifests()
            .fetch_artifact(tag)
            .await?
            .and_then(|artifact| artifact.manifest.as_index().cloned());
        let updated_index = mutator(existing_index)?;
        self.put(tag, &updated_index).await
    }

    pub async fn update_sharded_cas<F>(
        &self,
        tag: &str,
        system: &SystemArch,
        max_retries: usize,
        mut mutator: F,
    ) -> Result<String, OciError>
    where
        F: FnMut(Option<ShardedArchCacheIndexData>) -> Result<ShardedArchCacheIndexData, OciError>,
    {
        self.ensure_cas_supported(tag)?;
        let mut attempt = 0;
        let arch_tag = arch_tag(tag, system);
        loop {
            attempt += 1;
            let (existing_root, condition) = match self.get_sharded_root(&arch_tag, system).await? {
                Some((data, digest)) => (Some(data), ManifestCasCondition::Match(digest)),
                None => (None, ManifestCasCondition::CreateOnly),
            };
            let updated_root = mutator(existing_root)?;
            match self
                .push_sharded_root_cas(&arch_tag, &updated_root, condition)
                .await
            {
                Ok(digest) => return Ok(digest),
                Err(OciError::CasPreconditionFailed { .. }) if attempt <= max_retries => {
                    let pid = get_process_id();
                    let backoff_ms =
                        (100 * (1 << attempt.min(5))) + ((pid * 37 + attempt as u64 * 53) % 100);
                    warn!(
                        "CAS conflict on sharded root index tag {}, retrying in {}ms (attempt {}/{})",
                        arch_tag, backoff_ms, attempt, max_retries
                    );
                    self.client
                        .transport()
                        .sleep(Duration::from_millis(backoff_ms))
                        .await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub async fn update_sharded_single_writer<F>(
        &self,
        tag: &str,
        system: &SystemArch,
        mut mutator: F,
    ) -> Result<String, OciError>
    where
        F: FnMut(Option<ShardedArchCacheIndexData>) -> Result<ShardedArchCacheIndexData, OciError>,
    {
        let arch_tag = arch_tag(tag, system);
        let existing_root = self
            .get_sharded_root(&arch_tag, system)
            .await?
            .map(|(data, _)| data);
        let updated_root = mutator(existing_root)?;
        self.push_sharded_root(&arch_tag, &updated_root).await
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

fn arch_tag(tag: &str, system: &SystemArch) -> String {
    if tag.ends_with(system.as_str()) {
        tag.to_string()
    } else {
        format!("{}-{}", tag, system.as_str())
    }
}
