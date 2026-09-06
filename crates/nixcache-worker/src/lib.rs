pub mod error;
mod state;
mod store;
mod transport;

use crate::{
    store::{CacheStore, WorkerOciClient, WorkerProxyConfig},
    transport::WorkerFetchTransport,
};
pub use error::WorkerStoreError;
use futures_util::TryStreamExt;
use nixcache_core::SystemArch;
use worker::{Env, Fetch, Headers, Request, Response, Result, Router, event};

pub fn parse_upstream_list(upstream_str: &str) -> Vec<String> {
    upstream_str
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn get_worker_config(env: &Env) -> Result<WorkerProxyConfig> {
    let registry = env
        .var("NIXCACHE_REGISTRY")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "ghcr.io".to_string());
    let repo = env
        .var("NIXCACHE_REPO")
        .map(|v| v.to_string())
        .map_err(|_| worker::Error::from("NIXCACHE_REPO environment variable must be set"))?;
    if repo.is_empty() || repo == "YOUR_GITHUB_USERNAME_OR_ORG/YOUR_REPO_NAME" {
        return Err(worker::Error::from(
            "NIXCACHE_REPO must be configured with your actual GitHub repository (currently using default placeholder)",
        ));
    }

    let baseline_tag = env
        .var("NIXCACHE_BASELINE_TAG")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "cache-index".to_string());

    let upstream_str = env
        .var("NIXCACHE_UPSTREAM_CACHES")
        .or_else(|_| env.var("NIXCACHE_UPSTREAM"))
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "https://cache.nixos.org".to_string());
    let upstream_caches = parse_upstream_list(&upstream_str);

    let baseline_ttl_secs = env
        .var("NIXCACHE_INDEX_TTL")
        .or_else(|_| env.var("NIXCACHE_BASELINE_TTL"))
        .map(|v| v.to_string().parse::<u64>().unwrap_or(300))
        .unwrap_or(300);

    let target_system = env
        .var("NIXCACHE_SYSTEM")
        .map(|v| SystemArch::from(v.to_string().as_str()))
        .unwrap_or(SystemArch::X86_64Linux);

    Ok(WorkerProxyConfig {
        registry,
        repo,
        baseline_tag,
        upstream_caches,
        baseline_ttl_secs,
        target_system,
    })
}

fn get_store(env: &Env) -> Result<CacheStore> {
    let config = get_worker_config(env)?;
    let github_token = env
        .secret("GITHUB_TOKEN")
        .or_else(|_| env.secret("NIXCACHE_AUTH_TOKEN"))
        .or_else(|_| env.var("GITHUB_TOKEN"))
        .or_else(|_| env.var("NIXCACHE_AUTH_TOKEN"))
        .map(|v| v.to_string())
        .unwrap_or_default();

    let oci_client = WorkerOciClient::with_transport(
        &config.registry,
        &config.repo,
        &github_token,
        false,
        WorkerFetchTransport,
    );
    Ok(CacheStore::new(oci_client, config))
}

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    let router = Router::new();

    router
        .get("/nix-cache-info", |_req, _ctx| {
            let body = "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n";
            let headers = Headers::new();
            headers.set("Content-Type", "text/x-nix-cache-info")?;
            Ok(Response::ok(body)?.with_headers(headers))
        })
        .get_async("/public-key", |_req, ctx| async move {
            let store = get_store(&ctx.env)?;
            match store.get_public_key(&ctx.env).await {
                Ok(Some(public_key)) => {
                    let headers = Headers::new();
                    headers.set("Content-Type", "text/x-nix-public-key")?;
                    Ok(Response::ok(format!("{}\n", public_key))?.with_headers(headers))
                }
                Ok(None) => Response::error("No public key configured", 404),
                Err(e) => Response::error(format!("Failed to load public key: {}", e), 500),
            }
        })
        .get_async("/_status", |_req, ctx| async move {
            let store = match get_store(&ctx.env) {
                Ok(s) => s,
                Err(e) => {
                    let status = serde_json::json!({
                        "remote_connected": false,
                        "remote_error": e.to_string(),
                        "registry": "ghcr.io",
                        "repo": "",
                        "tier0_hot_entries": 0,
                        "baseline_entries": 0,
                        "total_unique_entries": 0,
                        "index_entries": 0,
                        "index_ttl": 300,
                        "baseline_ttl": 300,
                        "upstream": ["https://cache.nixos.org"],
                        "manifest_digest": "",
                        "generated": ""
                    });
                    return Response::from_json(&status);
                }
            };

            let status_data = store.get_status(&ctx.env).await;
            Response::from_json(&status_data)
        })
        .post_async("/_refresh", |_req, ctx| async move {
            let store = get_store(&ctx.env)?;
            match store.force_refresh(&ctx.env).await {
                Ok(refresh_res) => {
                    let status = store.get_status(&ctx.env).await;
                    let res = serde_json::json!({
                        "refreshed": true,
                        "entries": refresh_res.total_entries,
                        "warmed_shards": refresh_res.warmed_shards,
                        "manifest_digest": status.manifest_digest,
                    });
                    Response::from_json(&res)
                }
                Err(e) => {
                    let res = serde_json::json!({
                        "refreshed": false,
                        "error": e.to_string(),
                    });
                    Response::from_json(&res)
                }
            }
        })
        .get_async("/nar/:nar_name", |_req, ctx| async move {
            let nar_name = match ctx.param("nar_name") {
                Some(name) => name,
                None => return Response::error("Missing NAR name", 400),
            };

            let content_type_str = if nar_name.ends_with(".xz") {
                "application/x-xz"
            } else {
                "application/x-nix-nar"
            };

            let store = get_store(&ctx.env)?;

            // 1. 级联反向解析 NAR Digest (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3)
            if let Ok(Some(digest)) = store.lookup_nar_digest(&ctx.env, nar_name).await {
                match store.oci_client().stream_blob(digest.as_str()).await {
                    Ok(blob_stream) if blob_stream.status.is_success() => {
                        store.set_remote_status(true, None);
                        let headers = Headers::new();
                        headers.set("Content-Type", content_type_str)?;
                        headers.set("Accept-Ranges", "bytes")?;
                        if let Some(len) = blob_stream.content_length() {
                            headers.set("Content-Length", &len.to_string())?;
                        }
                        let stream = blob_stream
                            .stream
                            .map_err(|e| worker::Error::RustError(e.to_string()));
                        return Ok(Response::from_stream(stream)?.with_headers(headers));
                    }
                    Ok(blob_stream) => {
                        store.set_remote_status(
                            false,
                            Some(format!("GHCR HTTP {}", blob_stream.status)),
                        );
                    }
                    Err(e) => {
                        store.set_remote_status(false, Some(format!("Stream failed: {}", e)));
                    }
                }
            }

            // 2. Fallback to upstream (保持原始 Headers，设置 Content-Type 与 Accept-Ranges)
            for cache_url in &store.config().upstream_caches {
                let upstream_url = format!("{}/nar/{}", cache_url, nar_name);
                if let Ok(resp) = Fetch::Url(
                    upstream_url
                        .parse()
                        .map_err(|e| worker::Error::from(format!("{:?}", e)))?,
                )
                .send()
                .await
                    && resp.status_code() == 200
                {
                    let headers = resp.headers().clone();
                    headers.set("Content-Type", content_type_str)?;
                    headers.set("Accept-Ranges", "bytes")?;
                    return Ok(resp.with_headers(headers));
                }
            }

            Response::error("NAR not found", 404)
        })
        .get_async("/:hash_ext", |_req, ctx| async move {
            let hash_ext = match ctx.param("hash_ext") {
                Some(h) => h,
                None => return Response::error("Missing parameter", 400),
            };

            if !hash_ext.ends_with(".narinfo") {
                return Response::error("Not found", 404);
            }
            let store_hash = hash_ext.trim_end_matches(".narinfo");

            let store = get_store(&ctx.env)?;

            // 1. 级联解析与自愈穿透 (Tier 1 -> Tier 2 -> Tier 3 -> Read-Through SWR)
            match store.lookup_narinfo(&ctx.env, store_hash).await {
                Ok(Some(res)) => {
                    let headers = Headers::new();
                    headers.set("Content-Type", "text/x-nix-narinfo")?;
                    headers.set("X-NixCache-Version", "6")?;
                    if res.self_healed {
                        headers.set("X-NixCache-Self-Healed", "1")?;
                    } else {
                        headers.set("X-NixCache-Self-Healed", "0")?;
                    }
                    if let Some(shard_id) = res.shard_id {
                        headers.set("X-NixCache-Shard", &shard_id.to_string())?;
                    }
                    if let Some(ref digest) = res.manifest_digest {
                        headers.set("X-NixCache-Digest", digest)?;
                    }
                    return Ok(Response::ok(&res.narinfo_content)?.with_headers(headers));
                }
                Ok(None) => {}
                Err(e) => return Response::error(format!("Failed to query narinfo: {}", e), 500),
            }

            // 2. Fallback to upstream
            for cache_url in &store.config().upstream_caches {
                let upstream_url = format!("{}/{}.narinfo", cache_url, store_hash);
                if let Ok(mut resp) = Fetch::Url(
                    upstream_url
                        .parse()
                        .map_err(|e| worker::Error::from(format!("{:?}", e)))?,
                )
                .send()
                .await
                    && resp.status_code() == 200
                    && let Ok(body) = resp.text().await
                {
                    let headers = resp.headers().clone();
                    headers.set("Content-Type", "text/x-nix-narinfo")?;
                    headers.set("X-NixCache-Version", "6")?;
                    headers.set("X-NixCache-Self-Healed", "0")?;
                    return Ok(Response::ok(body)?.with_headers(headers));
                }
            }

            let headers = Headers::new();
            headers.set("X-NixCache-Version", "6")?;
            headers.set("X-NixCache-Self-Healed", "0")?;
            Ok(Response::error("narinfo not found", 404)?.with_headers(headers))
        })
        .run(req, env)
        .await
}

#[cfg(test)]
mod tests {
    use super::parse_upstream_list;

    #[test]
    fn test_worker_upstream_parsing() {
        let single = "https://cache.nixos.org";
        assert_eq!(
            parse_upstream_list(single),
            vec!["https://cache.nixos.org".to_string()]
        );

        let comma_separated = "https://cache.nixos.org, https://nix-community.cachix.org";
        assert_eq!(
            parse_upstream_list(comma_separated),
            vec![
                "https://cache.nixos.org".to_string(),
                "https://nix-community.cachix.org".to_string()
            ]
        );

        let mixed_whitespace = "  https://cache.nixos.org \n  https://nix-community.cachix.org ,  ";
        assert_eq!(
            parse_upstream_list(mixed_whitespace),
            vec![
                "https://cache.nixos.org".to_string(),
                "https://nix-community.cachix.org".to_string()
            ]
        );

        let empty = "   \n\t  ";
        assert!(parse_upstream_list(empty).is_empty());
    }
}
