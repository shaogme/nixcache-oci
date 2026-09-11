mod update;

use super::{ManifestCasCondition, OciClient, endpoint};
use crate::{
    backend::ManifestCasSupport,
    codec::IndexCodec,
    error::{OciError, TransportError},
    manifest::{
        CacheLayerMediaType, CacheLayerMediaTypeV6, EMPTY_CONFIG_DIGEST, EMPTY_CONFIG_SIZE,
        OCI_IMAGE_INDEX_MEDIA_TYPE, OciArtifactManifest, OciImageIndex, OciImageManifest,
        ShardedArchIndexManifestParams, build_sharded_arch_index_manifest,
    },
    transport::OciTransport,
};
use bytes::Bytes;
use http::{HeaderValue, StatusCode};
use nixcache_core::{ShardDataPayload, ShardedArchCacheIndexData, SystemArch};
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
            Some((json, digest)) => Ok(Some((serde_json::from_str(&json)?, digest))),
            None => Ok(None),
        }
    }

    pub async fn get_image_index(
        &self,
        tag: &str,
    ) -> Result<Option<(OciImageIndex, String)>, OciError> {
        match self.client.manifests().get_with_digest(tag).await? {
            Some((json, digest)) => Ok(Some((serde_json::from_str(&json)?, digest))),
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

        if let Some((sub_manifest, sub_digest)) = self.get_image_manifest(&arch_tag).await?
            && let Some(layer) = sub_manifest
                .layers
                .iter()
                .find(|layer| {
                    CacheLayerMediaType::parse(&layer.media_type)
                        .is_some_and(|media_type| media_type.is_root_index())
                })
                .or_else(|| sub_manifest.layers.first())
        {
            let blob_bytes = self.client.blobs().get(&layer.digest).await?;
            let root_data = IndexCodec::decode_zstd(&blob_bytes, &layer.media_type)?;
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
                let (manifest_json, _) = self
                    .client
                    .manifests()
                    .get_with_digest(&descriptor.digest)
                    .await?
                    .ok_or_else(|| OciError::SubManifestMissing {
                        digest: descriptor.digest.clone(),
                    })?;
                let sub_manifest: OciImageManifest = serde_json::from_str(&manifest_json)?;
                let layer = sub_manifest
                    .layers
                    .iter()
                    .find(|layer| {
                        CacheLayerMediaType::parse(&layer.media_type)
                            .is_some_and(|media_type| media_type.is_root_index())
                    })
                    .or_else(|| sub_manifest.layers.first())
                    .ok_or(OciError::LayerDescriptorMissing)?;
                let blob_bytes = self.client.blobs().get(&layer.digest).await?;
                let root_data = IndexCodec::decode_zstd(&blob_bytes, &layer.media_type)?;
                Ok(Some((root_data, artifact.digest)))
            }
            OciArtifactManifest::Manifest(manifest) => {
                let Some(layer) = manifest
                    .layers
                    .iter()
                    .find(|layer| {
                        CacheLayerMediaType::parse(&layer.media_type)
                            .is_some_and(|media_type| media_type.is_root_index())
                    })
                    .or_else(|| manifest.layers.first())
                else {
                    return Ok(None);
                };
                let blob_bytes = self.client.blobs().get(&layer.digest).await?;
                let root_data = IndexCodec::decode_zstd(&blob_bytes, &layer.media_type)?;
                Ok(Some((root_data, artifact.digest)))
            }
        }
    }

    pub async fn push_sharded_root(
        &self,
        tag: &str,
        root_data: &ShardedArchCacheIndexData,
    ) -> Result<String, OciError> {
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

    pub async fn get_shard_data(&self, blob_digest: &str) -> Result<ShardDataPayload, OciError> {
        let blob_bytes = self.client.blobs().get(blob_digest).await?;
        IndexCodec::decode_zstd(&blob_bytes, CacheLayerMediaTypeV6::SHARD_DATA_V6_ZSTD)
    }

    pub async fn push_shard_data(
        &self,
        payload: &ShardDataPayload,
    ) -> Result<(String, u64, u64), OciError> {
        self.client.blobs().push_zstd(payload).await
    }

    pub async fn put(&self, tag: &str, index: &OciImageIndex) -> Result<(), OciError> {
        let url = endpoint::manifest_url(
            self.client.url_scheme(),
            self.client.registry(),
            self.client.repo(),
            tag,
        );
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
        let url = endpoint::manifest_url(
            self.client.url_scheme(),
            self.client.registry(),
            self.client.repo(),
            tag,
        );
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
