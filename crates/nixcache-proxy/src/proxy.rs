use crate::index::CacheIndex;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use nixcache_oci::OciClient;
use serde::Serialize;
use serde_json::json;
use tracing::error;

#[derive(Clone)]
pub struct AppState {
    pub repo: String,
    pub index_ttl: u64,
    pub upstream_caches: Vec<String>,
    pub index: CacheIndex,
    pub oci_client: OciClient,
    pub http_client: reqwest::Client,
}

#[derive(Serialize)]
struct StatusResponse {
    remote_connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_error: Option<String>,
    registry: String,
    repo: String,
    index_entries: usize,
    index_generated: String,
    index_ttl: u64,
    upstream: Vec<String>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/nix-cache-info", get(serve_cache_info))
        .route("/public-key", get(serve_public_key))
        .route("/_status", get(serve_status))
        .route("/_refresh", post(handle_refresh))
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
    let index_data = state.index.get_data().await;
    if index_data.public_key.is_empty() {
        (StatusCode::NOT_FOUND, "No public key configured\n").into_response()
    } else {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        let body = format!("{}\n", index_data.public_key);
        (StatusCode::OK, headers, body).into_response()
    }
}

async fn serve_status(State(state): State<AppState>) -> impl IntoResponse {
    let index_data = state.index.get_data().await;
    let (remote_connected, remote_error) = state.index.remote_status().await;
    let status = StatusResponse {
        remote_connected,
        remote_error,
        registry: state.index.registry().to_string(),
        repo: state.repo.clone(),
        index_entries: index_data.entries.len(),
        index_generated: index_data.generated.clone(),
        index_ttl: state.index_ttl,
        upstream: state.upstream_caches.clone(),
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

async fn serve_narinfo(State(state): State<AppState>, Path(hash_ext): Path<String>) -> Response {
    if !hash_ext.ends_with(".narinfo") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let store_hash = hash_ext.trim_end_matches(".narinfo");

    // 1. Look up in OCI index
    if let Some(entry) = state.index.lookup(store_hash).await {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/x-nix-narinfo"));
        return (StatusCode::OK, headers, entry.narinfo).into_response();
    }

    // 2. Fallback to upstream
    for cache_url in &state.upstream_caches {
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
        match state.oci_client.stream_blob(&digest).await {
            Ok(resp) if resp.status().is_success() => {
                let content_len = resp.content_length();
                let mut headers = HeaderMap::new();
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

                let stream = resp.bytes_stream();
                let body = Body::from_stream(stream);
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
    for cache_url in &state.upstream_caches {
        let upstream_url = format!("{}/nar/{}", cache_url, nar_name);
        match state.http_client.get(&upstream_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let content_len = resp.content_length();
                let mut headers = HeaderMap::new();
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
    use axum::http::Request;
    use http_body_util::BodyExt;
    use nixcache_oci::{CacheIndexData, IndexEntry};
    use std::{collections::HashMap, path::PathBuf};
    use tower::ServiceExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[tokio::test]
    async fn test_cache_info_endpoint() {
        let index = CacheIndex::new("ghcr.io", "test/repo", "", PathBuf::from("/tmp"), 300);
        let index_data = CacheIndexData {
            public_key: "test-key-1:abcd".to_string(),
            ..Default::default()
        };
        index.update_data_in_memory(index_data).await;

        let state = AppState {
            repo: "test/repo".to_string(),
            index_ttl: 300,
            upstream_caches: vec!["https://cache.nixos.org".to_string()],
            index,
            oci_client: OciClient::new("ghcr.io", "test/repo", "", true),
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
        let index = CacheIndex::new("ghcr.io", "test/repo", "", PathBuf::from("/tmp"), 300);
        let index_data = CacheIndexData {
            public_key: "test-key-1:abcd".to_string(),
            ..Default::default()
        };
        index.update_data_in_memory(index_data).await;

        let state = AppState {
            repo: "test/repo".to_string(),
            index_ttl: 300,
            upstream_caches: vec![],
            index: index.clone(),
            oci_client: OciClient::new("ghcr.io", "test/repo", "", true),
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
        index.update_data_in_memory(CacheIndexData::default()).await;
        let empty_state = AppState {
            repo: "test/repo".to_string(),
            index_ttl: 300,
            upstream_caches: vec![],
            index,
            oci_client: OciClient::new("ghcr.io", "test/repo", "", true),
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
        let index = CacheIndex::new("ghcr.io", "test/repo", "", PathBuf::from("/tmp"), 300);
        let mut entries = HashMap::new();
        entries.insert(
            "hash1".to_string(),
            IndexEntry {
                name: "pkg1".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/hash1-pkg1\n".to_string(),
                nar_digest: "sha256:digest1".to_string(),
                nar_size: 100,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );

        let index_data = CacheIndexData {
            generated: "2026-08-28T00:00:00Z".to_string(),
            entries,
            ..Default::default()
        };
        index.update_data_in_memory(index_data).await;

        let state = AppState {
            repo: "test/repo".to_string(),
            index_ttl: 300,
            upstream_caches: vec!["https://cache.nixos.org".to_string()],
            index,
            oci_client: OciClient::new("ghcr.io", "test/repo", "", true),
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

        let index = CacheIndex::new("ghcr.io", "test/repo", "", PathBuf::from("/tmp"), 300);

        let local_narinfo = "StorePath: /nix/store/localhash-pkg\nURL: nar/local.nar.xz\n";
        let mut entries = HashMap::new();
        entries.insert(
            "localhash".to_string(),
            IndexEntry {
                name: "local-pkg".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: local_narinfo.to_string(),
                nar_digest: "sha256:localdigest".to_string(),
                nar_size: 1024,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );

        let index_data = CacheIndexData {
            entries,
            ..Default::default()
        };
        index.update_data_in_memory(index_data).await;

        let upstream_narinfo = "StorePath: /nix/store/upstreamhash-pkg\nURL: nar/upstream.nar.xz\n";
        Mock::given(method("GET"))
            .and(path("/upstreamhash.narinfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/x-nix-narinfo")
                    .set_body_string(upstream_narinfo),
            )
            .mount(&upstream_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/notfoundhash.narinfo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&upstream_server)
            .await;

        let state = AppState {
            repo: "test/repo".to_string(),
            index_ttl: 300,
            upstream_caches: vec![upstream_url],
            index,
            oci_client: OciClient::new("ghcr.io", "test/repo", "", true),
            http_client: reqwest::Client::new(),
        };

        let app = create_router(state);

        // 1. Local hit
        let local_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/localhash.narinfo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(local_resp.status(), StatusCode::OK);
        let local_body = local_resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(String::from_utf8_lossy(&local_body), local_narinfo);

        // 2. Upstream fallback hit
        let upstream_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/upstreamhash.narinfo")
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
                    .uri("/notfoundhash.narinfo")
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
                    .uri("/localhash.notnarinfo")
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

        let index = CacheIndex::new(&oci_host, "test/repo", "", PathBuf::from("/tmp"), 300);

        let mut entries = HashMap::new();
        entries.insert(
            "localhash".to_string(),
            IndexEntry {
                name: "local-pkg".to_string(),
                system: Some("x86_64-linux".to_string()),
                narinfo: "StorePath: /nix/store/localhash-pkg\nURL: nar/local.nar.xz\n".to_string(),
                nar_digest: "sha256:localdigest123".to_string(),
                nar_size: 5,
                added: "2026-08-28T00:00:00Z".to_string(),
            },
        );

        let index_data = CacheIndexData {
            entries,
            ..Default::default()
        };
        index.update_data_in_memory(index_data).await;

        // Mock OCI Blob streaming
        Mock::given(method("GET"))
            .and(path("/v2/test/repo/nix-cache/blobs/sha256:localdigest123"))
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
            index_ttl: 300,
            upstream_caches: vec![upstream_url],
            index,
            oci_client: OciClient::new(&oci_host, "test/repo", "", false),
            http_client: reqwest::Client::new(),
        };

        let app = create_router(state);

        // 1. Local NAR stream
        let local_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/nar/local.nar.xz")
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

        // 3. Not found anywhere -> 404
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
}
