#![cfg(target_arch = "wasm32")]

mod state;
mod store;
mod transport;

use crate::{
    store::{CacheStore, WorkerOciClient, WorkerProxyConfig},
    transport::WorkerFetchTransport,
};
use nixcache_core::{IndexEntry, StoreHash};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use worker::{Env, Fetch, Headers, Request, Response, Result, Router, event};

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum RegisterPayload {
    Map(HashMap<StoreHash, IndexEntry>),
    List(Vec<IndexEntry>),
    Object {
        entries: HashMap<StoreHash, IndexEntry>,
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

            let map: HashMap<StoreHash, IndexEntry> = match payload {
                RegisterPayload::Map(m) => m,
                RegisterPayload::List(list) => {
                    let mut m = HashMap::new();
                    for entry in list {
                        if let Some(sh) = entry.store_hash() {
                            m.insert(sh, entry);
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
                    if let Ok(bytes) = store.oci_client().get_blob(digest.as_str()).await {
                        let headers = Headers::new();
                        headers.set("Content-Type", content_type_str)?;

                        headers.set("Content-Length", &bytes.len().to_string())?;
                        return Ok(Response::from_bytes(bytes.to_vec())?.with_headers(headers));
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
    use nixcache_core::{IndexEntry, NarDigest, NarInfoMeta, StoreHash};
    use std::collections::HashMap;

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
        let hash1_str = "00000000000000000000000000000001";
        let hash2_str = "00000000000000000000000000000002";
        let hash3_str = "00000000000000000000000000000003";

        let entry1 = IndexEntry {
            name: "pkg1".to_string(),
            system: None,
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg1", hash1_str),
                nar_basename: "pkg1.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 100,
            added: "2026-08-29T10:00:00Z".to_string(),
            origin_job: None,
        };

        let mut map = HashMap::new();
        let sh1 = StoreHash::parse(hash1_str).unwrap();
        map.insert(sh1.clone(), entry1.clone());
        let map_json = serde_json::to_string(&map).unwrap();

        let payload: RegisterPayload = serde_json::from_str(&map_json).unwrap();
        match payload {
            RegisterPayload::Map(m) => {
                assert_eq!(m.len(), 1);
                assert_eq!(m.get(&sh1).unwrap().name, "pkg1");
            }
            _ => panic!("Expected Map payload"),
        }

        let entry2 = IndexEntry {
            name: "pkg2".to_string(),
            system: None,
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg2", hash2_str),
                nar_basename: "pkg2.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 200,
            added: "2026-08-29T10:00:00Z".to_string(),
            origin_job: None,
        };
        let list_json = serde_json::to_string(&vec![entry2]).unwrap();
        let payload_list: RegisterPayload = serde_json::from_str(&list_json).unwrap();
        match payload_list {
            RegisterPayload::List(l) => {
                assert_eq!(l.len(), 1);
                assert_eq!(l[0].name, "pkg2");
            }
            _ => panic!("Expected List payload"),
        }

        let entry3 = IndexEntry {
            name: "pkg3".to_string(),
            system: None,
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg3", hash3_str),
                nar_basename: "pkg3.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 300,
            added: "2026-08-29T10:00:00Z".to_string(),
            origin_job: None,
        };
        let mut obj_map = HashMap::new();
        let sh3 = StoreHash::parse(hash3_str).unwrap();
        obj_map.insert(sh3.clone(), entry3);
        let obj = RegisterPayload::Object { entries: obj_map };
        let obj_json = serde_json::to_string(&obj).unwrap();
        let payload_obj: RegisterPayload = serde_json::from_str(&obj_json).unwrap();
        match payload_obj {
            RegisterPayload::Object { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries.get(&sh3).unwrap().name, "pkg3");
            }
            _ => panic!("Expected Object payload"),
        }
    }
}
