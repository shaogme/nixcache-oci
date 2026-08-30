use crate::args::{auth::AuthTokenArgs, oci::OciTargetArgs, selector::CacheSelectorArgs};
use clap::Args;
use nixcache_core::{CacheSelector, CascadeMode, StoreHash};
use nixcache_utils::Env;

/// 构建缓存主动清理与失效参数组
#[derive(Args, Debug, Clone, Default)]
pub struct PurgeArgs {
    #[command(flatten)]
    pub oci: OciTargetArgs,

    #[command(flatten)]
    pub auth: AuthTokenArgs,

    #[command(flatten)]
    pub selector: CacheSelectorArgs,

    #[arg(
        long,
        help = "Attempt physical OCI blob deletion [env: NIXCACHE_DELETE_BLOBS]"
    )]
    pub delete_blobs: bool,

    #[arg(
        long,
        help = "Allow skipping physical blob deletion if registry does not support it (e.g. GHCR) [env: NIXCACHE_ALLOW_UNSUPPORTED_BLOB_DELETION]"
    )]
    pub allow_unsupported_blob_deletion: bool,

    #[arg(
        long,
        default_missing_value = "true",
        num_args = 0..=1,
        help = "Strict error handling mode (default true) [env: NIXCACHE_STRICT]"
    )]
    pub strict: Option<bool>,

    #[arg(long, help = "Disable strict error handling mode")]
    pub no_strict: bool,

    #[arg(
        long,
        help = "Dry run mode (preview deletions without applying) [env: NIXCACHE_DRY_RUN]"
    )]
    pub dry_run: bool,
}

impl PurgeArgs {
    pub fn resolve_delete_blobs(&self) -> bool {
        if self.delete_blobs {
            return true;
        }
        Env::get_bool("NIXCACHE_DELETE_BLOBS").unwrap_or(false)
    }

    pub fn resolve_allow_unsupported_blob_deletion(&self) -> bool {
        if self.allow_unsupported_blob_deletion {
            return true;
        }
        Env::get_bool("NIXCACHE_ALLOW_UNSUPPORTED_BLOB_DELETION").unwrap_or(false)
    }

    pub fn resolve_strict(&self) -> bool {
        if self.no_strict {
            return false;
        }
        if let Some(s) = self.strict {
            return s;
        }
        Env::get_bool("NIXCACHE_STRICT").unwrap_or(true)
    }

    pub fn resolve_dry_run(&self) -> bool {
        if self.dry_run {
            return true;
        }
        Env::get_bool("NIXCACHE_DRY_RUN").unwrap_or(false)
    }

    /// 转换为 nixcache-core 的 CacheSelector 结构体 (Purge 默认采用 Dependents 级联)
    pub fn to_purge_filter(&self, extra_hashes: &[StoreHash]) -> CacheSelector {
        self.selector
            .to_selector(extra_hashes, CascadeMode::Dependents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixcache_core::{SizeFilter, SystemArch};

    #[test]
    fn test_purge_args_resolution_and_parsing() {
        let args = PurgeArgs {
            selector: CacheSelectorArgs {
                all: false,
                hashes: Some(
                    "s66mzxpvicwk07gjbjfw9izjfa797vsw,00000000000000000000000000000001".to_string(),
                ),
                patterns: vec!["*chromium*".to_string()],
                system: vec!["x86_64-linux".to_string()],
                older_than: Some("30d".to_string()),
                cascade: Some("transitive".to_string()),
                min_size: Some("500M".to_string()),
                origin_job: vec!["job1".to_string()],
                origin_run: vec![12345],
                protect_gc_roots: true,
                ..Default::default()
            },
            delete_blobs: true,
            allow_unsupported_blob_deletion: true,
            strict: Some(true),
            no_strict: false,
            dry_run: true,
            ..Default::default()
        };

        assert!(!args.selector.resolve_all());
        assert_eq!(args.selector.resolve_hashes().len(), 2);
        assert_eq!(args.selector.resolve_patterns(), vec!["*chromium*"]);
        assert_eq!(
            args.selector.resolve_systems(),
            vec![SystemArch::X86_64Linux]
        );
        assert!(args.selector.resolve_older_than().is_some());
        assert_eq!(
            args.selector.resolve_cascade(CascadeMode::Dependents),
            CascadeMode::Transitive
        );
        assert_eq!(args.selector.resolve_min_size(), Some(500 * 1024 * 1024));
        assert!(args.selector.resolve_origin_jobs().contains("job1"));
        assert!(args.selector.resolve_origin_runs().contains(&12345));
        assert!(args.resolve_delete_blobs());
        assert!(args.resolve_allow_unsupported_blob_deletion());
        assert!(args.resolve_strict());
        assert!(args.selector.resolve_protect_gc_roots());
        assert!(args.resolve_dry_run());

        let selector = args.to_purge_filter(&[]);
        assert_eq!(selector.store_hashes.len(), 2);
        assert_eq!(selector.cascade_mode, CascadeMode::Transitive);
        assert!(selector.protect_gc_roots);
        assert_eq!(
            selector.size_filter,
            Some(SizeFilter::MinBytes(500 * 1024 * 1024))
        );
    }

    #[test]
    fn test_purge_args_clap_parse_strict_flag() {
        use clap::Parser;

        #[derive(Parser, Debug)]
        struct TestPurgeCli {
            #[command(flatten)]
            purge: PurgeArgs,
        }

        let parsed = TestPurgeCli::try_parse_from(["test-purge", "--strict"]).unwrap();
        assert_eq!(parsed.purge.strict, Some(true));
        assert!(parsed.purge.resolve_strict());

        let parsed_no_strict = TestPurgeCli::try_parse_from(["test-purge", "--no-strict"]).unwrap();
        assert_eq!(parsed_no_strict.purge.strict, None);
        assert!(parsed_no_strict.purge.no_strict);
        assert!(!parsed_no_strict.purge.resolve_strict());
    }
}
