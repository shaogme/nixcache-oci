use http::Uri;
use std::{
    fmt,
    net::{IpAddr, Ipv6Addr},
    str::FromStr,
};
use thiserror::Error;

/// Registry endpoint 使用的传输协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegistryScheme {
    Http,
    Https,
}

impl RegistryScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

impl fmt::Display for RegistryScheme {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Registry endpoint 解析或 Location 解析失败时的诊断错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegistryEndpointError {
    #[error("registry endpoint is empty")]
    Empty,

    #[error("registry endpoint '{input}' has unsupported scheme '{scheme}'")]
    UnsupportedScheme { input: String, scheme: String },

    #[error("registry endpoint '{input}' has no authority")]
    MissingAuthority { input: String },

    #[error("registry endpoint '{input}' contains userinfo")]
    UserInfo { input: String },

    #[error("registry endpoint '{input}' contains a query or fragment")]
    QueryOrFragment { input: String },

    #[error("registry endpoint '{input}' has invalid authority: {details}")]
    InvalidAuthority { input: String, details: String },

    #[error("registry location '{input}' is invalid: {details}")]
    InvalidLocation { input: String, details: String },
}

/// 规范化后的 OCI Registry endpoint。
///
/// `authority` 只包含小写 host 和规范化后的端口，`base_path` 为空或以 `/`
/// 开头且没有尾部 `/`。路径大小写保持调用方输入。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RegistryEndpoint {
    scheme: RegistryScheme,
    host: String,
    port: Option<u16>,
    authority: String,
    base_path: String,
}

impl RegistryEndpoint {
    /// 解析裸 host、显式 HTTP(S) URL 以及带部署前缀的 endpoint。
    pub fn parse(input: &str) -> Result<Self, RegistryEndpointError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(RegistryEndpointError::Empty);
        }
        if trimmed.contains('#') {
            return Err(RegistryEndpointError::QueryOrFragment {
                input: trimmed.to_string(),
            });
        }

        if let Some(separator) = trimmed.find("://") {
            let raw_scheme = &trimmed[..separator];
            let scheme = parse_scheme(trimmed, raw_scheme)?;
            return Self::parse_with_scheme(trimmed, scheme, &trimmed[separator + 3..]);
        }

        let provisional = Self::parse_with_scheme(trimmed, RegistryScheme::Https, trimmed)?;
        let scheme = if is_local_host(&provisional.host) {
            RegistryScheme::Http
        } else {
            RegistryScheme::Https
        };
        if scheme == RegistryScheme::Https {
            Ok(provisional)
        } else {
            Self::parse_with_scheme(trimmed, scheme, trimmed)
        }
    }

    fn parse_with_scheme(
        input: &str,
        scheme: RegistryScheme,
        authority_and_path: &str,
    ) -> Result<Self, RegistryEndpointError> {
        if authority_and_path.is_empty() || authority_and_path.starts_with('/') {
            return Err(RegistryEndpointError::MissingAuthority {
                input: input.to_string(),
            });
        }

        let uri_text = format!("{}://{}", scheme.as_str(), authority_and_path);
        let uri =
            uri_text
                .parse::<Uri>()
                .map_err(|error| RegistryEndpointError::InvalidAuthority {
                    input: input.to_string(),
                    details: error.to_string(),
                })?;
        let authority = uri.authority().map(|value| value.as_str()).ok_or_else(|| {
            RegistryEndpointError::MissingAuthority {
                input: input.to_string(),
            }
        })?;
        if authority.contains('@') {
            return Err(RegistryEndpointError::UserInfo {
                input: input.to_string(),
            });
        }
        if uri.query().is_some() {
            return Err(RegistryEndpointError::QueryOrFragment {
                input: input.to_string(),
            });
        }

        let (host, port) = parse_authority(input, authority)?;
        let base_path = normalize_base_path(uri.path());
        let authority = format_authority(&host, port);
        Ok(Self {
            scheme,
            host,
            port,
            authority,
            base_path,
        })
    }

    pub fn scheme(&self) -> RegistryScheme {
        self.scheme
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// 返回不含方括号的规范化 host，便于后端类型探测。
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn base_path(&self) -> &str {
        &self.base_path
    }

    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.authority)
    }

    /// 将 OCI API 路径拼接到 endpoint，确保 base path 与 API path 之间只有一个 `/`。
    pub fn api_url(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        format!("{}{}{}{}", self.origin(), self.base_path, '/', path)
    }

    /// 解析 Registry 返回的绝对、root-relative 或 base path-relative Location。
    pub fn resolve_location(&self, location: &str) -> Result<String, RegistryEndpointError> {
        let location = location.trim();
        if location.is_empty() {
            return Err(RegistryEndpointError::InvalidLocation {
                input: location.to_string(),
                details: "location is empty".to_string(),
            });
        }
        if location.contains('#') {
            return Err(RegistryEndpointError::InvalidLocation {
                input: location.to_string(),
                details: "fragments are not valid request targets".to_string(),
            });
        }

        if let Some(separator) = location.find("://") {
            let raw_scheme = &location[..separator];
            let scheme = parse_scheme(location, raw_scheme).map_err(|error| {
                RegistryEndpointError::InvalidLocation {
                    input: location.to_string(),
                    details: error.to_string(),
                }
            })?;
            let uri = location.parse::<Uri>().map_err(|error| {
                RegistryEndpointError::InvalidLocation {
                    input: location.to_string(),
                    details: error.to_string(),
                }
            })?;
            let authority = uri.authority().map(|value| value.as_str()).ok_or_else(|| {
                RegistryEndpointError::InvalidLocation {
                    input: location.to_string(),
                    details: "absolute location has no authority".to_string(),
                }
            })?;
            if authority.contains('@') {
                return Err(RegistryEndpointError::InvalidLocation {
                    input: location.to_string(),
                    details: "userinfo is not allowed".to_string(),
                });
            }
            parse_authority(location, authority).map_err(|error| {
                RegistryEndpointError::InvalidLocation {
                    input: location.to_string(),
                    details: error.to_string(),
                }
            })?;
            let normalized_scheme = scheme.as_str();
            let rest = &location[separator + 3..];
            return Ok(format!("{}://{}", normalized_scheme, rest));
        }

        if location.starts_with("//") {
            return Err(RegistryEndpointError::InvalidLocation {
                input: location.to_string(),
                details: "network-path references are not supported".to_string(),
            });
        }

        let resolved = if location.starts_with('/') {
            format!("{}{}", self.origin(), location)
        } else {
            format!(
                "{}{}/{}",
                self.origin(),
                self.base_path,
                location.trim_start_matches('/')
            )
        };
        resolved
            .parse::<Uri>()
            .map_err(|error| RegistryEndpointError::InvalidLocation {
                input: location.to_string(),
                details: error.to_string(),
            })?;
        Ok(resolved)
    }

    pub fn service_name(&self) -> &str {
        &self.authority
    }

    pub(crate) fn replace_host(&self, host: &str) -> Self {
        let host = host.to_ascii_lowercase();
        Self {
            scheme: self.scheme,
            host: host.clone(),
            port: self.port,
            authority: format_authority(&host, self.port),
            base_path: self.base_path.clone(),
        }
    }
}

impl FromStr for RegistryEndpoint {
    type Err = RegistryEndpointError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

impl fmt::Display for RegistryEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", self.origin(), self.base_path)
    }
}

fn parse_scheme(input: &str, scheme: &str) -> Result<RegistryScheme, RegistryEndpointError> {
    match scheme.to_ascii_lowercase().as_str() {
        "http" => Ok(RegistryScheme::Http),
        "https" => Ok(RegistryScheme::Https),
        _ => Err(RegistryEndpointError::UnsupportedScheme {
            input: input.to_string(),
            scheme: scheme.to_string(),
        }),
    }
}

fn parse_authority(
    input: &str,
    authority: &str,
) -> Result<(String, Option<u16>), RegistryEndpointError> {
    if authority.is_empty() {
        return Err(RegistryEndpointError::MissingAuthority {
            input: input.to_string(),
        });
    }

    let (host, port) = if authority.starts_with('[') {
        let closing =
            authority
                .find(']')
                .ok_or_else(|| RegistryEndpointError::InvalidAuthority {
                    input: input.to_string(),
                    details: "unterminated IPv6 literal".to_string(),
                })?;
        let host = &authority[1..closing];
        if host.parse::<Ipv6Addr>().is_err() {
            return Err(RegistryEndpointError::InvalidAuthority {
                input: input.to_string(),
                details: "invalid IPv6 literal".to_string(),
            });
        }
        let suffix = &authority[closing + 1..];
        let port = match suffix {
            "" => None,
            suffix => {
                let port = suffix.strip_prefix(':').ok_or_else(|| {
                    RegistryEndpointError::InvalidAuthority {
                        input: input.to_string(),
                        details: "characters after IPv6 literal must be a port".to_string(),
                    }
                })?;
                parse_port(input, Some(port))?
            }
        };
        (host.to_ascii_lowercase(), port)
    } else {
        if authority.contains('[') || authority.contains(']') || authority.matches(':').count() > 1
        {
            return Err(RegistryEndpointError::InvalidAuthority {
                input: input.to_string(),
                details: "IPv6 literals must be enclosed in brackets".to_string(),
            });
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, parse_port(input, Some(port))?),
            None => (authority, None),
        };
        (host.to_ascii_lowercase(), port)
    };

    if host.is_empty()
        || host.chars().any(|character| {
            character.is_ascii_whitespace() || matches!(character, '/' | '?' | '#')
        })
    {
        return Err(RegistryEndpointError::InvalidAuthority {
            input: input.to_string(),
            details: "host is empty or contains an invalid character".to_string(),
        });
    }
    Ok((host, port))
}

fn parse_port(input: &str, port: Option<&str>) -> Result<Option<u16>, RegistryEndpointError> {
    let Some(port) = port else {
        return Ok(None);
    };
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RegistryEndpointError::InvalidAuthority {
            input: input.to_string(),
            details: "port must be a decimal number".to_string(),
        });
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| RegistryEndpointError::InvalidAuthority {
            input: input.to_string(),
            details: "port is outside the valid range".to_string(),
        })?;
    Ok(Some(port))
}

fn format_authority(host: &str, port: Option<u16>) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

fn normalize_base_path(path: &str) -> String {
    let path = path.trim_matches('/');
    if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    }
}

fn is_local_host(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback() || address.is_unspecified())
}

#[cfg(test)]
mod tests {
    use super::{RegistryEndpoint, RegistryEndpointError, RegistryScheme};

    #[test]
    fn parses_and_normalizes_registry_endpoints() {
        let cases = [
            ("ghcr.io", "https://ghcr.io"),
            ("  HTTPS://GHCR.IO/ ", "https://ghcr.io"),
            ("http://127.0.0.1:5000", "http://127.0.0.1:5000"),
            ("https://127.0.0.1:5000", "https://127.0.0.1:5000"),
            ("localhost:5000/", "http://localhost:5000"),
            (
                "https://registry.example/Harbor/",
                "https://registry.example/Harbor",
            ),
            ("https://[::1]:5000/registry", "https://[::1]:5000/registry"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                RegistryEndpoint::parse(input).unwrap().to_string(),
                expected
            );
        }
    }

    #[test]
    fn preserves_path_case_and_normalizes_port() {
        let endpoint = RegistryEndpoint::parse("HTTPS://REGISTRY.example:05000/Harbor/").unwrap();
        assert_eq!(endpoint.scheme(), RegistryScheme::Https);
        assert_eq!(endpoint.authority(), "registry.example:5000");
        assert_eq!(endpoint.base_path(), "/Harbor");
    }

    #[test]
    fn rejects_invalid_registry_endpoints() {
        for input in [
            "ftp://registry.example",
            "https://",
            "https://user:password@registry.example",
            "https://registry.example/path?query=1",
            "https://[::1",
            "https://registry.example:not-a-port",
        ] {
            assert!(RegistryEndpoint::parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn builds_api_urls_and_resolves_locations() {
        let endpoint = RegistryEndpoint::parse("https://REGISTRY.example/Harbor/").unwrap();
        assert_eq!(
            endpoint.api_url("/v2/team/repo/nix-cache/manifests/latest"),
            "https://registry.example/Harbor/v2/team/repo/nix-cache/manifests/latest"
        );
        assert_eq!(
            endpoint
                .resolve_location("https://uploads.example/session?x=1")
                .unwrap(),
            "https://uploads.example/session?x=1"
        );
        assert_eq!(
            endpoint.resolve_location("/Harbor/session").unwrap(),
            "https://registry.example/Harbor/session"
        );
        assert_eq!(
            endpoint.resolve_location("session").unwrap(),
            "https://registry.example/Harbor/session"
        );
    }

    #[test]
    fn invalid_location_is_reported() {
        let endpoint = RegistryEndpoint::parse("registry.example").unwrap();
        assert!(matches!(
            endpoint.resolve_location("ftp://registry.example/session"),
            Err(RegistryEndpointError::InvalidLocation { .. })
        ));
    }
}
