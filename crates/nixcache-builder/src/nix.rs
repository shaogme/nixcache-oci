use crate::error::BuilderError;

pub mod discovery;
pub mod driver;
pub mod exporter;
pub mod filter;

pub use discovery::{discover_outputs, resolve_flake_output_hashes};
pub use driver::{BuildConfig, BuildMode, BuildTarget, NixCli};
pub use exporter::{ParallelExportConfig, ParallelExporter};

/// 便捷函数：获取当前系统架构
pub async fn get_system() -> Result<String, BuilderError> {
    NixCli.current_system().await
}

/// 便捷函数：构建目标输出
pub async fn build_outputs(targets: &[BuildTarget]) -> Result<Vec<String>, BuilderError> {
    NixCli.build_outputs(targets).await
}

/// 便捷函数：查找本地构建路径
pub async fn find_locally_built_paths(
    paths: &[String],
    own_hashes: &[String],
) -> Result<Vec<String>, BuilderError> {
    NixCli.find_locally_built_paths(paths, own_hashes).await
}
