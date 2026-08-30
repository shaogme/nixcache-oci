use nixcache_core::StoreHash;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

/// 强类型单个 Nix Store Path 的元数据解析结构 (零动态 DOM 分配)
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NixPathInfoItem {
    /// 完整 store path，例如 `/nix/store/s66mzxpvicwk07gjbjfw9izjfa797vsw-hello-2.12.1`
    #[serde(default)]
    pub path: String,

    /// NAR 包散列值 (如 `sha256:1x94w57b8q8q...`)
    #[serde(default)]
    pub nar_hash: String,

    /// NAR 原始未压缩字节大小
    #[serde(default)]
    pub nar_size: u64,

    /// 直接运行时依赖引用列表
    #[serde(default)]
    pub references: Vec<String>,

    /// 关联的 .drv 路径 (若有)
    #[serde(default)]
    pub deriver: Option<String>,

    /// 签名列表 (支持 `signatures` 和 `sigs` 双别名)
    #[serde(default, alias = "sigs")]
    pub signatures: Vec<String>,

    /// 内容寻址产物标记 (CA)
    #[serde(default)]
    pub ca: Option<String>,

    /// 注册时间戳
    #[serde(default)]
    pub registration_time: Option<u64>,
}

impl NixPathInfoItem {
    /// 提取 32 字符 StoreHash
    pub fn store_hash(&self) -> Option<StoreHash> {
        let file_name = Path::new(&self.path).file_name()?.to_str()?;
        if file_name.len() >= 32 {
            StoreHash::parse(&file_name[..32]).ok()
        } else {
            None
        }
    }

    /// 提取所有引用的文件名（不带目录前缀）
    pub fn normalized_references(&self) -> Vec<String> {
        self.references
            .iter()
            .map(|r| {
                Path::new(r)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(r)
                    .to_string()
            })
            .collect()
    }

    /// 判断是否匹配任何受信任的额外上游签名（如 cache.nixos.org-1）
    pub fn has_upstream_signature(&self, trusted_upstream_prefixes: &[String]) -> bool {
        if self.signatures.is_empty() || trusted_upstream_prefixes.is_empty() {
            return false;
        }
        self.signatures.iter().any(|sig| {
            let sig_prefix = sig.split(':').next().unwrap_or(sig);
            trusted_upstream_prefixes
                .iter()
                .any(|prefix| prefix == sig_prefix)
        })
    }

    /// 判断是否具备任何外部签名（即非本仓库信任的签名）
    pub fn has_external_signature(&self, own_public_key: Option<&str>) -> bool {
        if self.signatures.is_empty() {
            return false;
        }

        match own_public_key {
            Some(own_key) => {
                let own_key_prefix = own_key.split(':').next().unwrap_or(own_key);
                // 如果存在任何一个签名不属于本仓库公钥，则判定为外部代换签名
                self.signatures.iter().any(|sig| {
                    let sig_prefix = sig.split(':').next().unwrap_or(sig);
                    sig_prefix != own_key_prefix
                })
            }
            None => {
                // 如果本地未配置自签名私钥，任何签名均视为外部签名
                !self.signatures.is_empty()
            }
        }
    }
}

/// 产物判决归类状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactClassification {
    /// 本地真正新编译构建的产物 (需要导出与上传)
    LocallyBuilt,
    /// 来自官方上游或外部 Substituter 的代换包 (直接跳过)
    Substituted { external_signatures: Vec<String> },
    /// 已存在于远端 OCI 缓存 (Session 或 Baseline) 的产物 (直接跳过)
    AlreadyCached { store_hash: StoreHash },
    /// 临时或非法路径 (.drv, .lock, 短路径等)
    Ignored { reason: &'static str },
}

/// 先验过滤综合决策报告
#[derive(Debug, Clone, Default)]
pub struct FilterDecisionReport {
    /// 确定需要导出并上传的本地新构建产物
    pub to_export: Vec<NixPathInfoItem>,
    /// 被过滤掉的上游代换包数量
    pub substituted_count: usize,
    /// 被过滤掉的已缓存包数量
    pub already_cached_count: usize,
    /// 忽略的非法/临时文件数量
    pub ignored_count: usize,
    /// 详细路径决策映射 (用于 Trace 与 Receipt 记录)
    pub decisions: HashMap<String, ArtifactClassification>,
}

/// 强类型先验过滤器上下文
pub struct NixArtifactFilterContext<'a> {
    /// 本仓库自身的公钥 (用于识别自签名产物)
    pub own_public_key: Option<&'a str>,
    /// 远端 Baseline (cache-index) 与当前 Session 中已存在的 StoreHash 集合
    pub remote_cached_hashes: &'a HashSet<StoreHash>,
    /// 显式信任的额外上游签名列表前缀 (例如 `cache.nixos.org-1`, `nix-community.cachix.org-1`)
    pub trusted_upstream_prefixes: &'a [String],
}

pub struct NixArtifactFilter;

impl NixArtifactFilter {
    /// 执行多维先验过滤分类算法
    pub fn classify_and_filter(
        items: Vec<NixPathInfoItem>,
        ctx: &NixArtifactFilterContext<'_>,
    ) -> FilterDecisionReport {
        let mut report = FilterDecisionReport::default();

        for item in items {
            let path_str = item.path.clone();

            // 1. 基础合法性与后缀检查
            if path_str.ends_with(".drv")
                || path_str.ends_with(".lock")
                || path_str.ends_with(".check")
            {
                report.ignored_count += 1;
                report.decisions.insert(
                    path_str,
                    ArtifactClassification::Ignored {
                        reason: "drv/lock/check file",
                    },
                );
                continue;
            }

            let Some(sh) = item.store_hash() else {
                report.ignored_count += 1;
                report.decisions.insert(
                    path_str,
                    ArtifactClassification::Ignored {
                        reason: "invalid store hash format",
                    },
                );
                continue;
            };

            // 2. 远端已存在性先验检查 (Pre-filtering by StoreHash)
            if ctx.remote_cached_hashes.contains(&sh) {
                report.already_cached_count += 1;
                report.decisions.insert(
                    path_str,
                    ArtifactClassification::AlreadyCached { store_hash: sh },
                );
                continue;
            }

            // 3. 上游外部签名过滤检查 (Upstream Substituter Filtering)
            if item.has_upstream_signature(ctx.trusted_upstream_prefixes)
                || item.has_external_signature(ctx.own_public_key)
            {
                report.substituted_count += 1;
                report.decisions.insert(
                    path_str,
                    ArtifactClassification::Substituted {
                        external_signatures: item.signatures.clone(),
                    },
                );
                continue;
            }

            // 4. 判定为真正由本地编译产生的产物
            report
                .decisions
                .insert(path_str, ArtifactClassification::LocallyBuilt);
            report.to_export.push(item);
        }

        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证上游 cache.nixos.org 签名被精准剔除
    #[test]
    fn test_filter_upstream_cache_nixos_org_signatures() {
        let item = NixPathInfoItem {
            path: "/nix/store/11111111111111111111111111111111-glibc-2.38".to_string(),
            signatures: vec![
                "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=".to_string(),
            ],
            ..Default::default()
        };
        let cached = HashSet::new();
        let ctx = NixArtifactFilterContext {
            own_public_key: Some("my-repo-1:ABCD..."),
            remote_cached_hashes: &cached,
            trusted_upstream_prefixes: &["cache.nixos.org-1".to_string()],
        };

        let report = NixArtifactFilter::classify_and_filter(vec![item], &ctx);
        assert_eq!(report.to_export.len(), 0);
        assert_eq!(report.substituted_count, 1);
        assert_eq!(report.already_cached_count, 0);
        assert_eq!(report.ignored_count, 0);
    }

    /// 验证远端已存在的 StoreHash 被先验过滤
    #[test]
    fn test_filter_already_cached_store_hashes() {
        let sh = StoreHash::new_unchecked("22222222222222222222222222222222");
        let item = NixPathInfoItem {
            path: "/nix/store/22222222222222222222222222222222-my-app-1.0".to_string(),
            signatures: vec![],
            ..Default::default()
        };
        let mut cached = HashSet::new();
        cached.insert(sh.clone());

        let ctx = NixArtifactFilterContext {
            own_public_key: Some("my-repo-1:ABCD..."),
            remote_cached_hashes: &cached,
            trusted_upstream_prefixes: &[],
        };

        let report = NixArtifactFilter::classify_and_filter(vec![item], &ctx);
        assert_eq!(report.to_export.len(), 0);
        assert_eq!(report.already_cached_count, 1);
        assert_eq!(report.substituted_count, 0);
    }

    /// 验证本地无签名的真正新构建产物被保留
    #[test]
    fn test_filter_locally_built_unsigned_outputs() {
        let item = NixPathInfoItem {
            path: "/nix/store/33333333333333333333333333333333-new-service".to_string(),
            signatures: vec![],
            ..Default::default()
        };
        let cached = HashSet::new();
        let ctx = NixArtifactFilterContext {
            own_public_key: Some("my-repo-1:ABCD..."),
            remote_cached_hashes: &cached,
            trusted_upstream_prefixes: &[],
        };

        let report = NixArtifactFilter::classify_and_filter(vec![item], &ctx);
        assert_eq!(report.to_export.len(), 1);
        assert_eq!(
            report.to_export[0].path,
            "/nix/store/33333333333333333333333333333333-new-service"
        );
        assert_eq!(report.substituted_count, 0);
        assert_eq!(report.already_cached_count, 0);
    }

    /// 验证本仓库自签名的产物被保留为 LocallyBuilt
    #[test]
    fn test_filter_locally_built_own_signed_outputs() {
        let item = NixPathInfoItem {
            path: "/nix/store/44444444444444444444444444444444-own-signed".to_string(),
            signatures: vec!["my-repo-1:XYZ123...".to_string()],
            ..Default::default()
        };
        let cached = HashSet::new();
        let ctx = NixArtifactFilterContext {
            own_public_key: Some("my-repo-1:ABCD..."),
            remote_cached_hashes: &cached,
            trusted_upstream_prefixes: &[],
        };

        let report = NixArtifactFilter::classify_and_filter(vec![item], &ctx);
        assert_eq!(report.to_export.len(), 1);
        assert_eq!(
            report.to_export[0].path,
            "/nix/store/44444444444444444444444444444444-own-signed"
        );
    }

    /// 验证忽略临时与非法后缀文件
    #[test]
    fn test_filter_ignored_suffixes() {
        let items = vec![
            NixPathInfoItem {
                path: "/nix/store/55555555555555555555555555555555-app.drv".to_string(),
                ..Default::default()
            },
            NixPathInfoItem {
                path: "/nix/store/66666666666666666666666666666666-app.lock".to_string(),
                ..Default::default()
            },
            NixPathInfoItem {
                path: "/nix/store/77777777777777777777777777777777-app.check".to_string(),
                ..Default::default()
            },
            NixPathInfoItem {
                path: "/nix/store/short".to_string(),
                ..Default::default()
            },
        ];
        let cached = HashSet::new();
        let ctx = NixArtifactFilterContext {
            own_public_key: None,
            remote_cached_hashes: &cached,
            trusted_upstream_prefixes: &[],
        };

        let report = NixArtifactFilter::classify_and_filter(items, &ctx);
        assert_eq!(report.to_export.len(), 0);
        assert_eq!(report.ignored_count, 4);
    }
}
