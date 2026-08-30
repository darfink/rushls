//! Cross-origin access, which for a media origin is a caching concern as much
//! as a security one.
//!
//! A player on `example.com` fetching from this origin is the ordinary case,
//! not the exception, so something must answer for CORS. Two details make it
//! worth more than a hardcoded `*`:
//!
//! Behind a CDN, a response whose `Access-Control-Allow-Origin` depends on the
//! request's `Origin` **must** carry `Vary: Origin`. Without it the first
//! viewer's allowed origin is cached and handed to every other viewer, and
//! playback breaks for everyone who is not that first viewer — intermittently,
//! and only in production, where there is a cache in front.
//!
//! And credentialed playback (signed cookies, the common way to gate a stream)
//! cannot use `*` at all: the Fetch specification requires a concrete origin
//! whenever `Access-Control-Allow-Credentials` is set. A configuration that
//! asks for both is a configuration that silently does not work.
//!
//! # What is ours and what is not
//!
//! The header mechanics are [`tower_http`]'s. What stays here is the part it
//! has no opinion about: [`pattern`] decides which origins an allowlist entry
//! stands for, and [`layer`] chooses the `Vary` for each mode.
//!
//! **That `Vary` choice is load-bearing, not decoration.** `tower-http` varies
//! on `Origin` by default in every mode, including `*` — where the answer is a
//! constant and varying on it would key a CDN's cache per viewer origin,
//! storing one copy of every segment per site that embeds the player. So the
//! wildcard mode clears it, and the allowlist mode sets it explicitly rather
//! than inheriting it. A mode added later must make the same decision on
//! purpose; the tests below are what catches it if one does not.

mod pattern;

use std::{sync::Arc, time::Duration};

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, header};
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};

pub use pattern::{OriginPattern, OriginPatternError, WildcardDepth};

use pattern::RequestOrigin;

/// Headers a player needs to read to run its buffer accounting.
///
/// Not configurable because the list follows from what the origin serves: a
/// player that cannot see `Content-Length` or `Content-Range` cannot tell a
/// truncated transfer from a short resource.
const EXPOSED: [HeaderName; 3] = [header::CONTENT_LENGTH, header::CONTENT_RANGE, header::DATE];

/// Everything this origin answers, preflight included.
///
/// Spelled as text because its other use is the `Allow` header on a 405, which
/// is not a CORS concern at all.
pub const ALLOWED_METHODS: &str = "GET, HEAD, OPTIONS";

/// Who may read a response from this origin.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum AllowedOrigins {
    /// No CORS headers at all: same-origin players only.
    Disabled,
    /// `*`, which is right for a public origin and incompatible with
    /// credentials.
    #[default]
    Any,
    /// An allowlist. The matching origin is echoed back, with `Vary: Origin`
    /// so a shared cache keeps the answers apart.
    ///
    /// Entries are [`OriginPattern`]s rather than strings so the grammar is
    /// checked once at startup, not re-interpreted per request.
    Only(Vec<OriginPattern>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorsConfig {
    pub allowed_origins: AllowedOrigins,
    /// Lets a player send cookies or an `Authorization` header.
    ///
    /// Requires an allowlist: see [`CorsConfig::validate`].
    pub allow_credentials: bool,
    /// How long a browser may reuse one preflight result.
    pub max_age: Duration,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: AllowedOrigins::Any,
            allow_credentials: false,
            // Ten minutes: Chrome's ceiling is two hours, but a shorter window
            // keeps a policy change from lingering in browsers for a whole
            // afternoon, and preflights are rare on this origin anyway.
            max_age: Duration::from_mins(10),
        }
    }
}

impl CorsConfig {
    /// Rejects a policy no browser will honour.
    ///
    /// Caught at startup rather than per request because the failure mode is
    /// otherwise invisible from the server side: the origin looks healthy, the
    /// headers look present, and every credentialed fetch fails inside the
    /// browser.
    pub fn validate(&self) -> Result<(), &'static str> {
        match (&self.allowed_origins, self.allow_credentials) {
            (AllowedOrigins::Any, true) => Err(
                "credentialed CORS requires an explicit origin allowlist, because a browser \
                 refuses `Access-Control-Allow-Origin: *` on a credentialed request",
            ),
            (AllowedOrigins::Disabled, true) => {
                Err("credentialed CORS was requested with cross-origin access disabled")
            }
            _ => Ok(()),
        }
    }
}

/// Builds the middleware that answers for cross-origin access, or `None` when
/// the policy is to answer nothing at all.
///
/// `None` rather than a layer that emits no headers, because "no CORS" and "an
/// empty CORS policy" are different: a layer would still intercept `OPTIONS`.
pub fn layer(config: &CorsConfig) -> Option<CorsLayer> {
    let (origins, vary) = match &config.allowed_origins {
        AllowedOrigins::Disabled => return None,
        // A constant answer is the same for every caller, so there is nothing
        // for a cache to key on — and keying on it would be actively harmful.
        AllowedOrigins::Any => (AllowOrigin::any(), Vec::new()),
        AllowedOrigins::Only(allowed) => {
            // Parsed once per request and compared field by field; the grammar
            // itself was checked at startup.
            let allowed = Arc::new(allowed.clone());
            let predicate = AllowOrigin::predicate(move |origin, _| {
                origin
                    .to_str()
                    .ok()
                    .and_then(RequestOrigin::parse)
                    .is_some_and(|origin| {
                        allowed.iter().any(|candidate| candidate.matches(&origin))
                    })
            });
            // Emitted whether or not this particular origin matched: a cache
            // must not reuse a miss for a request that would have hit.
            (predicate, vec![header::ORIGIN])
        }
    };

    Some(
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET, Method::HEAD, Method::OPTIONS])
            // Echoed rather than enumerated: the origin has no header
            // requirements of its own, and a player asking for `Range` should
            // not be refused because a fixed list did not anticipate it.
            .allow_headers(AllowHeaders::mirror_request())
            .expose_headers(EXPOSED)
            .allow_credentials(config.allow_credentials)
            .max_age(config.max_age)
            .vary(vary),
    )
}

/// Adds one field name to `Vary` without dropping what is already there.
///
/// Inserting would silently discard a `Vary` a lower layer had set, which is
/// the kind of bug that only shows up as a cache serving the wrong body.
pub(super) fn append_vary(headers: &mut HeaderMap, name: &str) {
    if let Some(existing) = headers.get(header::VARY) {
        let Ok(existing) = existing.to_str() else {
            return;
        };
        if existing
            .split(',')
            .any(|field| field.trim().eq_ignore_ascii_case(name))
        {
            return;
        }
        if let Ok(value) = HeaderValue::try_from(format!("{existing}, {name}")) {
            headers.insert(header::VARY, value);
        }
        return;
    }
    if let Ok(value) = HeaderValue::try_from(name) {
        headers.insert(header::VARY, value);
    }
}

#[cfg(test)]
mod tests {
    use axum::{Router, body::Body, http::Request, routing::any};
    use tower::ServiceExt;

    use super::*;

    fn pattern(value: &str) -> OriginPattern {
        OriginPattern::parse(value).expect("the pattern is valid")
    }

    fn allowlist() -> CorsConfig {
        CorsConfig {
            allowed_origins: AllowedOrigins::Only(vec![pattern("https://player.example")]),
            ..CorsConfig::default()
        }
    }

    /// Sends one ordinary GET through the configured policy.
    async fn get(config: &CorsConfig, origin: Option<&str>) -> HeaderMap {
        let mut router = Router::new().route("/{*path}", any(|| async { "media" }));
        if let Some(cors) = layer(config) {
            router = router.layer(cors);
        }
        let mut request = Request::builder().uri("/live/camera/0/segment/1.m4s");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        router
            .oneshot(
                request
                    .body(Body::empty())
                    .expect("the request is well formed"),
            )
            .await
            .expect("the router answers")
            .headers()
            .clone()
    }

    fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|value| value.to_str().ok())
    }

    #[tokio::test]
    async fn a_public_origin_allows_anyone_without_varying() {
        let headers = get(&CorsConfig::default(), Some("https://player.example")).await;

        assert_eq!(header(&headers, "access-control-allow-origin"), Some("*"));
        // A constant answer is the same for every caller, so there is nothing
        // for a cache to key on. Varying anyway would store one copy of every
        // segment per site that embeds a player, which is the default
        // configuration's busiest path.
        assert_eq!(header(&headers, "vary"), None);
    }

    #[tokio::test]
    async fn an_allowlisted_origin_is_echoed_and_always_varies() {
        let config = allowlist();

        let allowed = get(&config, Some("https://player.example")).await;
        assert_eq!(
            header(&allowed, "access-control-allow-origin"),
            Some("https://player.example")
        );
        assert_eq!(header(&allowed, "vary"), Some("origin"));

        // The refusal varies too. Were it not to, a CDN could store this
        // header-less response and replay it for the allowed origin, breaking
        // playback for a viewer whose request was perfectly acceptable.
        let refused = get(&config, Some("https://elsewhere.test")).await;
        assert_eq!(header(&refused, "access-control-allow-origin"), None);
        assert_eq!(header(&refused, "vary"), Some("origin"));

        // As does a request that named no origin at all.
        let bare = get(&config, None).await;
        assert_eq!(header(&bare, "vary"), Some("origin"));
    }

    #[tokio::test]
    async fn the_allowlist_grammar_still_decides_what_matches() {
        let config = CorsConfig {
            allowed_origins: AllowedOrigins::Only(vec![pattern("https://*.example.com")]),
            ..CorsConfig::default()
        };

        let subdomain = get(&config, Some("https://a.example.com")).await;
        assert_eq!(
            header(&subdomain, "access-control-allow-origin"),
            Some("https://a.example.com")
        );
        // The `evil-example.com` bug, checked through the wiring rather than
        // only against the matcher.
        let lookalike = get(&config, Some("https://evil-example.com")).await;
        assert_eq!(header(&lookalike, "access-control-allow-origin"), None);
    }

    #[tokio::test]
    async fn an_unallowed_preflight_is_not_approved() {
        let mut router = Router::new().route("/{*path}", any(|| async { "media" }));
        router = router.layer(layer(&allowlist()).expect("an allowlist has a policy"));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/live/camera/index.m3u8")
                    .method(Method::OPTIONS)
                    .header(header::ORIGIN, "https://elsewhere.test")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .body(Body::empty())
                    .expect("the request is well formed"),
            )
            .await
            .expect("the router answers");

        assert_eq!(
            header(response.headers(), "access-control-allow-origin"),
            None,
            "asking permission is not being granted it"
        );
    }

    #[tokio::test]
    async fn disabled_access_writes_nothing() {
        let config = CorsConfig {
            allowed_origins: AllowedOrigins::Disabled,
            ..CorsConfig::default()
        };
        assert!(layer(&config).is_none(), "there is no middleware to apply");

        let headers = get(&config, Some("https://player.example")).await;

        assert_eq!(header(&headers, "access-control-allow-origin"), None);
        assert_eq!(header(&headers, "vary"), None);
    }

    #[tokio::test]
    async fn credentials_require_a_concrete_origin() {
        let wildcard = CorsConfig {
            allow_credentials: true,
            ..CorsConfig::default()
        };
        assert!(wildcard.validate().is_err());

        let allowlisted = CorsConfig {
            allow_credentials: true,
            ..allowlist()
        };
        assert!(allowlisted.validate().is_ok());

        let headers = get(&allowlisted, Some("https://player.example")).await;
        assert_eq!(
            header(&headers, "access-control-allow-credentials"),
            Some("true")
        );
    }

    #[test]
    fn vary_accumulates_rather_than_replacing() {
        let mut headers = HeaderMap::new();
        headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));

        append_vary(&mut headers, "Origin");
        append_vary(&mut headers, "origin");

        assert_eq!(
            header(&headers, "vary"),
            Some("Accept-Encoding, Origin"),
            "an existing field survives and a repeat is not duplicated"
        );
    }
}
