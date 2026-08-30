pub mod driver;
pub mod kind;

pub use driver::{
    AwsEcrDriver, AzureAcrDriver, DockerHubDriver, GcpArtifactRegistryDriver, GenericOciDriver,
    GhcrDriver, OciBackendDriver, detect_driver, driver_for_kind,
};
pub use kind::{BlobUploadStrategy, RegistryCapabilities, RegistryKind};
