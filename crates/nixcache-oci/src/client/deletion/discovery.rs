use super::{DeletionClient, PackageDeletionSummary};
use crate::{
    backend::{GitHubPackagesClient, RegistryDeletionStrategy},
    codec::IndexCodec,
    error::OciError,
    integrity::ContentDigest,
    manifest::{
        CacheLayerMediaType, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MANIFEST_MEDIA_TYPE,
        OciArtifactManifest, OciDescriptor, OciImageManifest,
    },
    transport::OciTransport,
};
use nixcache_core::{ShardDataPayload, ShardedArchCacheIndexData, SystemArch};
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
            let artifact = parse_discovered_manifest(&body, &digest)?;
            plan.manifests.insert(digest.clone(), body);

            match artifact {
                OciArtifactManifest::Index(index) => {
                    for descriptor in index.manifests {
                        descriptor
                            .validate_for(&digest, self.client.limits().max_manifest_bytes())
                            .map_err(|error| {
                                discovery_error("manifest_descriptor", &digest, error)
                            })?;
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
                        let Some((child_body, _child_digest)) = self
                            .client
                            .manifests()
                            .get_with_digest_and_size(&descriptor.digest, descriptor.size)
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

                    for layer in &manifest.layers {
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
                                .get_descriptor(layer)
                                .await
                                .map_err(|error| {
                                    discovery_error("cache_layer_get", &layer.digest, error)
                                })?;
                        if layer_type.is_root_index() {
                            let root: ShardedArchCacheIndexData = IndexCodec::decode_zstd(
                                &layer_bytes,
                                &layer.media_type,
                                self.client.limits().max_index_uncompressed_bytes(),
                            )
                            .map(|decoded| decoded.value)
                            .map_err(|error| {
                                discovery_error("root_index_decode", &layer.digest, error)
                            })?;
                            root.validate_for(
                                &root.system,
                                self.client.repo(),
                                self.client.endpoint().authority(),
                                self.client.limits(),
                            )
                            .map_err(|error| {
                                discovery_error("root_index_validate", &layer.digest, error)
                            })?;
                            for shard in root.shards {
                                if shard.entry_count == 0 {
                                    continue;
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
                                    let shard_data = self
                                        .client
                                        .indexes()
                                        .get_shard_data(&shard, &root.system)
                                        .await
                                        .map_err(|error| {
                                            discovery_error("shard_get", &shard_digest, error)
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
                            let shard: ShardDataPayload = IndexCodec::decode_zstd(
                                &layer_bytes,
                                &layer.media_type,
                                self.client.limits().max_index_uncompressed_bytes(),
                            )
                            .map(|decoded| decoded.value)
                            .map_err(|error| {
                                discovery_error("shard_decode", &layer.digest, error)
                            })?;
                            let system = resolve_shard_system(&manifest, layer, &layer.digest)?;
                            shard
                                .validate_for(shard.shard_id, &system, self.client.limits())
                                .map_err(|error| {
                                    discovery_error("shard_validate", &layer.digest, error)
                                })?;
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

fn resolve_shard_system(
    manifest: &OciImageManifest,
    layer: &OciDescriptor,
    target: &str,
) -> Result<SystemArch, OciError> {
    let mut candidates = Vec::new();
    if let Some(platform) = &manifest.config.platform {
        candidates.push(("manifest config platform", platform.to_system()));
    }
    if let Some(platform) = &layer.platform {
        candidates.push(("shard layer platform", platform.to_system()));
    }
    if let Some(annotations) = &manifest.annotations
        && let Some(system) = annotations.get("org.nixos.nixcache.system")
    {
        candidates.push((
            "manifest system annotation",
            SystemArch::from(system.as_str()),
        ));
    }
    if let Some(annotations) = &layer.annotations
        && let Some(system) = annotations.get("org.nixos.nixcache.system")
    {
        candidates.push((
            "shard layer system annotation",
            SystemArch::from(system.as_str()),
        ));
    }

    let mut resolved = None;
    for (source, system) in candidates {
        if !system.is_known() {
            return Err(discovery_error(
                "shard_context",
                target,
                format!("{source} is unknown"),
            ));
        }
        if let Some(previous) = resolved
            && previous != system
        {
            return Err(discovery_error(
                "shard_context",
                target,
                "manifest and shard layer systems do not match",
            ));
        }
        resolved = Some(system);
    }

    resolved.ok_or_else(|| {
        discovery_error(
            "shard_context",
            target,
            "direct shard payload has no manifest or layer system",
        )
    })
}

fn valid_digest(digest: &str) -> bool {
    ContentDigest::parse(digest).is_ok()
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
