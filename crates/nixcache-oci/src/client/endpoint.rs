//! Registry endpoint construction shared by all domain clients.

use crate::{
    backend::RegistryEndpoint,
    error::{OciError, TransportError},
};
use http::HeaderMap;
use http::header::LOCATION;

pub(super) fn manifest_url(endpoint: &RegistryEndpoint, repo: &str, reference: &str) -> String {
    endpoint.api_url(&format!("/v2/{repo}/nix-cache/manifests/{reference}"))
}

pub(super) fn blob_url(endpoint: &RegistryEndpoint, repo: &str, digest: &str) -> String {
    endpoint.api_url(&format!("/v2/{repo}/nix-cache/blobs/{digest}"))
}

pub(super) fn upload_url(endpoint: &RegistryEndpoint, repo: &str) -> String {
    endpoint.api_url(&format!("/v2/{repo}/nix-cache/blobs/uploads/"))
}

pub(super) fn tags_url(endpoint: &RegistryEndpoint, repo: &str) -> String {
    endpoint.api_url(&format!("/v2/{repo}/nix-cache/tags/list"))
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

pub(super) fn resolved_location(
    endpoint: &RegistryEndpoint,
    location: &str,
) -> Result<String, OciError> {
    endpoint.resolve_location(location).map_err(OciError::from)
}

pub(super) fn update_location(
    endpoint: &RegistryEndpoint,
    session_url: &mut String,
    headers: &HeaderMap,
) -> Result<(), OciError> {
    if let Some(location) = headers.get(LOCATION) {
        let location = location
            .to_str()
            .map_err(|_| OciError::Transport(TransportError::HeaderParse { header: "Location" }))?;
        *session_url = resolved_location(endpoint, location)?;
    }
    Ok(())
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
