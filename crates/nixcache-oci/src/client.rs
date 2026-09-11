mod blob;
mod deletion;
mod endpoint;
mod index;
mod manifest;
mod request;
mod upload;

pub use blob::BlobClient;
pub use deletion::{DeletionClient, DeletionOutcome, DeletionSummary, PackageDeletionSummary};
pub use index::IndexClient;
pub use manifest::{FetchedOciArtifact, ManifestCasCondition, ManifestClient};

use crate::{
    auth::RegistryCredentials,
    backend::{
        OciDriver, RegistryCapabilities, RegistryEndpoint, RegistryKind, detect_driver,
        driver_for_kind,
    },
    error::OciError,
    limits::OciReadLimits,
    token::TokenManager,
    transport::OciTransport,
};
use std::{convert::TryFrom, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Shared OCI registry context. Domain operations are exposed through the borrowed clients.
#[derive(Clone)]
pub struct OciClient<T: OciTransport> {
    endpoint: RegistryEndpoint,
    repo: String,
    driver: OciDriver,
    token_manager: TokenManager,
    transport: T,
    limits: OciReadLimits,
    index_read_semaphore: Arc<Semaphore>,
}

impl<T: OciTransport + Clone> OciClient<T> {
    /// 基于指定驱动构造 OCI 客户端。
    pub fn new(
        registry: &str,
        repo: &str,
        credentials: impl Into<RegistryCredentials>,
        write_access: bool,
        driver: impl Into<OciDriver>,
        transport: T,
        limits: OciReadLimits,
    ) -> Result<Self, OciError> {
        limits
            .validate()
            .expect("OciReadLimits must be constructed through a checked constructor");
        let driver = driver.into();
        let endpoint = driver.canonicalize_endpoint(registry)?;
        let canonical_repo = driver.canonicalize_repository(repo);
        let token_manager = TokenManager::new(
            &endpoint,
            &canonical_repo,
            credentials,
            write_access,
            driver,
        );

        Ok(Self {
            endpoint,
            repo: canonical_repo,
            driver,
            token_manager,
            transport,
            index_read_semaphore: Arc::new(Semaphore::new(
                usize::try_from(limits.max_parallel_index_reads())
                    .expect("validated parallel read limit must fit usize"),
            )),
            limits,
        })
    }

    /// 基于指定的 RegistryKind 构造 OCI 客户端。
    pub fn from_kind(
        kind: RegistryKind,
        registry: &str,
        repo: &str,
        credentials: impl Into<RegistryCredentials>,
        write_access: bool,
        transport: T,
        limits: OciReadLimits,
    ) -> Result<Self, OciError> {
        Self::new(
            registry,
            repo,
            credentials,
            write_access,
            driver_for_kind(kind),
            transport,
            limits,
        )
    }

    /// 自动根据 registry 域名推导后端类型并构造 OCI 客户端。
    pub fn with_transport(
        registry: &str,
        repo: &str,
        credentials: impl Into<RegistryCredentials>,
        write_access: bool,
        transport: T,
        limits: OciReadLimits,
    ) -> Result<Self, OciError> {
        Self::new(
            registry,
            repo,
            credentials,
            write_access,
            detect_driver(registry),
            transport,
            limits,
        )
    }

    pub fn driver(&self) -> &OciDriver {
        &self.driver
    }

    pub fn kind(&self) -> RegistryKind {
        self.driver.kind()
    }

    pub fn capabilities(&self) -> &'static RegistryCapabilities {
        self.driver.capabilities()
    }

    pub fn endpoint(&self) -> &RegistryEndpoint {
        &self.endpoint
    }

    pub fn authority(&self) -> &str {
        self.endpoint.authority()
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn limits(&self) -> &OciReadLimits {
        &self.limits
    }

    pub(crate) async fn acquire_index_read(&self) -> OwnedSemaphorePermit {
        self.index_read_semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("OCI index read semaphore is never closed")
    }

    pub(crate) fn token_manager(&self) -> &TokenManager {
        &self.token_manager
    }

    pub fn blobs(&self) -> BlobClient<'_, T> {
        BlobClient::new(self)
    }

    pub fn manifests(&self) -> ManifestClient<'_, T> {
        ManifestClient::new(self)
    }

    pub fn indexes(&self) -> IndexClient<'_, T> {
        IndexClient::new(self)
    }

    pub fn deletion(&self) -> DeletionClient<'_, T> {
        DeletionClient::new(self)
    }
}
