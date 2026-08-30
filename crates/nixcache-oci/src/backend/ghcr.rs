use crate::{error::OciError, transport::OciTransport};
use http::{HeaderMap, HeaderValue, StatusCode};
use serde::{Deserialize, Serialize};
use std::{
    str::from_utf8,
    sync::atomic::{AtomicU8, Ordering},
};
use tracing::{debug, info};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GitHubContainerMetadata {
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GitHubPackageVersionMetadata {
    #[serde(default)]
    pub container: Option<GitHubContainerMetadata>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GitHubPackageVersion {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub metadata: Option<GitHubPackageVersionMetadata>,
}

/// GitHub Packages REST API 专用客户端 (处理 GHCR 上的包版本与 Tag 物理删除)
#[derive(Debug)]
pub struct GitHubPackagesClient<T: OciTransport> {
    transport: T,
    token: String,
    owner: String,
    package_name: String,
    /// 0 = 未知 (首次探测), 1 = 组织 (orgs), 2 = 用户 (users)
    owner_type: AtomicU8,
}

impl<T: OciTransport + Clone> Clone for GitHubPackagesClient<T> {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            token: self.token.clone(),
            owner: self.owner.clone(),
            package_name: self.package_name.clone(),
            owner_type: AtomicU8::new(self.owner_type.load(Ordering::Relaxed)),
        }
    }
}

impl<T: OciTransport> GitHubPackagesClient<T> {
    /// 构造 GitHubPackagesClient 实例
    pub fn new(transport: T, token: &str, repo_path: &str) -> Self {
        let clean = repo_path.trim_matches('/');
        let (owner, pkg) = match clean.split_once('/') {
            Some((o, rest)) => {
                let pkg_target = if rest.ends_with("/nix-cache") || rest == "nix-cache" {
                    rest.to_string()
                } else {
                    format!("{}/nix-cache", rest)
                };
                (o.to_string(), pkg_target.replace('/', "%2F"))
            }
            None => (clean.to_string(), "nix-cache".to_string()),
        };

        Self {
            transport,
            token: token.to_string(),
            owner,
            package_name: pkg,
            owner_type: AtomicU8::new(0),
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn package_name(&self) -> &str {
        &self.package_name
    }

    fn get_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Accept",
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        headers.insert("User-Agent", HeaderValue::from_static("nixcache-oci"));
        if !self.token.is_empty()
            && let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", self.token))
        {
            headers.insert("Authorization", val);
        }
        headers
    }

    fn format_url(&self, is_org: bool, endpoint_suffix: &str) -> String {
        let prefix = if is_org { "orgs" } else { "users" };
        let suffix = if endpoint_suffix.is_empty() {
            String::new()
        } else if endpoint_suffix.starts_with('/') || endpoint_suffix.starts_with('?') {
            endpoint_suffix.to_string()
        } else {
            format!("/{}", endpoint_suffix)
        };
        format!(
            "https://api.github.com/{}/{}/packages/container/{}{}",
            prefix, self.owner, self.package_name, suffix
        )
    }

    fn handle_status_error(&self, action: &str, status: StatusCode, body: &[u8]) -> OciError {
        let details = from_utf8(body).unwrap_or("<invalid utf-8>").trim();
        let target = format!("{}/{}", self.owner, self.package_name);
        if status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED {
            OciError::InsufficientPermission {
                target,
                required_scope: "delete:packages",
                details: format!(
                    "HTTP {} when performing {} on GHCR: {}. Remedy: Ensure your token has 'delete:packages' (PAT) or 'packages: write' (GitHub Actions) permissions.",
                    status, action, details
                ),
            }
        } else {
            OciError::DeletionFailed {
                target,
                status,
                details: format!("HTTP {} during {}: {}", status, action, details),
            }
        }
    }

    /// 列出当前 Package 的所有版本 (自动组织/用户智能寻路与分页聚合)
    pub async fn list_package_versions(&self) -> Result<Vec<GitHubPackageVersion>, OciError> {
        let mut all_versions = Vec::new();
        let mut page = 1;
        let cached_type = self.owner_type.load(Ordering::Relaxed);

        let try_orgs_first = cached_type != 2; // 0 或 1 先尝试 orgs

        loop {
            let url_suffix = format!("/versions?per_page=100&page={}", page);
            let is_org = if cached_type == 0 {
                try_orgs_first
            } else {
                cached_type == 1
            };

            let url = self.format_url(is_org, &url_suffix);
            let headers = self.get_headers();
            let (status, _resp_headers, body) = self.transport.get(&url, headers).await?;

            if status.is_success() {
                if cached_type == 0 {
                    self.owner_type
                        .store(if is_org { 1 } else { 2 }, Ordering::Relaxed);
                }
                let versions: Vec<GitHubPackageVersion> = serde_json::from_slice(&body)?;
                let count = versions.len();
                all_versions.extend(versions);
                if count < 100 {
                    break;
                }
                page += 1;
            } else if status == StatusCode::NOT_FOUND && cached_type == 0 && is_org {
                // Org 路由返回 404，回退尝试 User 路由
                debug!(
                    "GHCR org route 404 for owner {}, falling back to user route",
                    self.owner
                );
                let user_url = self.format_url(false, &url_suffix);
                let user_headers = self.get_headers();
                let (user_status, _resp_h, user_body) =
                    self.transport.get(&user_url, user_headers).await?;

                if user_status.is_success() {
                    self.owner_type.store(2, Ordering::Relaxed);
                    let versions: Vec<GitHubPackageVersion> = serde_json::from_slice(&user_body)?;
                    let count = versions.len();
                    all_versions.extend(versions);
                    if count < 100 {
                        break;
                    }
                    page += 1;
                } else if user_status == StatusCode::NOT_FOUND {
                    // 两边都是 404，说明包或用户不存在，返回空列表
                    return Ok(Vec::new());
                } else {
                    return Err(self.handle_status_error(
                        "list package versions",
                        user_status,
                        &user_body,
                    ));
                }
            } else if status == StatusCode::NOT_FOUND {
                // 已经确定类型或者是 user 路由返回 404，视为包不存在
                return Ok(Vec::new());
            } else {
                return Err(self.handle_status_error("list package versions", status, &body));
            }
        }

        Ok(all_versions)
    }

    /// 根据 Tag 查询对应的 Package Version ID
    pub async fn find_version_id_by_tag(&self, tag: &str) -> Result<Option<u64>, OciError> {
        let versions = self.list_package_versions().await?;
        for v in versions {
            if let Some(ref meta) = v.metadata
                && let Some(ref container) = meta.container
                && container.tags.iter().any(|t| t == tag)
            {
                return Ok(Some(v.id));
            }
        }
        Ok(None)
    }

    /// 删除指定 Version ID 的包版本 (同步移除关联的 Tag)
    pub async fn delete_package_version(&self, version_id: u64) -> Result<(), OciError> {
        let url_suffix = format!("/versions/{}", version_id);
        let cached_type = self.owner_type.load(Ordering::Relaxed);
        let is_org = cached_type != 2;

        let url = self.format_url(is_org, &url_suffix);
        let headers = self.get_headers();
        let status = self.transport.delete(&url, headers).await?;

        if status == StatusCode::NO_CONTENT
            || status == StatusCode::OK
            || status == StatusCode::ACCEPTED
        {
            if cached_type == 0 {
                self.owner_type
                    .store(if is_org { 1 } else { 2 }, Ordering::Relaxed);
            }
            info!(
                "Successfully deleted GHCR package version {} for {}",
                version_id, self.package_name
            );
            Ok(())
        } else if status == StatusCode::NOT_FOUND && cached_type == 0 && is_org {
            // 尝试用户路由
            let user_url = self.format_url(false, &url_suffix);
            let user_headers = self.get_headers();
            let user_status = self.transport.delete(&user_url, user_headers).await?;
            if user_status == StatusCode::NO_CONTENT
                || user_status == StatusCode::OK
                || user_status == StatusCode::ACCEPTED
                || user_status == StatusCode::NOT_FOUND
            {
                self.owner_type.store(2, Ordering::Relaxed);
                Ok(())
            } else {
                Err(self.handle_status_error("delete package version", user_status, b""))
            }
        } else if status == StatusCode::NOT_FOUND {
            // 404 视为已删除幂等成功
            Ok(())
        } else {
            Err(self.handle_status_error("delete package version", status, b""))
        }
    }

    /// 根据 Tag 精确删除包版本 (若 Tag 不存在返回 Ok(()))
    pub async fn delete_by_tag(&self, tag: &str) -> Result<(), OciError> {
        match self.find_version_id_by_tag(tag).await? {
            Some(version_id) => self.delete_package_version(version_id).await,
            None => {
                debug!(
                    "GHCR Tag '{}' not found in package {}, skipping deletion",
                    tag, self.package_name
                );
                Ok(())
            }
        }
    }

    /// 删除整个 Package (用于 purge --all)
    pub async fn delete_entire_package(&self) -> Result<(), OciError> {
        let cached_type = self.owner_type.load(Ordering::Relaxed);
        let is_org = cached_type != 2;

        let url = self.format_url(is_org, "");
        let headers = self.get_headers();
        let status = self.transport.delete(&url, headers).await?;

        if status == StatusCode::NO_CONTENT
            || status == StatusCode::OK
            || status == StatusCode::ACCEPTED
        {
            if cached_type == 0 {
                self.owner_type
                    .store(if is_org { 1 } else { 2 }, Ordering::Relaxed);
            }
            info!(
                "Successfully deleted entire GHCR package {}",
                self.package_name
            );
            Ok(())
        } else if status == StatusCode::NOT_FOUND && cached_type == 0 && is_org {
            let user_url = self.format_url(false, "");
            let user_headers = self.get_headers();
            let user_status = self.transport.delete(&user_url, user_headers).await?;
            if user_status == StatusCode::NO_CONTENT
                || user_status == StatusCode::OK
                || user_status == StatusCode::ACCEPTED
                || user_status == StatusCode::NOT_FOUND
            {
                self.owner_type.store(2, Ordering::Relaxed);
                Ok(())
            } else {
                Err(self.handle_status_error("delete package", user_status, b""))
            }
        } else if status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(self.handle_status_error("delete package", status, b""))
        }
    }
}
