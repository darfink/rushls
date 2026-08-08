//! Which origins an allowlist entry stands for.
//!
//! An `Origin` is three parts and no more — scheme, host, optional port, never
//! a path — so both a pattern and an incoming header decompose into the same
//! shape and matching is field by field. That is the whole reason this is not
//! string globbing: every boundary a wildcard could blur is compared
//! explicitly instead.
//!
//! # Why not a glob
//!
//! `origin.ends_with("example.com")` is the obvious implementation and it is a
//! well-known vulnerability: it accepts `https://evil-example.com`, which
//! anyone can register. The suffix here is always matched with its separating
//! dot, so a wildcard can only ever stand for whole labels.
//!
//! Patterns are parsed once at startup. A request compares; it never parses.

use std::fmt;

use thiserror::Error;

/// How many labels a leading wildcard stands for.
///
/// Chosen in the pattern rather than by a separate setting, so reading an
/// allowlist tells you what it covers. A configuration knob would make the
/// same string mean different things depending on a line somewhere else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WildcardDepth {
    /// `*` — exactly one label, the semantics of a TLS wildcard certificate.
    ///
    /// The conservative default. Widening it silently would enlarge the
    /// credentialed surface without anyone writing that down, and a deep
    /// subdomain is the more likely takeover target: there are more of them
    /// and they are watched less.
    SingleLabel,
    /// `**` — one or more labels, for a subtree an operator genuinely owns.
    AnyDepth,
}

/// One entry in a CORS allowlist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OriginPattern {
    /// One origin, matched whole.
    Exact {
        scheme: String,
        host: String,
        port: Option<String>,
    },
    /// A wildcard in the leftmost label, standing for subdomains of
    /// `host_suffix`. The suffix itself is never matched: `https://*.a.test`
    /// does not permit `https://a.test`, which must be listed separately if it
    /// is wanted.
    Subdomains {
        scheme: String,
        host_suffix: String,
        port: Option<String>,
        depth: WildcardDepth,
    },
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum OriginPatternError {
    #[error("`{0}` is missing a scheme, for example `https://player.example.com`")]
    NoScheme(String),
    #[error("`{0}` has no host")]
    NoHost(String),
    /// An operator will write this, and an origin never has a path, so it
    /// would match nothing at all. Refusing beats failing silently later.
    #[error("`{0}` must not contain a path; an origin is only scheme, host, and port")]
    HasPath(String),
    #[error("`{0}` has an invalid port")]
    BadPort(String),
    /// The `evil-example.com` bug, written by hand.
    #[error(
        "`{0}` must separate its wildcard with a dot, as `https://*.example.com`; without it \
         the pattern would also accept a host that merely ends in those characters"
    )]
    UnseparatedWildcard(String),
    #[error(
        "`{0}` may only use a wildcard as its leftmost label, as `https://*.example.com` or \
         `https://**.example.com`"
    )]
    MisplacedWildcard(String),
    /// Any sandboxed iframe can send `Origin: null`, so allowlisting it grants
    /// access to everyone rather than to a specific someone.
    #[error("`null` cannot be allowlisted, because any page can present it")]
    Null,
}

impl OriginPattern {
    /// Reads one allowlist entry.
    ///
    /// Rejects anything that is not an origin with an optional leading
    /// wildcard. Mid-pattern globs are refused rather than supported: they are
    /// not meaningful origin syntax, and accepting them would turn this into
    /// an arbitrary matcher whose safety is far harder to argue.
    pub fn parse(value: &str) -> Result<Self, OriginPatternError> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("null") {
            return Err(OriginPatternError::Null);
        }
        let lowered = value.to_ascii_lowercase();
        let (scheme, rest) = lowered
            .split_once("://")
            .ok_or_else(|| OriginPatternError::NoScheme(value.to_owned()))?;
        if scheme.is_empty() {
            return Err(OriginPatternError::NoScheme(value.to_owned()));
        }
        // A trailing `/` is the common version of this, and `https://a.test/`
        // would otherwise be parsed as a host that can never appear.
        if rest.contains('/') {
            return Err(OriginPatternError::HasPath(value.to_owned()));
        }

        let (host, port) = split_port(rest).ok_or_else(|| {
            if rest.is_empty() {
                OriginPatternError::NoHost(value.to_owned())
            } else {
                OriginPatternError::BadPort(value.to_owned())
            }
        })?;
        if host.is_empty() {
            return Err(OriginPatternError::NoHost(value.to_owned()));
        }
        let port = normalize_port(scheme, port);

        let Some((wildcard, suffix)) = host
            .strip_prefix("**")
            .map(|rest| (WildcardDepth::AnyDepth, rest))
            .or_else(|| {
                host.strip_prefix('*')
                    .map(|rest| (WildcardDepth::SingleLabel, rest))
            })
        else {
            // No leading wildcard, so a `*` anywhere else is a glob this
            // deliberately does not implement.
            if host.contains('*') {
                return Err(OriginPatternError::MisplacedWildcard(value.to_owned()));
            }
            return Ok(Self::Exact {
                scheme: scheme.to_owned(),
                host: host.to_owned(),
                port,
            });
        };

        let Some(host_suffix) = suffix.strip_prefix('.') else {
            return Err(OriginPatternError::UnseparatedWildcard(value.to_owned()));
        };
        if host_suffix.is_empty() {
            return Err(OriginPatternError::NoHost(value.to_owned()));
        }
        if host_suffix.contains('*') {
            return Err(OriginPatternError::MisplacedWildcard(value.to_owned()));
        }
        Ok(Self::Subdomains {
            scheme: scheme.to_owned(),
            host_suffix: host_suffix.to_owned(),
            port,
            depth: wildcard,
        })
    }

    /// Whether this entry permits a request's origin.
    pub fn matches(&self, origin: &RequestOrigin<'_>) -> bool {
        match self {
            Self::Exact { scheme, host, port } => {
                scheme.eq_ignore_ascii_case(origin.scheme)
                    && host.eq_ignore_ascii_case(origin.host)
                    && port.as_deref() == origin.port
            }
            Self::Subdomains {
                scheme,
                host_suffix,
                port,
                depth,
            } => {
                if !scheme.eq_ignore_ascii_case(origin.scheme) || port.as_deref() != origin.port {
                    return false;
                }
                // Stripping the dot separately is what anchors the boundary:
                // `evil-example.com` sheds `example.com` but has no dot left,
                // and `example.com` itself sheds everything and has nothing.
                let Some(labels) = strip_suffix_ignore_ascii_case(origin.host, host_suffix) else {
                    return false;
                };
                let Some(labels) = labels.strip_suffix('.') else {
                    return false;
                };
                if labels.is_empty() {
                    return false;
                }
                match depth {
                    WildcardDepth::SingleLabel => !labels.contains('.'),
                    WildcardDepth::AnyDepth => true,
                }
            }
        }
    }
}

impl fmt::Display for OriginPattern {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact { scheme, host, port } => {
                write!(output, "{scheme}://{host}")?;
                port.as_ref()
                    .map_or(Ok(()), |port| write!(output, ":{port}"))
            }
            Self::Subdomains {
                scheme,
                host_suffix,
                port,
                depth,
            } => {
                let wildcard = match depth {
                    WildcardDepth::SingleLabel => "*",
                    WildcardDepth::AnyDepth => "**",
                };
                write!(output, "{scheme}://{wildcard}.{host_suffix}")?;
                port.as_ref()
                    .map_or(Ok(()), |port| write!(output, ":{port}"))
            }
        }
    }
}

/// A request's `Origin`, decomposed once so every pattern can compare.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestOrigin<'a> {
    scheme: &'a str,
    host: &'a str,
    port: Option<&'a str>,
}

impl<'a> RequestOrigin<'a> {
    /// Reads an `Origin` header, or `None` if it is not one.
    ///
    /// Borrowed throughout: matching a request allocates nothing.
    pub fn parse(value: &'a str) -> Option<Self> {
        // `null` parses as nothing rather than as a host, so it can never be
        // matched even by an allowlist that somehow contains it.
        if value.eq_ignore_ascii_case("null") {
            return None;
        }
        let (scheme, rest) = value.split_once("://")?;
        if scheme.is_empty() || rest.contains('/') {
            return None;
        }
        let (host, port) = split_port(rest)?;
        if host.is_empty() {
            return None;
        }
        Some(Self {
            scheme,
            host,
            port: normalize_port_ref(scheme, port),
        })
    }
}

/// Splits a trailing `:port`, leaving an IPv6 literal's own colons alone.
///
/// `None` means the port was present but not a number.
fn split_port(value: &str) -> Option<(&str, Option<&str>)> {
    // An IPv6 host is bracketed precisely so its colons are unambiguous, so
    // the port can only follow the closing bracket.
    let searchable = match value.rfind(']') {
        Some(end) => &value[end..],
        None => value,
    };
    let Some(colon) = searchable.rfind(':') else {
        return Some((value, None));
    };
    let colon = value.len() - (searchable.len() - colon);
    let port = &value[colon + 1..];
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((&value[..colon], Some(port)))
}

/// Drops a port a browser would never send.
///
/// An origin is serialized with its default port omitted, so `https://a.test`
/// and `https://a.test:443` are the same origin — and a pattern spelling the
/// port out would otherwise match nothing.
fn normalize_port(scheme: &str, port: Option<&str>) -> Option<String> {
    normalize_port_ref(scheme, port).map(str::to_owned)
}

fn normalize_port_ref<'a>(scheme: &str, port: Option<&'a str>) -> Option<&'a str> {
    let port = port?;
    let default = match scheme {
        "http" | "ws" => "80",
        "https" | "wss" => "443",
        _ => return Some(port),
    };
    (port != default).then_some(port)
}

fn strip_suffix_ignore_ascii_case<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let split = value.len().checked_sub(suffix.len())?;
    value
        .is_char_boundary(split)
        .then(|| value.split_at(split))
        .filter(|(_, tail)| tail.eq_ignore_ascii_case(suffix))
        .map(|(head, _)| head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(pattern: &str) -> OriginPattern {
        OriginPattern::parse(pattern).expect("the pattern is valid")
    }

    fn permits(pattern: &str, origin: &str) -> bool {
        let origin = RequestOrigin::parse(origin).expect("the origin is well formed");
        parse(pattern).matches(&origin)
    }

    #[test]
    fn an_exact_pattern_matches_only_itself() {
        assert!(permits("https://a.test", "https://a.test"));
        assert!(!permits("https://a.test", "https://b.test"));
        assert!(!permits("https://a.test", "https://sub.a.test"));
        // A scheme downgrade is a different origin, and accepting it would
        // hand a network attacker everything the real origin has.
        assert!(!permits("https://a.test", "http://a.test"));
    }

    /// The classic CORS wildcard vulnerability, in one place.
    ///
    /// Every row here is accepted by the obvious `ends_with` implementation.
    #[test]
    fn a_wildcard_never_matches_a_host_that_merely_ends_the_same_way() {
        // Registrable by anyone, and one character away from the real thing.
        assert!(!permits(
            "https://*.example.com",
            "https://evil-example.com"
        ));
        // The apex is not a subdomain of itself.
        assert!(!permits("https://*.example.com", "https://example.com"));
        // A suffix in the middle is not a suffix.
        assert!(!permits(
            "https://*.video.example.com",
            "https://video.example.com.evil.test"
        ));
        // Scheme and port are part of the origin, not decoration.
        assert!(!permits("https://*.example.com", "http://a.example.com"));
        assert!(!permits(
            "https://*.example.com",
            "https://a.example.com:8443"
        ));

        assert!(permits("https://*.example.com", "https://a.example.com"));
    }

    #[test]
    fn one_star_is_one_label_and_two_are_any_depth() {
        assert!(permits(
            "https://*.video.example.com",
            "https://a.video.example.com"
        ));
        assert!(!permits(
            "https://*.video.example.com",
            "https://b.c.video.example.com"
        ));

        assert!(permits(
            "https://**.video.example.com",
            "https://a.video.example.com"
        ));
        assert!(permits(
            "https://**.video.example.com",
            "https://b.c.video.example.com"
        ));
        // Depth does not reach the apex either way.
        assert!(!permits(
            "https://**.video.example.com",
            "https://video.example.com"
        ));
    }

    #[test]
    fn a_port_is_matched_exactly_and_a_default_one_is_the_same_as_none() {
        assert!(permits(
            "https://*.example.com:8443",
            "https://a.example.com:8443"
        ));
        assert!(!permits(
            "https://*.example.com:8443",
            "https://a.example.com"
        ));
        assert!(!permits(
            "https://*.example.com",
            "https://a.example.com:8443"
        ));

        // A browser omits the default port, so spelling it out must not
        // produce a pattern that can never match.
        assert!(permits(
            "https://a.example.com:443",
            "https://a.example.com"
        ));
        assert!(permits("http://a.example.com:80", "http://a.example.com"));
    }

    #[test]
    fn an_ipv6_literal_keeps_its_own_colons() {
        assert!(permits("http://[::1]:8080", "http://[::1]:8080"));
        assert!(!permits("http://[::1]:8080", "http://[::1]:9090"));
        assert!(permits("http://[::1]", "http://[::1]"));
    }

    #[test]
    fn a_wildcard_is_only_allowed_as_the_leftmost_label() {
        for rejected in [
            "https://foo.*.example.com",
            "https://foo*bar.example.com",
            "https://ex*ample.com",
        ] {
            assert_eq!(
                OriginPattern::parse(rejected),
                Err(OriginPatternError::MisplacedWildcard(rejected.to_owned())),
                "{rejected}"
            );
        }
    }

    #[test]
    fn a_wildcard_must_be_separated_by_a_dot() {
        // Permitting this is precisely the `evil-example.com` bug.
        assert_eq!(
            OriginPattern::parse("https://*example.com"),
            Err(OriginPatternError::UnseparatedWildcard(
                "https://*example.com".to_owned()
            ))
        );
    }

    #[test]
    fn malformed_entries_are_refused_with_the_reason() {
        use OriginPatternError::*;
        let cases = [
            ("player.example.com", NoScheme("player.example.com".into())),
            (
                "https://player.example.com/",
                HasPath("https://player.example.com/".into()),
            ),
            (
                "https://player.example.com/hls",
                HasPath("https://player.example.com/hls".into()),
            ),
            ("https://", NoHost("https://".into())),
            ("https://a.test:http", BadPort("https://a.test:http".into())),
            ("null", Null),
            ("NULL", Null),
        ];
        for (rejected, expected) in cases {
            assert_eq!(OriginPattern::parse(rejected), Err(expected), "{rejected}");
        }
    }

    #[test]
    fn a_null_origin_never_matches_anything() {
        // Any sandboxed iframe can present it, so it identifies nobody.
        assert!(RequestOrigin::parse("null").is_none());
    }

    #[test]
    fn patterns_are_case_insensitive_the_way_origins_are() {
        assert!(permits("HTTPS://A.Example.COM", "https://a.example.com"));
        assert!(permits("https://*.Example.COM", "https://A.example.com"));
    }

    #[test]
    fn a_parsed_pattern_prints_back_the_way_it_was_written() {
        for pattern in [
            "https://example.com",
            "https://*.example.com",
            "https://**.video.example.com",
            "https://*.example.com:8443",
        ] {
            assert_eq!(parse(pattern).to_string(), pattern);
        }
    }

    #[test]
    fn hostile_origin_patterns_never_panic() {
        let mut state = 0x3c6e_f5a1_9b2d_8470_u64;
        for _ in 0..4_000 {
            let pattern = crate::test_fuzz::string(&mut state, 64);
            // A pattern either parses or is refused; both answers are fine.
            let _ = OriginPattern::parse(&pattern);
        }
    }
}
