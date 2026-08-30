pub mod driver;
pub mod kind;

pub use driver::{
    AwsEcrDriver, AzureAcrDriver, DockerHubDriver, GcpArtifactRegistryDriver, GenericOciDriver,
    GhcrDriver, OciBackendDriver, OciDriver, detect_driver, driver_for_kind,
};
pub use kind::{BlobUploadStrategy, RegistryCapabilities, RegistryKind};
