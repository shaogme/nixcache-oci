use super::{DeletionClient, PackageDeletionSummary};
use crate::{
    backend::{GitHubPackagesClient, RegistryDeletionStrategy},
    client::endpoint,
    codec::IndexCodec,
    error::OciError,
    manifest::{
        CacheLayerMediaType, CacheLayerMediaTypeV6, OCI_IMAGE_INDEX_MEDIA_TYPE,
        OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciArtifactManifest,
    },
    transport::OciTransport,
};
use nixcache_core::{ShardDataPayload, ShardedArchCacheIndexData};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

pub(super) const MAX_DELETION_MANIFESTS: usize = 100_000;
pub(super) const MAX_DELETION_BLOBS: usize = 2_000_000;

#[derive(Debug, Clone)]
pub(super) struct DeletionPlan {
    pub(super) tags: Vec<String>,
    pub(super) manifests: HashMap<String, String>,
    pub(super) blobs: HashMap<String, u64>,
}

impl DeletionPlan {
    pub(super) fn summary(&self) -> PackageDeletionSummary {
        PackageDeletionSummary {
            tags_discovered: self.tags.len(),
            manifests_discovered: self.manifests.len(),
            blobs_discovered: self.blobs.len(),
            ..PackageDeletionSummary::default()
        }
    }
}

impl<'a, T: OciTransport + Clone> DeletionClient<'a, T> {
    pub(super) async fn discover_tag_reachable_graph(&self) -> Result<DeletionPlan, OciError> {
        let mut tags = self.list_tags_for_deletion().await?;
        tags.sort();
        tags.dedup();

        let mut plan = DeletionPlan {
            tags,
            manifests: HashMap::new(),
            blobs: HashMap::new(),
        };
        let mut pending = VecDeque::new();
        let mut queued_manifests = HashSet::new();
        let mut decoded_shard_blobs = HashSet::new();

        for tag in &plan.tags {
            let Some((body, digest)) = self
                .client
                .manifests()
                .get_with_digest(tag)
                .await
                .map_err(|error| discovery_error("manifest_get", tag, error))?
            else {
                continue;
            };
            if !valid_digest(&digest) {
                return Err(discovery_error(
                    "manifest_digest",
                    tag,
                    "registry returned an invalid Docker-Content-Digest",
                ));
            }
            let computed = endpoint::compute_sha256_digest(body.as_bytes());
            if computed != digest {
                return Err(discovery_error(
                    "manifest_digest",
                    tag,
                    format!("digest header/body mismatch: header {digest}, body {computed}"),
                ));
            }
            if queued_manifests.contains(&digest) {
                continue;
            }
            if queued_manifests.len() >= MAX_DELETION_MANIFESTS {
                return Err(OciError::DeletionObjectLimitExceeded {
                    target: self.client.repo().to_string(),
                });
            }
            queued_manifests.insert(digest.clone());
            pending.push_back((digest, body, tag.clone()));
        }

        while let Some((digest, body, source)) = pending.pop_front() {
            if plan.manifests.contains_key(&digest) {
                continue;
            }
            if plan.manifests.len() >= MAX_DELETION_MANIFESTS {
                return Err(OciError::DeletionObjectLimitExceeded {
                    target: self.client.repo().to_string(),
                });
            }
            let computed = endpoint::compute_sha256_digest(body.as_bytes());
            if computed != digest {
                return Err(discovery_error(
                    "manifest_digest",
                    &digest,
                    format!("digest/body mismatch while traversing tag {source}"),
                ));
            }
            let artifact = parse_discovered_manifest(&body, &digest)?;
            plan.manifests.insert(digest.clone(), body);

            match artifact {
                OciArtifactManifest::Index(index) => {
                    for descriptor in index.manifests {
                        if !valid_digest(&descriptor.digest) {
                            return Err(discovery_error(
                                "manifest_descriptor",
                                &digest,
                                "index contains an invalid manifest digest",
                            ));
                        }
                        if !matches!(
                            descriptor.media_type.as_str(),
                            OCI_IMAGE_INDEX_MEDIA_TYPE | OCI_IMAGE_MANIFEST_MEDIA_TYPE
                        ) {
                            return Err(discovery_error(
                                "manifest_descriptor",
                                &digest,
                                format!(
                                    "unsupported child manifest media type '{}'",
                                    descriptor.media_type
                                ),
                            ));
                        }
                        if queued_manifests.contains(&descriptor.digest) {
                            continue;
                        }
                        if queued_manifests.len() >= MAX_DELETION_MANIFESTS {
                            return Err(OciError::DeletionObjectLimitExceeded {
                                target: descriptor.digest,
                            });
                        }
                        queued_manifests.insert(descriptor.digest.clone());
                        let Some((child_body, child_digest)) = self
                            .client
                            .manifests()
                            .get_with_digest(&descriptor.digest)
                            .await
                            .map_err(|error| {
                                discovery_error("manifest_get", &descriptor.digest, error)
                            })?
                        else {
                            return Err(discovery_error(
                                "manifest_get",
                                &descriptor.digest,
                                "child manifest disappeared during discovery",
                            ));
                        };
                        if child_digest != descriptor.digest
                            || endpoint::compute_sha256_digest(child_body.as_bytes())
                                != descriptor.digest
                        {
                            return Err(discovery_error(
                                "manifest_digest",
                                &descriptor.digest,
                                "child manifest digest does not match descriptor",
                            ));
                        }
                        pending.push_back((descriptor.digest, child_body, source.clone()));
                    }
                }
                OciArtifactManifest::Manifest(manifest) => {
                    add_blob(
                        &mut plan,
                        &manifest.config.digest,
                        manifest.config.size,
                        &digest,
                        "config",
                        self.client.repo(),
                    )?;

                    for layer in manifest.layers {
                        add_blob(
                            &mut plan,
                            &layer.digest,
                            layer.size,
                            &digest,
                            "layer",
                            self.client.repo(),
                        )?;
                        let Some(layer_type) = CacheLayerMediaType::parse(&layer.media_type) else {
                            continue;
                        };
                        let layer_bytes =
                            self.client
                                .blobs()
                                .get(&layer.digest)
                                .await
                                .map_err(|error| {
                                    discovery_error("cache_layer_get", &layer.digest, error)
                                })?;
                        let computed_layer_digest = endpoint::compute_sha256_digest(&layer_bytes);
                        if computed_layer_digest != layer.digest {
                            return Err(discovery_error(
                                "cache_layer_digest",
                                &layer.digest,
                                format!(
                                    "cache layer body digest mismatch: expected {}, got {}",
                                    layer.digest, computed_layer_digest
                                ),
                            ));
                        }
                        if layer_type.is_root_index() {
                            let root: ShardedArchCacheIndexData =
                                IndexCodec::decode_zstd(&layer_bytes, &layer.media_type).map_err(
                                    |error| {
                                        discovery_error("root_index_decode", &layer.digest, error)
                                    },
                                )?;
                            for shard in root.shards {
                                if shard.entry_count == 0 {
                                    continue;
                                }
                                if shard.blob_digest.is_empty() || !valid_digest(&shard.blob_digest)
                                {
                                    return Err(discovery_error(
                                        "root_index_decode",
                                        &layer.digest,
                                        "root index contains an invalid shard digest",
                                    ));
                                }
                                let shard_digest = shard.blob_digest.clone();
                                add_blob(
                                    &mut plan,
                                    &shard_digest,
                                    shard.compressed_size,
                                    &layer.digest,
                                    "shard",
                                    self.client.repo(),
                                )?;
                                if decoded_shard_blobs.insert(shard_digest.clone()) {
                                    let shard_bytes =
                                        self.client.blobs().get(&shard_digest).await.map_err(
                                            |error| {
                                                discovery_error("shard_get", &shard_digest, error)
                                            },
                                        )?;
                                    if endpoint::compute_sha256_digest(&shard_bytes) != shard_digest
                                    {
                                        return Err(discovery_error(
                                            "shard_digest",
                                            &shard_digest,
                                            "shard body digest does not match descriptor",
                                        ));
                                    }
                                    let shard_data: ShardDataPayload = IndexCodec::decode_zstd(
                                        &shard_bytes,
                                        CacheLayerMediaTypeV6::SHARD_DATA_V6_ZSTD,
                                    )
                                    .map_err(|error| {
                                        discovery_error("shard_decode", &shard_digest, error)
                                    })?;
                                    add_nar_blobs(
                                        &mut plan,
                                        &shard_data,
                                        &shard_digest,
                                        self.client.repo(),
                                    )?;
                                }
                            }
                        } else {
                            let shard: ShardDataPayload =
                                IndexCodec::decode_zstd(&layer_bytes, &layer.media_type).map_err(
                                    |error| discovery_error("shard_decode", &layer.digest, error),
                                )?;
                            add_nar_blobs(&mut plan, &shard, &layer.digest, self.client.repo())?;
                        }
                    }
                }
            }
        }
        Ok(plan)
    }

    pub(super) async fn list_tags_for_deletion(&self) -> Result<Vec<String>, OciError> {
        let mut tags = self.client.manifests().list_tags().await?;
        if tags.is_empty()
            && self.client.capabilities().deletion_strategy
                == RegistryDeletionStrategy::GitHubPackagesRestApi
        {
            let ghcr = GitHubPackagesClient::new(
                self.client.transport().clone(),
                self.client.token_manager().auth_token(),
                self.client.repo(),
            );
            for version in ghcr.list_package_versions().await? {
                if let Some(metadata) = version.metadata
                    && let Some(container) = metadata.container
                {
                    tags.extend(container.tags);
                }
            }
        }
        tags.sort();
        tags.dedup();
        Ok(tags)
    }
}

fn valid_digest(digest: &str) -> bool {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn discovery_error(
    stage: &'static str,
    target: impl Into<String>,
    error: impl ToString,
) -> OciError {
    OciError::DeletionDiscoveryFailed {
        stage,
        target: target.into(),
        details: error.to_string(),
    }
}

fn parse_discovered_manifest(body: &str, target: &str) -> Result<OciArtifactManifest, OciError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| discovery_error("manifest_json", target, error))?;
    let schema_version = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| discovery_error("manifest_json", target, "missing schemaVersion"))?;
    if schema_version != 2 {
        return Err(discovery_error(
            "manifest_json",
            target,
            "unsupported OCI schemaVersion",
        ));
    }
    let media_type = value
        .get("mediaType")
        .and_then(Value::as_str)
        .ok_or_else(|| discovery_error("manifest_json", target, "missing mediaType"))?;
    if !matches!(
        media_type,
        OCI_IMAGE_INDEX_MEDIA_TYPE | OCI_IMAGE_MANIFEST_MEDIA_TYPE
    ) {
        return Err(discovery_error(
            "manifest_json",
            target,
            format!("unsupported OCI manifest media type '{media_type}'"),
        ));
    }
    serde_json::from_value(value).map_err(|error| discovery_error("manifest_json", target, error))
}

fn add_blob(
    plan: &mut DeletionPlan,
    digest: &str,
    size: u64,
    target: &str,
    kind: &'static str,
    repo: &str,
) -> Result<(), OciError> {
    if !valid_digest(digest) {
        return Err(discovery_error(
            "blob_descriptor",
            target,
            format!("manifest contains an invalid {kind} digest"),
        ));
    }
    if plan.blobs.len() >= MAX_DELETION_BLOBS && !plan.blobs.contains_key(digest) {
        return Err(OciError::DeletionObjectLimitExceeded {
            target: repo.to_string(),
        });
    }
    plan.blobs.entry(digest.to_string()).or_insert(size);
    Ok(())
}

fn add_nar_blobs(
    plan: &mut DeletionPlan,
    shard: &ShardDataPayload,
    target: &str,
    repo: &str,
) -> Result<(), OciError> {
    for entry in shard.entries.values() {
        let digest = entry.nar_digest.to_string();
        if !valid_digest(&digest) {
            return Err(discovery_error(
                "shard_decode",
                target,
                "shard contains an invalid NAR digest",
            ));
        }
        if plan.blobs.len() >= MAX_DELETION_BLOBS && !plan.blobs.contains_key(&digest) {
            return Err(OciError::DeletionObjectLimitExceeded {
                target: repo.to_string(),
            });
        }
        plan.blobs.entry(digest).or_insert(entry.nar_size);
    }
    Ok(())
}
