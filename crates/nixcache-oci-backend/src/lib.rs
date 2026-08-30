pub use nixcache_oci::backend::{
    AwsEcrDriver, AzureAcrDriver, BlobUploadStrategy, DockerHubDriver, GcpArtifactRegistryDriver,
    GenericOciDriver, GhcrDriver, OciBackendDriver, OciDriver, RegistryCapabilities, RegistryKind,
    detect_driver, driver_for_kind,
};

#[cfg(feature = "tokio-reqwest")]
#[path = "tokio-reqwest.rs"]
pub mod tokio_reqwest;

#[cfg(feature = "tokio-reqwest")]
pub use tokio_reqwest::{
    OciClientExt, ReqwestTransport, create_tokio_reqwest_client,
    create_tokio_reqwest_client_from_kind, create_tokio_reqwest_client_with_driver,
};
