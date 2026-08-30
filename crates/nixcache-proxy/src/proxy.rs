use crate::index::CacheIndex;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use nixcache_core::{IndexEntry, StoreHash};
use nixcache_oci::OciClient;
use nixcache_oci_backend::ReqwestTransport;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use tracing::error;

#[derive(Clone)]
pub struct AppState {
    pub repo: String,
    pub index: CacheIndex,
    pub oci_client: OciClient<ReqwestTransport>,
    pub http_client: reqwest::Client,
}

#[derive(Serialize)]
struct StatusResponse {
    remote_connected: bool,
    remote_error: Option<String>,
    registry: String,
    repo: String,
    run_id: Option<u64>,
    branch_or_pr: Option<String>,
    tier0_hot_entries: usize,
    tier1_session_entries: usize,
    tier2_branch_entries: usize,
    tier3_baseline_entries: usize,
    total_unique_entries: usize,
    index_entries: usize,
    index_ttl: u64,
    session_ttl: u64,
    baseline_ttl: u64,
    upstream: Vec<String>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum RegisterPayload {
    Map(HashMap<StoreHash, IndexEntry>),
    List(Vec<IndexEntry>),
    Object {
        entries: HashMap<StoreHash, IndexEntry>,
    },
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/nix-cache-info", get(serve_cache_info))
        .route("/public-key", get(serve_public_key))
        .route("/_status", get(serve_status))
        .route("/_refresh", post(handle_refresh))
        .route("/_session/register", post(handle_register_session))
        .route("/{hash_ext}", get(serve_narinfo))
        .route("/nar/{nar_name}", get(serve_nar))
        .with_state(state)
}

async fn serve_cache_info() -> impl IntoResponse {
    let body = "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n";
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/x-nix-cache-info"),
    );
    (StatusCode::OK, headers, body)
}

async fn serve_public_key(State(state): State<AppState>) -> impl IntoResponse {
    if let Some(pubkey) = state.index.get_public_key().await {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/x-nix-public-key"),
        );
        let body = format!("{}\n", pubkey);
        (StatusCode::OK, headers, body).into_response()
    } else {
        (StatusCode::NOT_FOUND, "No public key configured\n").into_response()
    }
}

async fn serve_status(State(state): State<AppState>) -> impl IntoResponse {
    let (remote_connected, remote_error) = state.index.remote_status();
    let counts = state.index.get_entry_counts().await;
    let config = state.index.config();

    let status = StatusResponse {
        remote_connected,
        remote_error,
        registry: state.index.registry().to_string(),
        repo: state.repo.clone(),
        run_id: config.run_id,
        branch_or_pr: config.branch_or_pr.clone(),
        tier0_hot_entries: counts.tier0_hot_entries,
        tier1_session_entries: counts.tier1_session_entries,
        tier2_branch_entries: counts.tier2_branch_entries,
        tier3_baseline_entries: counts.tier3_baseline_entries,
        total_unique_entries: counts.total_unique_entries,
        index_entries: counts.total_unique_entries,
        index_ttl: config.baseline_ttl.as_secs(),
        session_ttl: config.session_ttl.as_secs(),
        baseline_ttl: config.baseline_ttl.as_secs(),
        upstream: state.index.upstream_caches().to_vec(),
    };
    (StatusCode::OK, axum::Json(status))
}

async fn handle_refresh(State(state): State<AppState>) -> impl IntoResponse {
    match state.index.force_refresh().await {
        Ok(count) => {
            let res = json!({
                "refreshed": true,
                "entries": count
            });
            (StatusCode::OK, axum::Json(res))
        }
        Err(e) => {
            let res = json!({
                "refreshed": false,
                "error": e
            });
            (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(res))
        }
    }
}

async fn handle_register_session(
    State(state): State<AppState>,
    Json(payload): Json<RegisterPayload>,
) -> impl IntoResponse {
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
    state.index.register_hot_entries(map).await;

    let res = json!({
        "status": "ok",
        "registered": count
    });
    (StatusCode::OK, axum::Json(res))
}

async fn serve_narinfo(State(state): State<AppState>, Path(hash_ext): Path<String>) -> Response {
    if !hash_ext.ends_with(".narinfo") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let store_hash = hash_ext.trim_end_matches(".narinfo");

    // 1. Look up in OCI index
    if let Some(entry) = state.index.lookup(store_hash).await {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/x-nix-narinfo"));
        return (StatusCode::OK, headers, entry.to_narinfo_string()).into_response();
    }

    // 2. Fallback to upstream
    for cache_url in state.index.upstream_caches() {
        let upstream_url = format!("{}/{}.narinfo", cache_url, store_hash);
        match state.http_client.get(&upstream_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.text().await {
                    let mut headers = HeaderMap::new();
                    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/x-nix-narinfo"));
                    return (StatusCode::OK, headers, body).into_response();
                }
            }
            _ => {}
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

async fn serve_nar(State(state): State<AppState>, Path(nar_name): Path<String>) -> Response {
    let content_type_str = if nar_name.ends_with(".xz") {
        "application/x-xz"
    } else {
        "application/x-nix-nar"
    };

    // 1. Try our GHCR cache — stream directly
    if let Some(digest) = state.index.find_nar_digest(&nar_name).await {
        match state.oci_client.stream_blob(digest.as_str()).await {
            Ok(resp) if resp.status.is_success() => {
                let content_len = resp.content_length();
                let mut headers = HeaderMap::new();
                headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
                if let Ok(val) = HeaderValue::from_str(content_type_str) {
                    headers.insert(CONTENT_TYPE, val);
                }
                if let Some(len) = content_len {
                    headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
                }

                let body = Body::from_stream(resp.stream);
                return (StatusCode::OK, headers, body).into_response();
            }
            Err(e) => {
                error!(
                    "[nixcache-proxy] Failed to stream blob {} from GHCR: {}",
                    digest, e
                );
            }
            _ => {}
        }
    }

    // 2. Fallback to upstream — stream directly
    for cache_url in state.index.upstream_caches() {
        let upstream_url = format!("{}/nar/{}", cache_url, nar_name);
        match state.http_client.get(&upstream_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let content_len = resp.content_length();
                let mut headers = HeaderMap::new();
                headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                if let Ok(val) = HeaderValue::from_str(content_type_str) {
                    headers.insert(CONTENT_TYPE, val);
                }
                if let Some(len) = content_len {
                    headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
                }

                let stream = resp.bytes_stream();
                let body = Body::from_stream(stream);
                return (StatusCode::OK, headers, body).into_response();
            }
            _ => {}
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{CacheIndex, CascadingProxyConfig};
    use axum::http::Request;
    use http_body_util::BodyExt;
    use nixcache_core::{
        IndexEntry, NarDigest, NarInfoMeta, ShardDataPayload, ShardedArchCacheIndexData, StoreHash,
        SystemArch, calculate_shard_id,
    };
    use nixcache_oci_backend::create_tokio_reqwest_client;
    use std::{collections::HashMap, time::Duration};
    use tower::ServiceExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[tokio::test]
    async fn test_cache_info_endpoint() {
        let index = CacheIndex::with_config(
            CascadingProxyConfig {
                repo: "test/repo".to_string(),
                upstream_caches: vec!["https://cache.nixos.org".to_string()],
                ..Default::default()
            },
            "",
        );
        let mut root =
            ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        root.public_key = "test-key-1:abcd".to_string();
        index
            .update_sharded_baseline_in_memory(root, vec![], None)
            .await;

        let state = AppState {
            repo: "test/repo".to_string(),
            index,
            oci_client: create_tokio_reqwest_client("ghcr.io", "test/repo", "", true),
            http_client: reqwest::Client::new(),
        };

        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nix-cache-info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/x-nix-cache-info"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("StoreDir: /nix/store"));
        assert!(body_str.contains("WantMassQuery: 1"));
        assert!(body_str.contains("Priority: 40"));
    }

    #[tokio::test]
    async fn test_public_key_endpoint_present_and_missing() {
        let index = CacheIndex::with_config(
            CascadingProxyConfig {
                repo: "test/repo".to_string(),
                upstream_caches: vec![],
                ..Default::default()
            },
            "",
        );
        let mut root =
            ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        root.public_key = "test-key-1:abcd".to_string();
        index
            .update_sharded_baseline_in_memory(root, vec![], None)
            .await;

        let state = AppState {
            repo: "test/repo".to_string(),
            index: index.clone(),
            oci_client: create_tokio_reqwest_client("ghcr.io", "test/repo", "", true),
            http_client: reqwest::Client::new(),
        };

        let app = create_router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/public-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8_lossy(&body);
        assert_eq!(body_str.trim(), "test-key-1:abcd");

        // Missing key returns 404
        index
            .update_sharded_baseline_in_memory(
                ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io"),
                vec![],
                None,
            )
            .await;
        let empty_state = AppState {
            repo: "test/repo".to_string(),
            index,
            oci_client: create_tokio_reqwest_client("ghcr.io", "test/repo", "", true),
            http_client: reqwest::Client::new(),
        };
        let empty_app = create_router(empty_state);

        let missing_resp = empty_app
            .oneshot(
                Request::builder()
                    .uri("/public-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(missing_resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_status_endpoint() {
        let index = CacheIndex::with_config(
            CascadingProxyConfig {
                repo: "test/repo".to_string(),
                upstream_caches: vec!["https://cache.nixos.org".to_string()],
                baseline_ttl: Duration::from_secs(300),
                ..Default::default()
            },
            "",
        );
        let hash1 = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let shard_id = calculate_shard_id(&hash1);
        let mut shard = ShardDataPayload::new(shard_id);
        shard.entries.insert(
            hash1.clone(),
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-pkg1", hash1),
                    nar_basename: "pkg1.nar.xz".to_string(),
                    nar_hash:
                        "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                            .to_string(),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_sha256(
                    "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
                )
                .unwrap(),
                nar_size: 100,
                added: "2026-08-28T00:00:00Z".to_string(),
                origin_job: None,
            },
        );

        let root = ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        index
            .update_sharded_baseline_in_memory(root, vec![shard], None)
            .await;

        let state = AppState {
            repo: "test/repo".to_string(),
            index,
            oci_client: create_tokio_reqwest_client("ghcr.io", "test/repo", "", true),
            http_client: reqwest::Client::new(),
        };

        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/_status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let status_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status_json["remote_connected"], true);
        assert_eq!(status_json["registry"], "ghcr.io");
        assert_eq!(status_json["index_entries"], 1);
        assert_eq!(status_json["repo"], "test/repo");
        assert_eq!(status_json["index_ttl"], 300);
        assert_eq!(status_json["upstream"][0], "https://cache.nixos.org");
    }

    #[tokio::test]
    async fn test_narinfo_hit_local_and_miss_upstream_fallback() {
        let upstream_server = MockServer::start().await;
        let upstream_url = upstream_server.uri();

        let index = CacheIndex::with_config(
            CascadingProxyConfig {
                repo: "test/repo".to_string(),
                upstream_caches: vec![upstream_url],
                ..Default::default()
            },
            "",
        );

        let local_hash = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let entry = IndexEntry {
            name: "local-pkg".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", local_hash),
                nar_basename: "local.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 1024,
            added: "2026-08-28T00:00:00Z".to_string(),
            origin_job: None,
        };
        let local_rendered = entry.to_narinfo_string();
        let shard_id = calculate_shard_id(&local_hash);
        let mut shard = ShardDataPayload::new(shard_id);
        shard.entries.insert(local_hash.clone(), entry);

        let root = ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        index
            .update_sharded_baseline_in_memory(root, vec![shard], None)
            .await;

        let upstream_narinfo = "StorePath: /nix/store/11111111111111111111111111111111-pkg\nURL: nar/upstream.nar.xz\nNarHash: sha256:000\nNarSize: 10\n";
        Mock::given(method("GET"))
            .and(path("/11111111111111111111111111111111.narinfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/x-nix-narinfo")
                    .set_body_string(upstream_narinfo),
            )
            .mount(&upstream_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/22222222222222222222222222222222.narinfo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&upstream_server)
            .await;

        let state = AppState {
            repo: "test/repo".to_string(),
            index,
            oci_client: create_tokio_reqwest_client("ghcr.io", "test/repo", "", true),
            http_client: reqwest::Client::new(),
        };

        let app = create_router(state);

        // 1. Local hit
        let local_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/{}.narinfo", local_hash))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(local_resp.status(), StatusCode::OK);
        let local_body = local_resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(String::from_utf8_lossy(&local_body), local_rendered);

        // 2. Upstream fallback hit
        let upstream_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/11111111111111111111111111111111.narinfo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(upstream_resp.status(), StatusCode::OK);
        let upstream_body = upstream_resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(String::from_utf8_lossy(&upstream_body), upstream_narinfo);

        // 3. Complete miss -> 404
        let miss_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/22222222222222222222222222222222.narinfo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(miss_resp.status(), StatusCode::NOT_FOUND);

        // 4. Invalid extension -> 404
        let invalid_resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/{}.notnarinfo", local_hash))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_nar_streaming_and_fallback() {
        let oci_server = MockServer::start().await;
        let oci_host = oci_server.address().to_string();

        let upstream_server = MockServer::start().await;
        let upstream_url = upstream_server.uri();

        let index = CacheIndex::with_config(
            CascadingProxyConfig {
                registry: oci_host.clone(),
                repo: "test/repo".to_string(),
                upstream_caches: vec![upstream_url],
                ..Default::default()
            },
            "",
        );

        let local_hash = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap();
        let digest_str = "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0";
        let shard_id = calculate_shard_id(&local_hash);
        let mut shard = ShardDataPayload::new(shard_id);
        shard.entries.insert(
            local_hash.clone(),
            IndexEntry {
                name: "local-pkg".to_string(),
                system: Some(SystemArch::X86_64Linux),
                narinfo_meta: NarInfoMeta {
                    store_path: format!("/nix/store/{}-pkg", local_hash),
                    nar_basename: format!("{}-local.nar.xz", local_hash),
                    nar_hash: format!("sha256:{}", digest_str),
                    ..Default::default()
                },
                nar_digest: NarDigest::new_sha256(digest_str).unwrap(),
                nar_size: 5,
                added: "2026-08-28T00:00:00Z".to_string(),
                origin_job: None,
            },
        );

        let root = ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "test/repo", "ghcr.io");
        index
            .update_sharded_baseline_in_memory(root, vec![shard], None)
            .await;

        // Mock OCI Blob streaming
        Mock::given(method("GET"))
            .and(path(format!(
                "/v2/test/repo/nix-cache/blobs/sha256:{}",
                digest_str
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-length", "5")
                    .set_body_bytes(b"HELLO"),
            )
            .mount(&oci_server)
            .await;

        // Mock Upstream NAR streaming
        Mock::given(method("GET"))
            .and(path("/nar/upstream.nar.xz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-length", "5")
                    .set_body_bytes(b"WORLD"),
            )
            .mount(&upstream_server)
            .await;

        let state = AppState {
            repo: "test/repo".to_string(),
            index,
            oci_client: create_tokio_reqwest_client(&oci_host, "test/repo", "", false),
            http_client: reqwest::Client::new(),
        };

        let app = create_router(state);

        // 1. Local NAR stream
        let local_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/nar/{}-local.nar.xz", local_hash))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(local_resp.status(), StatusCode::OK);
        assert_eq!(
            local_resp.headers().get("content-type").unwrap(),
            "application/x-xz"
        );
        let body = local_resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, b"HELLO"[..]);

        // 2. Upstream NAR stream
        let upstream_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/nar/upstream.nar.xz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(upstream_resp.status(), StatusCode::OK);
        let body = upstream_resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(body, b"WORLD"[..]);

        let miss_resp = app
            .oneshot(
                Request::builder()
                    .uri("/nar/nonexistent.nar.xz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(miss_resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_register_session_endpoint() {
        let index = CacheIndex::with_config(
            CascadingProxyConfig {
                repo: "test/repo".to_string(),
                upstream_caches: vec![],
                ..Default::default()
            },
            "",
        );
        let state = AppState {
            repo: "test/repo".to_string(),
            index,
            oci_client: create_tokio_reqwest_client("ghcr.io", "test/repo", "", true),
            http_client: reqwest::Client::new(),
        };

        let app = create_router(state);

        let hash1_str = "s66mzxpvicwk07gjbjfw9izjfa797vsw";
        let hash1 = StoreHash::parse(hash1_str).unwrap();
        let entry = IndexEntry {
            name: "hot-pkg".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{}-pkg", hash1_str),
                nar_basename: "hot.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: NarDigest::new_sha256(
                "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
            )
            .unwrap(),
            nar_size: 42,
            added: "2026-08-29T10:00:00Z".to_string(),
            origin_job: None,
        };

        let mut payload_map = HashMap::new();
        payload_map.insert(hash1.clone(), entry);

        let reg_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_session/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&payload_map).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(reg_resp.status(), StatusCode::OK);

        // 验证注册后立即可以通过 /{hash}.narinfo 查询到
        let get_resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/{}.narinfo", hash1))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(get_resp.status(), StatusCode::OK);
        let body = get_resp.into_body().collect().await.unwrap().to_bytes();
        assert!(
            String::from_utf8_lossy(&body)
                .contains(&format!("StorePath: /nix/store/{}-pkg", hash1))
        );
    }
}
