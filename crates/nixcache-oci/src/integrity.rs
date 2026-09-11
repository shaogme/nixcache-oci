use crate::error::OciError;
use http::HeaderValue;
use sha2::{Digest, Sha256};
use std::fmt;

/// 严格的 OCI SHA-256 content digest。它与 Nix 的 `NarDigest` 有意分开。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContentDigest(String);

impl ContentDigest {
    pub fn parse(raw: &str) -> Result<Self, OciError> {
        let valid = raw.len() == 71
            && raw.starts_with("sha256:")
            && raw[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        if !valid {
            return Err(OciError::InvalidDescriptor {
                target: raw.to_string(),
                details:
                    "expected canonical sha256: followed by 64 lowercase hexadecimal characters"
                        .to_string(),
            });
        }
        Ok(Self(raw.to_string()))
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self::from_hasher(hasher)
    }

    pub(crate) fn from_hasher(hasher: Sha256) -> Self {
        let digest = hasher.finalize();
        let mut value = String::with_capacity(71);
        value.push_str("sha256:");
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(&mut value, "{byte:02x}");
        }
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) fn verify_stream_digest(
    target: &str,
    expected_digest: &str,
    header_digest: Option<&HeaderValue>,
    actual: &ContentDigest,
) -> Result<(), OciError> {
    if let Some(header) = header_digest {
        let header_text = header
            .to_str()
            .map_err(|_| OciError::HeaderDigestMismatch {
                target: target.to_string(),
                header: "<invalid UTF-8>".to_string(),
                expected: actual.to_string(),
            })?;
        let parsed =
            ContentDigest::parse(header_text).map_err(|_| OciError::HeaderDigestMismatch {
                target: target.to_string(),
                header: header_text.to_string(),
                expected: actual.to_string(),
            })?;
        if parsed != *actual {
            return Err(OciError::HeaderDigestMismatch {
                target: target.to_string(),
                header: parsed.to_string(),
                expected: actual.to_string(),
            });
        }
    }
    let expected = ContentDigest::parse(expected_digest)?;
    if expected != *actual {
        return Err(OciError::DigestMismatch {
            target: target.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 校验 buffered response 的 digest、可选 Content-Length 和实际 body。
pub fn verify_buffered_body(
    target: &str,
    expected_digest: Option<&str>,
    header_digest: Option<&HeaderValue>,
    expected_size: Option<u64>,
    content_length: Option<u64>,
    body: &[u8],
) -> Result<ContentDigest, OciError> {
    if let Some(expected_size) = expected_size {
        verify_size(target, expected_size, body.len() as u64)?;
    }
    if let Some(content_length) = content_length {
        verify_size(target, content_length, body.len() as u64)?;
    }

    let actual = ContentDigest::from_bytes(body);
    if let Some(header) = header_digest {
        let header_text = header
            .to_str()
            .map_err(|_| OciError::HeaderDigestMismatch {
                target: target.to_string(),
                header: "<invalid UTF-8>".to_string(),
                expected: actual.to_string(),
            })?;
        let parsed =
            ContentDigest::parse(header_text).map_err(|_| OciError::HeaderDigestMismatch {
                target: target.to_string(),
                header: header_text.to_string(),
                expected: actual.to_string(),
            })?;
        if parsed != actual {
            return Err(OciError::HeaderDigestMismatch {
                target: target.to_string(),
                header: parsed.to_string(),
                expected: actual.to_string(),
            });
        }
    }
    if let Some(expected_digest) = expected_digest {
        let expected = ContentDigest::parse(expected_digest)?;
        if expected != actual {
            return Err(OciError::DigestMismatch {
                target: target.to_string(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
    }
    Ok(actual)
}

pub fn verify_size(target: &str, expected: u64, actual: u64) -> Result<(), OciError> {
    if expected != actual {
        return Err(OciError::SizeMismatch {
            target: target.to_string(),
            expected,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ContentDigest, verify_buffered_body};
    use crate::error::OciError;
    use bytes::Bytes;
    use http::{HeaderMap, HeaderValue};

    #[test]
    fn buffered_body_requires_matching_body_and_header_digests() {
        let body = Bytes::from_static(b"oci body");
        let digest = ContentDigest::from_bytes(&body).to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            "Docker-Content-Digest",
            HeaderValue::from_str(&digest).unwrap(),
        );

        let actual = verify_buffered_body(
            "manifest",
            Some(&digest),
            headers.get("Docker-Content-Digest"),
            Some(body.len() as u64),
            Some(body.len() as u64),
            &body,
        )
        .expect("matching digest and sizes should pass");
        assert_eq!(actual.to_string(), digest);

        let error = verify_buffered_body(
            "manifest",
            Some("sha256:0000000000000000000000000000000000000000000000000000000000000000"),
            None,
            None,
            None,
            &body,
        )
        .expect_err("body digest mismatch must fail");
        assert!(matches!(error, OciError::DigestMismatch { .. }));
    }

    #[test]
    fn invalid_or_conflicting_digest_headers_never_fallback() {
        let body = b"oci body";
        let mut headers = HeaderMap::new();
        headers.insert(
            "Docker-Content-Digest",
            HeaderValue::from_static("sha256:NOT-A-DIGEST"),
        );
        let error = verify_buffered_body(
            "blob",
            None,
            headers.get("Docker-Content-Digest"),
            None,
            None,
            body,
        )
        .expect_err("invalid header must fail even without a requested digest");
        assert!(matches!(error, OciError::HeaderDigestMismatch { .. }));

        assert!(
            ContentDigest::parse(
                "sha256:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            )
            .is_ok()
        );
        assert!(
            ContentDigest::parse(
                "sha256:ABCDEFabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            )
            .is_err()
        );
    }

    #[test]
    fn size_mismatch_is_reported_before_accepting_buffered_body() {
        let error = verify_buffered_body("blob", None, None, Some(99), None, b"short")
            .expect_err("descriptor size mismatch must fail");
        assert!(matches!(error, OciError::SizeMismatch { .. }));
    }
}
