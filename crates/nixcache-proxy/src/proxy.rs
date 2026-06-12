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
    index_entries: usize,
    index_generated: String,
    index_ttl: u64,
    repo: String,
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
    let status = StatusResponse {
        index_entries: index_data.entries.len(),
        index_generated: index_data.generated.clone(),
        index_ttl: state.index_ttl,
        repo: state.repo.clone(),
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
