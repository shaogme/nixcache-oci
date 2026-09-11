use crate::error::OciError;
use http::{HeaderMap, header::WWW_AUTHENTICATE};
use std::{fmt, str::Chars};

/// 不允许凭据通过 `Debug`、`Display` 或序列化路径泄漏的秘密值。
#[derive(Clone)]
pub(crate) struct SecretToken(String);

impl SecretToken {
    pub(crate) fn new(value: &str) -> Self {
        Self(value.to_string())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Registry 的外部凭据。
///
/// `username` 是 Basic token exchange 的用户名，`secret` 通常是 PAT、云厂商
/// access key 或调用方提供的其他长期凭据。没有用户名时 Generic OCI 不会猜测
/// 一个用户名，也不会把 secret 直接当成 Bearer token。
#[derive(Clone)]
pub struct RegistryCredentials {
    username: Option<String>,
    secret: Option<SecretToken>,
}

impl RegistryCredentials {
    pub fn anonymous() -> Self {
        Self {
            username: None,
            secret: None,
        }
    }

    pub fn new(secret: impl Into<String>) -> Self {
        let secret = secret.into();
        if secret.is_empty() {
            Self::anonymous()
        } else {
            Self {
                username: None,
                secret: Some(SecretToken::new(&secret)),
            }
        }
    }

    pub fn with_username(username: impl Into<String>, secret: impl Into<String>) -> Self {
        let mut credentials = Self::new(secret);
        let username = username.into();
        if !username.is_empty() {
            credentials.username = Some(username);
        }
        credentials
    }

    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    pub fn has_secret(&self) -> bool {
        self.secret
            .as_ref()
            .is_some_and(|secret| !secret.as_str().is_empty())
    }

    pub(crate) fn secret(&self) -> Option<&str> {
        self.secret
            .as_ref()
            .map(SecretToken::as_str)
            .filter(|secret| !secret.is_empty())
    }
}

impl From<&str> for RegistryCredentials {
    fn from(secret: &str) -> Self {
        Self::new(secret)
    }
}

impl From<String> for RegistryCredentials {
    fn from(secret: String) -> Self {
        Self::new(secret)
    }
}

impl From<&String> for RegistryCredentials {
    fn from(secret: &String) -> Self {
        Self::new(secret.clone())
    }
}

impl From<&RegistryCredentials> for RegistryCredentials {
    fn from(credentials: &RegistryCredentials) -> Self {
        credentials.clone()
    }
}

impl fmt::Debug for RegistryCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryCredentials")
            .field("username", &self.username)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Registry 返回的 Bearer challenge。
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BearerChallenge {
    pub realm: String,
    pub service: Option<String>,
    pub scope: Option<String>,
}

impl BearerChallenge {
    pub fn new(
        realm: impl Into<String>,
        service: Option<String>,
        scope: Option<String>,
    ) -> Result<Self, OciError> {
        let challenge = Self {
            realm: realm.into(),
            service,
            scope,
        };
        challenge.validate()?;
        Ok(challenge)
    }

    pub fn cache_key(
        &self,
        default_service: &str,
        default_scope: &str,
    ) -> (String, String, String) {
        (
            self.realm.clone(),
            self.service
                .as_deref()
                .unwrap_or(default_service)
                .to_string(),
            self.scope.as_deref().unwrap_or(default_scope).to_string(),
        )
    }

    pub(crate) fn validate(&self) -> Result<(), OciError> {
        let uri = self
            .realm
            .parse::<http::Uri>()
            .map_err(|_| OciError::AuthChallengeInvalid {
                details: "Bearer realm must be an absolute HTTP(S) URL".to_string(),
            })?;
        let scheme = uri.scheme_str().unwrap_or_default();
        let authority = uri
            .authority()
            .map(|value| value.as_str())
            .unwrap_or_default();
        if !matches!(scheme, "http" | "https") || authority.is_empty() {
            return Err(OciError::AuthChallengeInvalid {
                details: "Bearer realm must be an absolute HTTP(S) URL".to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn is_insecure_non_localhost(&self) -> bool {
        let Ok(uri) = self.realm.parse::<http::Uri>() else {
            return true;
        };
        if uri.scheme_str() != Some("http") {
            return false;
        }
        let host = uri
            .authority()
            .map(|authority| authority.host())
            .unwrap_or_default();
        !matches!(host, "localhost" | "127.0.0.1" | "[::1]")
    }

    pub(crate) fn token_url(&self, default_service: &str, default_scope: &str) -> String {
        let service = self.service.as_deref().unwrap_or(default_service);
        let scope = self.scope.as_deref().unwrap_or(default_scope);
        let mut url = self.realm.clone();
        let separator = if url.contains('?') { '&' } else { '?' };
        url.push(separator);
        url.push_str("service=");
        url.push_str(&encode_query_component(service));
        url.push_str("&scope=");
        url.push_str(&encode_query_component(scope));
        url
    }
}

impl fmt::Debug for BearerChallenge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BearerChallenge")
            .field("realm", &self.realm)
            .field("service", &self.service)
            .field("scope", &self.scope)
            .finish()
    }
}

fn encode_query_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => unreachable!(),
    }
}

/// 从一个或多个 `WWW-Authenticate` header 中选择 Bearer challenge。
pub fn parse_www_authenticate(headers: &HeaderMap) -> Result<Option<BearerChallenge>, OciError> {
    let mut bearer = None;
    for value in headers.get_all(WWW_AUTHENTICATE) {
        let value = value.to_str().map_err(|_| OciError::AuthChallengeInvalid {
            details: "WWW-Authenticate contains invalid UTF-8".to_string(),
        })?;
        if let Some(candidate) = parse_www_authenticate_value(value)? {
            if let Some(existing) = &bearer
                && existing != &candidate
            {
                return Err(OciError::AuthChallengeInvalid {
                    details: "conflicting Bearer challenges".to_string(),
                });
            }
            bearer = Some(candidate);
        }
    }
    Ok(bearer)
}

/// 解析单个 `WWW-Authenticate` header 值，主要用于协议单元测试。
pub fn parse_www_authenticate_value(value: &str) -> Result<Option<BearerChallenge>, OciError> {
    let mut parser = ChallengeParser::new(value);
    let mut bearer = None;
    while parser.skip_separators() {
        let scheme = parser
            .token()?
            .ok_or_else(|| invalid_challenge("missing auth scheme"))?;
        parser.skip_spaces();
        let params = if parser.peek() == Some(',') || parser.peek().is_none() {
            Vec::new()
        } else if parser.next_is_token68() {
            parser.consume_until_comma();
            Vec::new()
        } else {
            parser.auth_params()?
        };

        if scheme.eq_ignore_ascii_case("bearer") {
            let challenge = BearerChallenge::new(
                find_param(&params, "realm")?
                    .ok_or_else(|| invalid_challenge("Bearer challenge is missing realm"))?,
                find_param(&params, "service")?,
                find_param(&params, "scope")?,
            )?;
            if let Some(existing) = &bearer
                && existing != &challenge
            {
                return Err(invalid_challenge("conflicting Bearer parameters"));
            }
            bearer = Some(challenge);
        }
    }
    Ok(bearer)
}

fn find_param(params: &[(String, String)], name: &str) -> Result<Option<String>, OciError> {
    Ok(params
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone()))
}

fn invalid_challenge(details: &str) -> OciError {
    OciError::AuthChallengeInvalid {
        details: details.to_string(),
    }
}

struct ChallengeParser<'a> {
    chars: Chars<'a>,
    lookahead: Option<char>,
}

impl<'a> ChallengeParser<'a> {
    fn new(value: &'a str) -> Self {
        let mut chars = value.chars();
        let lookahead = chars.next();
        Self { chars, lookahead }
    }

    fn peek(&self) -> Option<char> {
        self.lookahead
    }

    fn bump(&mut self) -> Option<char> {
        let current = self.lookahead;
        self.lookahead = self.chars.next();
        current
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.bump();
        }
    }

    fn skip_separators(&mut self) -> bool {
        self.skip_spaces();
        while self.peek() == Some(',') {
            self.bump();
            self.skip_spaces();
        }
        self.peek().is_some()
    }

    fn token(&mut self) -> Result<Option<String>, OciError> {
        let mut token = String::new();
        while let Some(value) = self.peek()
            && is_token_char(value)
        {
            token.push(value);
            self.bump();
        }
        if token.is_empty() {
            Ok(None)
        } else {
            Ok(Some(token))
        }
    }

    fn next_is_token68(&self) -> bool {
        let mut clone = self.clone_for_peek();
        let Some(_) = clone.token().ok().flatten() else {
            return false;
        };
        clone.skip_spaces();
        clone.peek() != Some('=')
    }

    fn clone_for_peek(&self) -> ChallengeParser<'a> {
        // `Chars` is cloneable; this keeps lookahead checks non-consuming.
        Self {
            chars: self.chars.clone(),
            lookahead: self.lookahead,
        }
    }

    fn consume_until_comma(&mut self) {
        let mut quoted = false;
        let mut escaped = false;
        while let Some(value) = self.peek() {
            if escaped {
                escaped = false;
            } else if value == '\\' && quoted {
                escaped = true;
            } else if value == '"' {
                quoted = !quoted;
            } else if value == ',' && !quoted {
                break;
            }
            self.bump();
        }
    }

    fn auth_params(&mut self) -> Result<Vec<(String, String)>, OciError> {
        let mut params: Vec<(String, String)> = Vec::new();
        loop {
            self.skip_spaces();
            let key = self
                .token()?
                .ok_or_else(|| invalid_challenge("invalid auth parameter name"))?;
            self.skip_spaces();
            if self.bump() != Some('=') {
                return Err(invalid_challenge("auth parameter is missing '='"));
            }
            self.skip_spaces();
            let value = self.value()?;
            if let Some((_, old)) = params
                .iter()
                .find(|(old_key, _)| old_key.eq_ignore_ascii_case(&key))
            {
                if old != &value {
                    return Err(invalid_challenge("conflicting duplicate auth parameter"));
                }
            } else {
                params.push((key, value));
            }

            self.skip_spaces();
            if self.peek() != Some(',') {
                break;
            }
            let mut clone = self.clone_for_peek();
            clone.bump();
            clone.skip_spaces();
            let Some(_) = clone.token()? else {
                return Err(invalid_challenge("trailing comma in challenge"));
            };
            clone.skip_spaces();
            if clone.peek() != Some('=') {
                break;
            }
            self.bump();
        }
        Ok(params)
    }

    fn value(&mut self) -> Result<String, OciError> {
        if self.peek() == Some('"') {
            self.bump();
            let mut value = String::new();
            let mut escaped = false;
            while let Some(current) = self.bump() {
                if escaped {
                    value.push(current);
                    escaped = false;
                } else if current == '\\' {
                    escaped = true;
                } else if current == '"' {
                    return Ok(value);
                } else {
                    value.push(current);
                }
            }
            return Err(invalid_challenge("unterminated quoted auth parameter"));
        }
        let value = self
            .token()?
            .ok_or_else(|| invalid_challenge("empty auth parameter"))?;
        Ok(value)
    }
}

fn is_token_char(value: char) -> bool {
    value.is_ascii_alphanumeric()
        || matches!(
            value,
            '!' | '#'
                | '$'
                | '%'
                | '&'
                | '\''
                | '*'
                | '+'
                | '-'
                | '.'
                | '^'
                | '_'
                | '`'
                | '|'
                | '~'
        )
}

#[cfg(test)]
mod tests {
    use super::{BearerChallenge, RegistryCredentials, parse_www_authenticate_value};
    use http::{HeaderMap, HeaderValue};

    #[test]
    fn parses_bearer_parameters_with_quoted_commas_and_query_realm() {
        let challenge = parse_www_authenticate_value(
            r##"Basic realm="public", bEaReR ReAlM="https://auth.example/token?x=1", SERVICE="registry.example", scope="repository:org/cache:pull,push""##,
        )
        .unwrap()
        .unwrap();
        assert_eq!(challenge.realm, "https://auth.example/token?x=1");
        assert_eq!(
            challenge.scope.as_deref(),
            Some("repository:org/cache:pull,push")
        );
        assert_eq!(
            challenge.token_url("fallback", "fallback-scope"),
            "https://auth.example/token?x=1&service=registry.example&scope=repository%3Aorg%2Fcache%3Apull%2Cpush"
        );
    }

    #[test]
    fn rejects_invalid_bearer_challenges() {
        assert!(parse_www_authenticate_value("Bearer service=registry").is_err());
        assert!(parse_www_authenticate_value("Bearer realm=not-a-url").is_err());
        assert!(parse_www_authenticate_value("Bearer realm=\"https://a\"").is_ok());
        assert!(parse_www_authenticate_value("Bearer realm=\"https://a").is_err());
        assert!(
            parse_www_authenticate_value("Bearer realm=\"https://a\", realm=\"https://b\"")
                .is_err()
        );
    }

    #[test]
    fn selects_bearer_from_multiple_authenticate_headers() {
        let mut headers = HeaderMap::new();
        headers.append(
            "WWW-Authenticate",
            HeaderValue::from_static("Basic realm=\"public\""),
        );
        headers.append(
            "WWW-Authenticate",
            HeaderValue::from_static(
                "Bearer realm=\"https://auth.example/token\", scope=\"repository:a/b:pull\"",
            ),
        );

        let challenge = super::parse_www_authenticate(&headers).unwrap().unwrap();
        assert_eq!(challenge.realm, "https://auth.example/token");
        assert_eq!(challenge.scope.as_deref(), Some("repository:a/b:pull"));
    }

    #[test]
    fn credentials_debug_is_redacted() {
        let credentials = RegistryCredentials::with_username("AWS", "secret-value");
        let debug = format!("{credentials:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-value"));
    }

    #[test]
    fn challenge_equality_is_structural() {
        let first = BearerChallenge::new("https://auth", None, None).unwrap();
        let second = BearerChallenge::new("https://auth", None, None).unwrap();
        assert_eq!(first, second);
    }
}
