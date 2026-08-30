use crate::args::{auth::AuthTokenArgs, oci::OciTargetArgs, selector::CacheSelectorArgs};
use clap::{Args, ValueEnum};
use nixcache_core::{SortBy, SortOrder};
use nixcache_utils::Env;
use serde::{Deserialize, Serialize};

/// 输出格式枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
    Ndjson,
    Paths,
    Summary,
}

/// 构建缓存查询与列表参数组
#[derive(Args, Debug, Clone, Default)]
pub struct ListArgs {
    #[command(flatten)]
    pub oci: OciTargetArgs,

    #[command(flatten)]
    pub auth: AuthTokenArgs,

    #[command(flatten)]
    pub selector: CacheSelectorArgs,

    #[arg(
        long,
        default_value = "cache-index",
        help = "Target OCI tag to inspect [env: NIXCACHE_TARGET_TAG]"
    )]
    pub target_tag: Option<String>,

    #[arg(
        long,
        value_enum,
        default_value_t = OutputFormat::Table,
        help = "Output format [env: NIXCACHE_OUTPUT_FORMAT]"
    )]
    pub format: OutputFormat,

    #[arg(
        long,
        help = "Sort by field (date, size, name, hash) [env: NIXCACHE_SORT_BY]"
    )]
    pub sort_by: Option<String>,

    #[arg(long, help = "Sort order (desc, asc) [env: NIXCACHE_SORT_ORDER]")]
    pub sort_order: Option<String>,

    #[arg(
        long,
        help = "Limit the number of displayed items [env: NIXCACHE_LIMIT]"
    )]
    pub limit: Option<usize>,

    #[arg(
        long,
        help = "Show verbose details including references and digests [env: NIXCACHE_DETAILS]"
    )]
    pub details: bool,
}

impl ListArgs {
    pub fn resolve_target_tag(&self) -> String {
        self.target_tag
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_TARGET_TAG"))
            .unwrap_or_else(|| "cache-index".to_string())
    }

    pub fn resolve_format(&self) -> OutputFormat {
        if let Some(env_fmt) = Env::get("NIXCACHE_OUTPUT_FORMAT") {
            match env_fmt.to_ascii_lowercase().as_str() {
                "json" => OutputFormat::Json,
                "ndjson" => OutputFormat::Ndjson,
                "paths" | "path" => OutputFormat::Paths,
                "summary" => OutputFormat::Summary,
                "table" => OutputFormat::Table,
                _ => self.format,
            }
        } else {
            self.format
        }
    }

    pub fn resolve_sort_by(&self) -> SortBy {
        let field_str = self
            .sort_by
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_ascii_lowercase())
            .or_else(|| Env::get("NIXCACHE_SORT_BY").map(|s| s.to_ascii_lowercase()))
            .unwrap_or_else(|| "date".to_string());

        match field_str.as_str() {
            "size" | "narsize" | "bytes" => SortBy::NarSize,
            "name" | "package" | "pkg" => SortBy::Name,
            "hash" | "storehash" => SortBy::StoreHash,
            _ => SortBy::AddedDate,
        }
    }

    pub fn resolve_sort_order(&self) -> SortOrder {
        let order_str = self
            .sort_order
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_ascii_lowercase())
            .or_else(|| Env::get("NIXCACHE_SORT_ORDER").map(|s| s.to_ascii_lowercase()))
            .unwrap_or_else(|| "desc".to_string());

        match order_str.as_str() {
            "asc" | "ascending" => SortOrder::Asc,
            _ => SortOrder::Desc,
        }
    }

    pub fn resolve_limit(&self) -> Option<usize> {
        self.limit.or_else(|| Env::parse("NIXCACHE_LIMIT"))
    }

    pub fn resolve_details(&self) -> bool {
        if self.details {
            return true;
        }
        Env::get_bool("NIXCACHE_DETAILS").unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_args_resolution() {
        let args = ListArgs {
            target_tag: Some("run-123456".to_string()),
            format: OutputFormat::Json,
            sort_by: Some("size".to_string()),
            sort_order: Some("asc".to_string()),
            limit: Some(25),
            details: true,
            ..Default::default()
        };

        assert_eq!(args.resolve_target_tag(), "run-123456");
        assert_eq!(args.resolve_format(), OutputFormat::Json);
        assert_eq!(args.resolve_sort_by(), SortBy::NarSize);
        assert_eq!(args.resolve_sort_order(), SortOrder::Asc);
        assert_eq!(args.resolve_limit(), Some(25));
        assert!(args.resolve_details());
    }
}
