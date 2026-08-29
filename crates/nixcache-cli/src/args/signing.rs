use clap::Args;
use nixcache_utils::Env;
use std::path::PathBuf;

/// 签名密钥文件参数组
#[derive(Args, Debug, Clone, Default)]
pub struct SigningKeyArgs {
    #[arg(
        long,
        help = "Path to signing key file [env: NIXCACHE_SIGNING_KEY_FILE]"
    )]
    pub signing_key_file: Option<PathBuf>,
}

impl SigningKeyArgs {
    /// 解析签名密钥文件路径（支持 NIXCACHE_SIGNING_KEY_FILE 环境变量）
    pub fn resolve_signing_key_file(&self) -> Option<PathBuf> {
        self.signing_key_file
            .as_deref()
            .and_then(Env::non_empty_path)
            .map(PathBuf::from)
            .or_else(|| Env::get_path("NIXCACHE_SIGNING_KEY_FILE"))
    }

    /// 解析为字符串形式（用于向下游传递）
    pub fn resolve_signing_key_str(&self) -> Option<String> {
        self.resolve_signing_key_file()
            .map(|p| p.to_string_lossy().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::SigningKeyArgs;
    use std::{env, path::PathBuf};

    #[test]
    fn test_signing_key_resolution() {
        let empty = SigningKeyArgs::default();
        assert_eq!(empty.resolve_signing_key_file(), None);

        let explicit = SigningKeyArgs {
            signing_key_file: Some(PathBuf::from("/etc/nix/secret.key")),
        };
        assert_eq!(
            explicit.resolve_signing_key_file(),
            Some(PathBuf::from("/etc/nix/secret.key"))
        );

        unsafe {
            env::set_var("NIXCACHE_SIGNING_KEY_FILE", "/tmp/env.key");
        }
        assert_eq!(
            empty.resolve_signing_key_file(),
            Some(PathBuf::from("/tmp/env.key"))
        );

        unsafe {
            env::remove_var("NIXCACHE_SIGNING_KEY_FILE");
        }
    }
}
