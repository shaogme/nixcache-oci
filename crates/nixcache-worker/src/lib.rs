mod oci;
mod store;

use crate::{
    oci::OciClient,
    store::{CacheStore, IndexEntry, WorkerProxyConfig},
};
use serde::Deserialize;
use std::collections::HashMap;
use worker::{Env, Fetch, Headers, Request, Response, Result, Router, event};

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum RegisterPayload {
    Map(HashMap<String, IndexEntry>),
    List(Vec<IndexEntry>),
    Object {
        entries: HashMap<String, IndexEntry>,
    },
}

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

    let run_id = env
        .var("NIXCACHE_RUN_ID")
        .ok()
        .and_then(|v| v.to_string().parse::<u64>().ok());

    let branch_or_pr = env
        .var("NIXCACHE_BRANCH")
        .or_else(|_| env.var("NIXCACHE_PR"))
        .map(|v| v.to_string())
        .ok()
        .filter(|s| !s.is_empty());

    let baseline_tag = env
        .var("NIXCACHE_BASELINE_TAG")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "cache-index".to_string());

    let upstream_str = env
        .var("NIXCACHE_UPSTREAM")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "https://cache.nixos.org".to_string());
    let upstream_caches = parse_upstream_list(&upstream_str);

    let baseline_ttl_secs = env
        .var("NIXCACHE_INDEX_TTL")
        .or_else(|_| env.var("NIXCACHE_BASELINE_TTL"))
        .map(|v| v.to_string().parse::<u64>().unwrap_or(300))
        .unwrap_or(300);

    let session_ttl_secs = env
        .var("NIXCACHE_SESSION_TTL")
        .map(|v| v.to_string().parse::<u64>().unwrap_or(10))
        .unwrap_or(10);

    Ok(WorkerProxyConfig {
        registry,
        repo,
        run_id,
        branch_or_pr,
        baseline_tag,
        upstream_caches,
        session_ttl_secs,
        baseline_ttl_secs,
    })
}

fn get_store(env: &Env) -> Result<CacheStore> {
    let config = get_worker_config(env)?;
    let github_token = env
        .secret("GITHUB_TOKEN")
        .map(|v| v.to_string())
        .or_else(|_| env.var("GITHUB_TOKEN").map(|v| v.to_string()))
        .unwrap_or_default();

    let oci_client = OciClient::new(&config.registry, &config.repo, &github_token);
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
                        "tier1_session_entries": 0,
                        "tier2_branch_entries": 0,
                        "tier3_baseline_entries": 0,
                        "total_unique_entries": 0,
                        "index_entries": 0,
                        "index_ttl": 300,
                        "session_ttl": 10,
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
                Ok(count) => {
                    let res = serde_json::json!({
                        "refreshed": true,
                        "entries": count,
                    });
                    Response::from_json(&res)
                }
                Err(e) => {
                    let res = serde_json::json!({
                        "refreshed": false,
                        "error": e,
                    });
                    Response::from_json(&res)
                }
            }
        })
        .post_async("/_session/register", |mut req, _ctx| async move {
            let payload = match req.json::<RegisterPayload>().await {
                Ok(p) => p,
                Err(e) => return Response::error(format!("Invalid register payload: {}", e), 400),
            };

            let map: HashMap<String, IndexEntry> = match payload {
                RegisterPayload::Map(m) => m,
                RegisterPayload::List(list) => {
                    let mut m = HashMap::new();
                    for entry in list {
                        for line in entry.narinfo.lines() {
                            if let Some(rest) = line.strip_prefix("StorePath: /nix/store/")
                                && rest.len() >= 32
                            {
                                m.insert(rest[..32].to_string(), entry.clone());
                                break;
                            }
                        }
                    }
                    m
                }
                RegisterPayload::Object { entries } => entries,
            };

            let count = map.len();
            CacheStore::register_hot_entries(map);

            let res = serde_json::json!({
                "status": "ok",
                "registered": count,
            });
            Response::from_json(&res)
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

            // 1. 级联解析 (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3)
            match store.lookup_nar_digest_cascading(&ctx.env, nar_name).await {
                Ok(Some(digest)) => {
                    if let Ok(resp) = store.oci_client().fetch_blob_response(&digest).await
                        && resp.status_code() == 200
                    {
                        let headers = Headers::new();
                        headers.set("Content-Type", content_type_str)?;
                        if let Ok(Some(len)) = resp.headers().get("Content-Length") {
                            headers.set("Content-Length", &len)?;
                        }
                        return Ok(resp.with_headers(headers));
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    return Response::error(format!("Failed to query NAR digest: {}", e), 500);
                }
            }

            // 2. Fallback to upstream
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
                    let headers = Headers::new();
                    headers.set("Content-Type", content_type_str)?;
                    if let Ok(Some(len)) = resp.headers().get("Content-Length") {
                        headers.set("Content-Length", &len)?;
                    }
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

            // 1. 级联解析 (Tier 0 -> Tier 1 -> Tier 2 -> Tier 3)
            match store.lookup_narinfo_cascading(&ctx.env, store_hash).await {
                Ok(Some(narinfo)) => {
                    let headers = Headers::new();
                    headers.set("Content-Type", "text/x-nix-narinfo")?;
                    return Ok(Response::ok(&narinfo)?.with_headers(headers));
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
                    let headers = Headers::new();
                    headers.set("Content-Type", "text/x-nix-narinfo")?;
                    return Ok(Response::ok(body)?.with_headers(headers));
                }
            }

            Response::error("narinfo not found", 404)
        })
        .run(req, env)
        .await
}

#[cfg(test)]
mod tests {
    use super::{RegisterPayload, parse_upstream_list};

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

    #[test]
    fn test_register_payload_deserialization() {
        let map_json = r#"{
            "hash1": {
                "name": "pkg1",
                "narinfo": "StorePath: /nix/store/hash1-pkg1\nURL: nar/pkg1.nar.xz\n",
                "nar_digest": "sha256:digest1",
                "nar_size": 100,
                "added": "2026-08-29T10:00:00Z"
            }
        }"#;
        let payload: RegisterPayload = serde_json::from_str(map_json).unwrap();
        match payload {
            RegisterPayload::Map(m) => {
                assert_eq!(m.len(), 1);
                assert_eq!(m.get("hash1").unwrap().name, "pkg1");
            }
            _ => panic!("Expected Map payload"),
        }

        let list_json = r#"[
            {
                "name": "pkg2",
                "narinfo": "StorePath: /nix/store/hash22222222222222222222222222222222-pkg2\nURL: nar/pkg2.nar.xz\n",
                "nar_digest": "sha256:digest2",
                "nar_size": 200,
                "added": "2026-08-29T10:00:00Z"
            }
        ]"#;
        let payload_list: RegisterPayload = serde_json::from_str(list_json).unwrap();
        match payload_list {
            RegisterPayload::List(l) => {
                assert_eq!(l.len(), 1);
                assert_eq!(l[0].name, "pkg2");
            }
            _ => panic!("Expected List payload"),
        }

        let obj_json = r#"{
            "entries": {
                "hash3": {
                    "name": "pkg3",
                    "narinfo": "StorePath: /nix/store/hash3-pkg3\nURL: nar/pkg3.nar.xz\n",
                    "nar_digest": "sha256:digest3",
                    "nar_size": 300,
                    "added": "2026-08-29T10:00:00Z"
                }
            }
        }"#;
        let payload_obj: RegisterPayload = serde_json::from_str(obj_json).unwrap();
        match payload_obj {
            RegisterPayload::Object { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries.get("hash3").unwrap().name, "pkg3");
            }
            _ => panic!("Expected Object payload"),
        }
    }
}
