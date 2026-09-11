pub mod driver;
pub mod endpoint;
pub mod ghcr;
pub mod kind;

pub use driver::{
    AwsEcrDriver, AzureAcrDriver, DockerHubDriver, GcpArtifactRegistryDriver, GenericOciDriver,
    GhcrDriver, OciBackendDriver, OciDriver, detect_driver, driver_for_kind,
};
pub use endpoint::{RegistryEndpoint, RegistryEndpointError, RegistryScheme};
pub use ghcr::{
    GitHubContainerMetadata, GitHubPackageVersion, GitHubPackageVersionMetadata,
    GitHubPackagesClient,
};
pub use kind::{
    BlobUploadStrategy, ManifestCasSupport, PackageDeletionSupport, RegistryCapabilities,
    RegistryDeletionStrategy, RegistryKind,
};
