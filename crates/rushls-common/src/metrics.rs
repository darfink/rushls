//! Small, transport-independent pieces shared by metrics endpoints.
//!
//! Metric collection and HTTP serving stay in the application. These helpers
//! provide bearer-token parsing, constant-time comparison, and Prometheus
//! label escaping without coupling callers to a registry or HTTP framework.

use std::fmt;

use subtle::ConstantTimeEq;

/// An optional bearer credential for a metrics endpoint.
///
/// The value is deliberately not exposed through `Debug`, because endpoint
/// configuration is commonly included in startup diagnostics.
#[derive(Clone, Eq, PartialEq)]
pub struct MetricsToken(Vec<u8>);

impl MetricsToken {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    /// Whether this token could never be presented as a bearer credential.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Compare a presented credential without exposing matching-prefix timing.
    pub fn matches(&self, presented: &[u8]) -> bool {
        self.0.as_slice().ct_eq(presented).into()
    }
}

impl fmt::Debug for MetricsToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MetricsToken([REDACTED])")
    }
}

/// Extract a bearer token from an `Authorization` field.
pub fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer")
        && !token.is_empty()
        && !token.chars().any(char::is_whitespace))
    .then_some(token)
}

/// Escape a value for a Prometheus text-format label.
pub fn escape_label(value: &str) -> String {
    value
        .replace('\\', r"\\")
        .replace('\n', r"\n")
        .replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens_are_parsed_strictly() {
        assert_eq!(bearer_token("Bearer secret"), Some("secret"));
        assert_eq!(bearer_token("bearer secret"), Some("secret"));
        assert_eq!(bearer_token("Basic secret"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer secret extra"), None);
    }

    #[test]
    fn labels_escape_prometheus_control_characters() {
        assert_eq!(escape_label("a\\b\n\"c"), r#"a\\b\n\"c"#);
    }

    #[test]
    fn token_debug_output_does_not_reveal_the_secret() {
        let debug = format!("{:?}", MetricsToken::new("secret"));
        assert_eq!(debug, "MetricsToken([REDACTED])");
        assert!(!debug.contains("secret"));
    }
}
