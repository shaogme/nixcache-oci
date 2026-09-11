//! Registry endpoint construction shared by all domain clients.

use http::HeaderMap;
use http::header::LOCATION;

pub(super) fn manifest_url(scheme: &str, registry: &str, repo: &str, reference: &str) -> String {
    format!("{scheme}://{registry}/v2/{repo}/nix-cache/manifests/{reference}")
}

pub(super) fn blob_url(scheme: &str, registry: &str, repo: &str, digest: &str) -> String {
    format!("{scheme}://{registry}/v2/{repo}/nix-cache/blobs/{digest}")
}

pub(super) fn upload_url(scheme: &str, registry: &str, repo: &str) -> String {
    format!("{scheme}://{registry}/v2/{repo}/nix-cache/blobs/uploads/")
}

pub(super) fn tags_url(scheme: &str, registry: &str, repo: &str) -> String {
    format!("{scheme}://{registry}/v2/{repo}/nix-cache/tags/list")
}

pub(super) fn with_digest(url: &str, digest: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}digest={digest}")
}

pub(super) fn with_last_cursor(url: &str, cursor: &str) -> String {
    format!("{url}?n=100&last={}", encode_query_value(cursor))
}

pub(super) fn encode_query_value(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                vec![byte as char]
            } else {
                vec!['%', hex_digit(byte >> 4), hex_digit(byte & 0x0f)]
            }
        })
        .collect()
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => unreachable!(),
    }
}

pub(super) fn resolved_location(scheme: &str, registry: &str, location: &str) -> String {
    if location.starts_with('/') {
        format!("{scheme}://{registry}{location}")
    } else {
        location.to_string()
    }
}

pub(super) fn update_location(
    scheme: &str,
    registry: &str,
    session_url: &mut String,
    headers: &HeaderMap,
) {
    if let Some(location) = headers.get(LOCATION).and_then(|value| value.to_str().ok()) {
        *session_url = resolved_location(scheme, registry, location);
    }
}

pub(super) fn compute_sha256_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    format!(
        "sha256:{}",
        hash.iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}
