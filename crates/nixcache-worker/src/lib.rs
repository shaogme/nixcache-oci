use crate::{
    oci::OciClient,
    store::CacheStore,
};
use worker::{
    Env, Fetch, Headers, Request, Response, Result, Router,
    event,
};

mod oci;
mod store;

fn get_upstream_list(env: &Env) -> Vec<String> {
    let upstream_str = env
        .var("NIXCACHE_UPSTREAM")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "https://cache.nixos.org".to_string());

    upstream_str
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn get_store(env: &Env) -> Result<CacheStore> {
    let registry = env
        .var("NIXCACHE_REGISTRY")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "ghcr.io".to_string());
    let repo = env
        .var("NIXCACHE_REPO")
        .map(|v| v.to_string())
        .map_err(|_| worker::Error::from("NIXCACHE_REPO environment variable must be set"))?;
    if repo.is_empty() || repo == "YOUR_GITHUB_USERNAME_OR_ORG/YOUR_REPO_NAME" {
        return Err(worker::Error::from("NIXCACHE_REPO must be configured with your actual GitHub repository (currently using default placeholder)"));
    }
    let github_token = env
        .secret("GITHUB_TOKEN")
        .map(|v| v.to_string())
        .or_else(|_| env.var("GITHUB_TOKEN").map(|v| v.to_string()))
        .unwrap_or_default();
    let ttl_seconds = env
        .var("NIXCACHE_INDEX_TTL")
        .map(|v| v.to_string().parse::<u64>().unwrap_or(300))
        .unwrap_or(300);

    let oci_client = OciClient::new(&registry, &repo, &github_token);
    Ok(CacheStore::new(oci_client, ttl_seconds))
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
            match store.get_data(&ctx.env).await {
                Ok(index_data) => {
                    if index_data.public_key.is_empty() {
                        Response::error("No public key configured", 404)
                    } else {
                        let headers = Headers::new();
                        headers.set("Content-Type", "text/plain")?;
                        Ok(Response::ok(format!("{}\n", index_data.public_key))?.with_headers(headers))
                    }
                }
                Err(e) => Response::error(format!("Failed to load index: {}", e), 500),
            }
        })
        .get_async("/_status", |_req, ctx| async move {
            let store = get_store(&ctx.env)?;
            match store.get_data(&ctx.env).await {
                Ok(index_data) => {
                    let repo = ctx
                        .env
                        .var("NIXCACHE_REPO")
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    let upstream = get_upstream_list(&ctx.env);

                    let status = serde_json::json!({
                        "index_entries": index_data.entries.len(),
                        "index_generated": index_data.generated,
                        "repo": repo,
                        "upstream": upstream,
                    });

                    Response::from_json(&status)
                }
                Err(e) => Response::error(format!("Failed to load index: {}", e), 500),
            }
        })
        .post_async("/_refresh", |_req, ctx| async move {
            let store = get_store(&ctx.env)?;
            match store.force_refresh(&ctx.env).await {
                Ok(data) => {
                    let res = serde_json::json!({
                        "refreshed": true,
                        "entries": data.entries.len(),
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
            let index_data = match store.get_data(&ctx.env).await {
                Ok(data) => data,
                Err(e) => return Response::error(format!("Failed to load index: {}", e), 500),
            };

            // 1. Try our GHCR cache — stream directly
            let mut nar_digest = None;
            for entry in index_data.entries.values() {
                for line in entry.narinfo.lines() {
                    if line.starts_with("URL: ") && line.contains(nar_name) {
                        nar_digest = Some(entry.nar_digest.clone());
                        break;
                    }
                }
                if nar_digest.is_some() {
                    break;
                }
            }

            if let Some(digest) = nar_digest {
                let registry = ctx
                    .env
                    .var("NIXCACHE_REGISTRY")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|_| "ghcr.io".to_string());
                let repo = ctx
                    .env
                    .var("NIXCACHE_REPO")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|_| "shaogme/nixcache-oci".to_string());
                let github_token = ctx
                    .env
                    .secret("GITHUB_TOKEN")
                    .map(|v| v.to_string())
                    .or_else(|_| ctx.env.var("GITHUB_TOKEN").map(|v| v.to_string()))
                    .unwrap_or_default();

                let oci_client = OciClient::new(&registry, &repo, &github_token);
                if let Ok(resp) = oci_client.fetch_blob_response(&digest).await {
                    if resp.status_code() == 200 {
                        let headers = Headers::new();
                        headers.set("Content-Type", content_type_str)?;
                        if let Ok(Some(len)) = resp.headers().get("Content-Length") {
                            headers.set("Content-Length", &len)?;
                        }
                        return Ok(resp.with_headers(headers));
                    }
                }
            }

            // 2. Fallback to upstream
            let upstreams = get_upstream_list(&ctx.env);
            for cache_url in upstreams {
                let upstream_url = format!("{}/nar/{}", cache_url, nar_name);
                if let Ok(resp) = Fetch::Url(upstream_url.parse().map_err(|e| worker::Error::from(format!("{:?}", e)))?).send().await {
                    if resp.status_code() == 200 {
                        let headers = Headers::new();
                        headers.set("Content-Type", content_type_str)?;
                        if let Ok(Some(len)) = resp.headers().get("Content-Length") {
                            headers.set("Content-Length", &len)?;
                        }
                        return Ok(resp.with_headers(headers));
                    }
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
            let index_data = match store.get_data(&ctx.env).await {
                Ok(data) => data,
                Err(e) => return Response::error(format!("Failed to load index: {}", e), 500),
            };

            // 1. Look up in OCI index
            if let Some(entry) = index_data.entries.get(store_hash) {
                let headers = Headers::new();
                headers.set("Content-Type", "text/x-nix-narinfo")?;
                return Ok(Response::ok(&entry.narinfo)?.with_headers(headers));
            }

            // 2. Fallback to upstream
            let upstreams = get_upstream_list(&ctx.env);
            for cache_url in upstreams {
                let upstream_url = format!("{}/{}.narinfo", cache_url, store_hash);
                if let Ok(mut resp) = Fetch::Url(upstream_url.parse().map_err(|e| worker::Error::from(format!("{:?}", e)))?).send().await {
                    if resp.status_code() == 200 {
                        if let Ok(body) = resp.text().await {
                            let headers = Headers::new();
                            headers.set("Content-Type", "text/x-nix-narinfo")?;
                            return Ok(Response::ok(body)?.with_headers(headers));
                        }
                    }
                }
            }

            Response::error("narinfo not found", 404)
        })
        .run(req, env)
        .await
}
