mod update;

use super::{ManifestCasCondition, OciClient, endpoint};
use crate::{
    backend::ManifestCasSupport,
    codec::IndexCodec,
    error::{OciError, TransportError},
    manifest::{
        CacheLayerMediaTypeV8, EMPTY_CONFIG_DIGEST, EMPTY_CONFIG_SIZE, OCI_IMAGE_INDEX_MEDIA_TYPE,
        OciArtifactManifest, OciDescriptor, OciImageIndex, OciImageManifest,
        ShardedArchIndexManifestParams, build_sharded_arch_index_manifest,
    },
    transport::OciTransport,
};
use bytes::Bytes;
use http::{HeaderValue, StatusCode};
use nixcache_core::{
    NUM_SHARDS, ShardDataPayload, ShardDescriptor, ShardedArchCacheIndexData, SystemArch,
};
use tracing::info;

/// Image Index 和分片索引领域客户端。
pub struct IndexClient<'a, T: OciTransport> {
    pub(super) client: &'a OciClient<T>,
}

impl<'a, T: OciTransport + Clone> IndexClient<'a, T> {
    pub(super) fn new(client: &'a OciClient<T>) -> Self {
        Self { client }
    }

    pub async fn get_image_manifest(
        &self,
        tag: &str,
    ) -> Result<Option<(OciImageManifest, String)>, OciError> {
        match self.client.manifests().get_with_digest(tag).await? {
            Some((json, digest)) => {
                let manifest = serde_json::from_str::<OciImageManifest>(&json)?;
                manifest.validate_for(self.client.limits(), tag)?;
                Ok(Some((manifest, digest)))
            }
            None => Ok(None),
        }
    }

    pub async fn get_image_index(
        &self,
        tag: &str,
    ) -> Result<Option<(OciImageIndex, String)>, OciError> {
        match self.client.manifests().get_with_digest(tag).await? {
            Some((json, digest)) => {
                let index = serde_json::from_str::<OciImageIndex>(&json)?;
                index.validate_for(self.client.limits(), tag)?;
                Ok(Some((index, digest)))
            }
            None => Ok(None),
        }
    }

    pub async fn get_sharded_root(
        &self,
        tag: &str,
        system: &SystemArch,
    ) -> Result<Option<(ShardedArchCacheIndexData, String)>, OciError> {
        let arch_tag = if tag.ends_with(system.as_str()) {
            tag.to_string()
        } else {
            format!("{}-{}", tag, system.as_str())
        };

        if let Some((sub_manifest, sub_digest)) = self.get_image_manifest(&arch_tag).await? {
            let root_data = self
                .decode_root_manifest(&sub_manifest, system, &arch_tag)
                .await?;
            return Ok(Some((root_data, sub_digest)));
        }

        let artifact = match self.client.manifests().fetch_artifact(tag).await? {
            Some(artifact) => artifact,
            None => return Ok(None),
        };
        match artifact.manifest {
            OciArtifactManifest::Index(index) => {
                let descriptor = match index.find_manifest_for_system(system) {
                    Some(descriptor) => descriptor,
                    None => return Ok(None),
                };
                let descriptor_system = descriptor
                    .platform
                    .as_ref()
                    .map(|platform| platform.to_system())
                    .or_else(|| {
                        descriptor
                            .annotations
                            .as_ref()
                            .and_then(|annotations| annotations.get("org.nixos.nixcache.system"))
                            .map(|system| SystemArch::from(system.as_str()))
                    });
                if descriptor_system != Some(*system) {
                    return Err(OciError::InvalidDescriptor {
                        target: descriptor.digest.clone(),
                        details: "image index descriptor platform does not match the request"
                            .to_string(),
                    });
                }
                let (manifest_json, _) = self
                    .client
                    .manifests()
                    .get_with_digest_and_size(&descriptor.digest, descriptor.size)
                    .await?
                    .ok_or_else(|| OciError::SubManifestMissing {
                        digest: descriptor.digest.clone(),
                    })?;
                let sub_manifest: OciImageManifest = serde_json::from_str(&manifest_json)?;
                sub_manifest.validate_for(self.client.limits(), &descriptor.digest)?;
                let root_data = self
                    .decode_root_manifest(&sub_manifest, system, &descriptor.digest)
                    .await?;
                Ok(Some((root_data, artifact.digest)))
            }
            OciArtifactManifest::Manifest(manifest) => {
                let root_data = self.decode_root_manifest(&manifest, system, tag).await?;
                Ok(Some((root_data, artifact.digest)))
            }
        }
    }

    async fn decode_root_manifest(
        &self,
        manifest: &OciImageManifest,
        system: &SystemArch,
        target: &str,
    ) -> Result<ShardedArchCacheIndexData, OciError> {
        let merkle_root = manifest
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get("org.nixos.nixcache.merkle_root"))
            .ok_or_else(|| OciError::InvalidDescriptor {
                target: target.to_string(),
                details: "Schema v8 root manifest merkle_root annotation is missing".to_string(),
            })?;
        let layer =
            manifest.validate_schema_v8_root(system, merkle_root, self.client.limits(), target)?;
        let blob_bytes = self.client.blobs().get_descriptor(layer).await?;
        let decoded = IndexCodec::decode_zstd(
            &blob_bytes,
            &layer.media_type,
            self.client.limits().max_index_uncompressed_bytes(),
        )?;
        let root_data: ShardedArchCacheIndexData = decoded.value;
        root_data.validate_for(
            system,
            self.client.repo(),
            self.client.endpoint().authority(),
            self.client.limits(),
        )?;
        for shard in &root_data.shards {
            if !shard.is_empty()
                && shard.compressed_size > self.client.limits().max_buffered_blob_bytes()
            {
                return Err(OciError::SizeLimitExceeded {
                    target: shard.blob_digest.clone(),
                    limit: self.client.limits().max_buffered_blob_bytes(),
                    actual: shard.compressed_size,
                });
            }
        }
        if root_data.merkle_root != *merkle_root {
            return Err(OciError::DigestMismatch {
                target: target.to_string(),
                expected: merkle_root.clone(),
                actual: root_data.merkle_root,
            });
        }
        Ok(root_data)
    }

    pub async fn push_sharded_root(
        &self,
        tag: &str,
        root_data: &ShardedArchCacheIndexData,
    ) -> Result<String, OciError> {
        root_data.validate_structure()?;
        let (root_blob_digest, root_compressed_size, _) =
            self.client.blobs().push_zstd(root_data).await?;
        let manifest = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
            root_blob_digest: &root_blob_digest,
            root_blob_size: root_compressed_size,
            config_digest: EMPTY_CONFIG_DIGEST,
            config_size: EMPTY_CONFIG_SIZE,
            system: &root_data.system,
            merkle_root: &root_data.merkle_root,
        });
        let manifest_json = manifest.to_json_string()?;
        self.client.manifests().put(tag, &manifest_json).await?;
        Ok(endpoint::compute_sha256_digest(manifest_json.as_bytes()))
    }

    pub async fn push_sharded_root_cas(
        &self,
        tag: &str,
        root_data: &ShardedArchCacheIndexData,
        condition: ManifestCasCondition,
    ) -> Result<String, OciError> {
        if self.client.capabilities().manifest_cas_support != ManifestCasSupport::IfMatch {
            return Err(OciError::CasUnsupported {
                tag: tag.to_string(),
                backend: self.client.kind(),
            });
        }
        root_data.validate_structure()?;
        let (root_blob_digest, root_compressed_size, _) =
            self.client.blobs().push_zstd(root_data).await?;
        let manifest = build_sharded_arch_index_manifest(ShardedArchIndexManifestParams {
            root_blob_digest: &root_blob_digest,
            root_blob_size: root_compressed_size,
            config_digest: EMPTY_CONFIG_DIGEST,
            config_size: EMPTY_CONFIG_SIZE,
            system: &root_data.system,
            merkle_root: &root_data.merkle_root,
        });
        let manifest_json = manifest.to_json_string()?;
        self.client
            .manifests()
            .put_cas(tag, &manifest_json, condition)
            .await?;
        Ok(endpoint::compute_sha256_digest(manifest_json.as_bytes()))
    }

    pub async fn get_shard_data(
        &self,
        descriptor: &ShardDescriptor,
        system: &SystemArch,
    ) -> Result<ShardDataPayload, OciError> {
        descriptor.validate_structure()?;
        if descriptor.entry_count == 0 {
            if descriptor.shard_id >= NUM_SHARDS as u16
                || !descriptor.blob_digest.is_empty()
                || descriptor.compressed_size != 0
                || descriptor.uncompressed_size != 0
                || descriptor.merkle_hash
                    != ShardDataPayload::new(descriptor.shard_id)?.compute_merkle_hash()?
            {
                return Err(OciError::InvalidDescriptor {
                    target: descriptor.shard_id.to_string(),
                    details: "empty shard descriptor has non-empty metadata".to_string(),
                });
            }
            return Ok(ShardDataPayload::new(descriptor.shard_id)?);
        }
        let blob_descriptor = OciDescriptor {
            media_type: CacheLayerMediaTypeV8::SHARD_DATA_V8_ZSTD.to_string(),
            digest: descriptor.blob_digest.clone(),
            size: descriptor.compressed_size,
            platform: None,
            annotations: None,
        };
        let blob_bytes = self.client.blobs().get_descriptor(&blob_descriptor).await?;
        let decoded = IndexCodec::decode_zstd(
            &blob_bytes,
            CacheLayerMediaTypeV8::SHARD_DATA_V8_ZSTD,
            self.client.limits().max_index_uncompressed_bytes(),
        )?;
        if decoded.uncompressed_size != descriptor.uncompressed_size {
            return Err(OciError::SizeMismatch {
                target: descriptor.blob_digest.clone(),
                expected: descriptor.uncompressed_size,
                actual: decoded.uncompressed_size,
            });
        }
        let payload: ShardDataPayload = decoded.value;
        payload.validate_for(descriptor.shard_id, system, self.client.limits())?;
        if payload.len() != descriptor.entry_count {
            return Err(OciError::SizeMismatch {
                target: descriptor.blob_digest.clone(),
                expected: descriptor.entry_count as u64,
                actual: payload.len() as u64,
            });
        }
        payload.validate_merkle_hash(&descriptor.merkle_hash)?;
        Ok(payload)
    }

    pub async fn push_shard_data(
        &self,
        payload: &ShardDataPayload,
    ) -> Result<(String, u64, u64), OciError> {
        payload.validate_structure()?;
        self.client.blobs().push_zstd(payload).await
    }

    pub async fn put(&self, tag: &str, index: &OciImageIndex) -> Result<(), OciError> {
        let url = endpoint::manifest_url(self.client.endpoint(), self.client.repo(), tag);
        let mut headers = self.client.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static(OCI_IMAGE_INDEX_MEDIA_TYPE),
        );
        let index_json = index.to_json_string()?;
        let status = self
            .client
            .request_put_bytes_with_auth_retry(
                &url,
                headers,
                Bytes::copy_from_slice(index_json.as_bytes()),
                "push image index",
            )
            .await?;
        if matches!(
            status,
            StatusCode::OK | StatusCode::CREATED | StatusCode::ACCEPTED
        ) {
            info!("Successfully pushed OCI Image Index for tag {}", tag);
            Ok(())
        } else {
            Err(OciError::ManifestPushFailed(status))
        }
    }

    pub async fn put_cas(
        &self,
        tag: &str,
        index: &OciImageIndex,
        condition: ManifestCasCondition,
    ) -> Result<(), OciError> {
        if self.client.capabilities().manifest_cas_support != ManifestCasSupport::IfMatch {
            return Err(OciError::CasUnsupported {
                tag: tag.to_string(),
                backend: self.client.kind(),
            });
        }
        let url = endpoint::manifest_url(self.client.endpoint(), self.client.repo(), tag);
        let mut headers = self.client.get_auth_headers().await?;
        headers.insert(
            "Content-Type",
            HeaderValue::from_static(OCI_IMAGE_INDEX_MEDIA_TYPE),
        );
        let expected = match condition {
            ManifestCasCondition::CreateOnly => {
                headers.insert("If-None-Match", HeaderValue::from_static("*"));
                None
            }
            ManifestCasCondition::Match(expected) => {
                let value = HeaderValue::from_str(&expected)
                    .map_err(|_| TransportError::HeaderParse { header: "If-Match" })?;
                headers.insert(http::header::IF_MATCH, value);
                Some(expected)
            }
        };
        let index_json = index.to_json_string()?;
        let status = self
            .client
            .request_put_bytes_with_auth_retry(
                &url,
                headers,
                Bytes::copy_from_slice(index_json.as_bytes()),
                "push image index with CAS",
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
            info!("Successfully pushed OCI Image Index for tag {}", tag);
            Ok(())
        } else {
            Err(OciError::ManifestPushFailed(status))
        }
    }
}
