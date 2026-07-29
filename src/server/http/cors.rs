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

use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};

/// Headers a player needs to read to run its buffer accounting.
///
/// Not configurable because the list follows from what the origin serves: a
/// player that cannot see `Content-Length` or `Content-Range` cannot tell a
/// truncated transfer from a short resource.
const EXPOSED: &str = "Content-Length, Content-Range, Date";

/// Everything this origin answers, preflight included.
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
    Only(Vec<String>),
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
            max_age: Duration::from_secs(600),
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

    /// What this policy answers a given request's `Origin` with, if anything.
    fn allowance(&self, origin: Option<&str>) -> Option<Allowance> {
        match &self.allowed_origins {
            AllowedOrigins::Disabled => None,
            AllowedOrigins::Any => Some(Allowance::Any),
            AllowedOrigins::Only(allowed) => {
                // The response varies by `Origin` whether or not this
                // particular one matched: a cache must not reuse a miss for a
                // request that would have hit.
                let origin = origin?;
                allowed
                    .iter()
                    .any(|candidate| candidate == origin)
                    .then(|| Allowance::Echo(origin.to_owned()))
            }
        }
    }

    /// Whether a response's content depends on the request's `Origin`.
    fn varies_by_origin(&self) -> bool {
        matches!(self.allowed_origins, AllowedOrigins::Only(_))
    }
}

enum Allowance {
    Any,
    Echo(String),
}

/// Writes the access-control headers a normal response carries.
pub fn apply(response: &mut axum::response::Response, config: &CorsConfig, request: &HeaderMap) {
    let origin = request
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let headers = response.headers_mut();

    // Emitted even when the origin did not match, and even on a same-origin
    // request that carried no `Origin` at all. A cache that stored this
    // response must not serve it to a request from a different origin, and
    // `Vary` is the only thing that says so.
    if config.varies_by_origin() {
        append_vary(headers, "Origin");
    }

    let Some(allowance) = config.allowance(origin) else {
        return;
    };
    let allowed = match allowance {
        Allowance::Any => HeaderValue::from_static("*"),
        Allowance::Echo(origin) => match HeaderValue::try_from(origin) {
            Ok(value) => value,
            Err(_) => return,
        },
    };
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allowed);
    // Without this a browser player cannot read Content-Length or
    // Content-Range from a cross-origin response, which is what its buffer
    // accounting runs on.
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(EXPOSED),
    );
    if config.allow_credentials {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
}

/// Answers a preflight, or declines to.
///
/// `None` means this `OPTIONS` was not a preflight — no `Origin`, or an origin
/// this policy does not allow — and the caller should treat it as an ordinary
/// request rather than inventing an approval.
pub fn preflight(config: &CorsConfig, request: &HeaderMap) -> Option<axum::response::Response> {
    let origin = request
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())?;
    // A bare OPTIONS is not a preflight, and answering it as an approved one
    // would invent permission the browser never asked for.
    request.get(header::ACCESS_CONTROL_REQUEST_METHOD)?;

    let mut response = axum::response::Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(ALLOWED_METHODS),
    );
    // A preflight's answer depends on all three negotiation headers, so all
    // three belong in `Vary` regardless of how origins are configured.
    for name in [
        "Origin",
        "Access-Control-Request-Method",
        "Access-Control-Request-Headers",
    ] {
        append_vary(headers, name);
    }
    // Echoed rather than enumerated: the origin has no header requirements of
    // its own, and a player asking for `Range` on a preflight should not be
    // refused because a fixed list did not anticipate it.
    if let Some(requested) = request.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested.clone());
    }
    if let Ok(value) = HeaderValue::try_from(config.max_age.as_secs().to_string()) {
        headers.insert(header::ACCESS_CONTROL_MAX_AGE, value);
    }

    let allowance = config.allowance(Some(origin))?;
    let allowed = match allowance {
        Allowance::Any => HeaderValue::from_static("*"),
        Allowance::Echo(origin) => HeaderValue::try_from(origin).ok()?,
    };
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allowed);
    if config.allow_credentials {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    Some(response)
}

/// Adds one field name to `Vary` without dropping what is already there.
///
/// Inserting would silently discard a `Vary` a lower layer had set, which is
/// the kind of bug that only shows up as a cache serving the wrong body.
fn append_vary(headers: &mut HeaderMap, name: &str) {
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
    use super::*;

    fn request(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                header::HeaderName::try_from(*name).expect("a valid header name"),
                HeaderValue::try_from(*value).expect("a valid header value"),
            );
        }
        headers
    }

    fn applied(config: &CorsConfig, request: &HeaderMap) -> HeaderMap {
        let mut response = axum::response::Response::new(axum::body::Body::empty());
        apply(&mut response, config, request);
        response.headers().clone()
    }

    fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|value| value.to_str().ok())
    }

    #[test]
    fn a_public_origin_allows_anyone_without_varying() {
        let headers = applied(
            &CorsConfig::default(),
            &request(&[("origin", "https://player.example")]),
        );

        assert_eq!(header(&headers, "access-control-allow-origin"), Some("*"));
        assert_eq!(
            header(&headers, "access-control-expose-headers"),
            Some(EXPOSED)
        );
        // A constant answer is the same for every caller, so there is nothing
        // for a cache to key on.
        assert_eq!(header(&headers, "vary"), None);
    }

    #[test]
    fn an_allowlisted_origin_is_echoed_and_always_varies() {
        let config = CorsConfig {
            allowed_origins: AllowedOrigins::Only(vec!["https://player.example".into()]),
            ..CorsConfig::default()
        };

        let allowed = applied(&config, &request(&[("origin", "https://player.example")]));
        assert_eq!(
            header(&allowed, "access-control-allow-origin"),
            Some("https://player.example")
        );
        assert_eq!(header(&allowed, "vary"), Some("Origin"));

        // The refusal varies too. Were it not to, a CDN could store this
        // header-less response and replay it for the allowed origin, breaking
        // playback for a viewer whose request was perfectly acceptable.
        let refused = applied(&config, &request(&[("origin", "https://elsewhere.test")]));
        assert_eq!(header(&refused, "access-control-allow-origin"), None);
        assert_eq!(header(&refused, "vary"), Some("Origin"));

        // As does a request that named no origin at all.
        let bare = applied(&config, &request(&[]));
        assert_eq!(header(&bare, "vary"), Some("Origin"));
    }

    #[test]
    fn disabled_access_writes_nothing() {
        let config = CorsConfig {
            allowed_origins: AllowedOrigins::Disabled,
            ..CorsConfig::default()
        };

        let headers = applied(&config, &request(&[("origin", "https://player.example")]));

        assert!(headers.is_empty());
    }

    #[test]
    fn credentials_require_a_concrete_origin() {
        let wildcard = CorsConfig {
            allow_credentials: true,
            ..CorsConfig::default()
        };
        assert!(wildcard.validate().is_err());

        let allowlisted = CorsConfig {
            allowed_origins: AllowedOrigins::Only(vec!["https://player.example".into()]),
            allow_credentials: true,
            ..CorsConfig::default()
        };
        assert!(allowlisted.validate().is_ok());

        let headers = applied(
            &allowlisted,
            &request(&[("origin", "https://player.example")]),
        );
        assert_eq!(
            header(&headers, "access-control-allow-credentials"),
            Some("true")
        );
    }

    #[test]
    fn a_preflight_is_answered_with_the_headers_it_asked_about() {
        let response = preflight(
            &CorsConfig::default(),
            &request(&[
                ("origin", "https://player.example"),
                ("access-control-request-method", "GET"),
                ("access-control-request-headers", "range"),
            ]),
        )
        .expect("a preflight is answered");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(header(headers, "access-control-allow-origin"), Some("*"));
        assert_eq!(
            header(headers, "access-control-allow-methods"),
            Some(ALLOWED_METHODS)
        );
        assert_eq!(
            header(headers, "access-control-allow-headers"),
            Some("range")
        );
        assert_eq!(header(headers, "access-control-max-age"), Some("600"));
        let vary = header(headers, "vary").expect("a preflight varies");
        assert!(vary.contains("Origin"));
        assert!(vary.contains("Access-Control-Request-Headers"));
    }

    #[test]
    fn an_options_request_that_is_not_a_preflight_is_left_alone() {
        // No `Access-Control-Request-Method`, so this is a plain OPTIONS and
        // answering it as an approved preflight would be an invention.
        assert!(
            preflight(
                &CorsConfig::default(),
                &request(&[("origin", "https://player.example")])
            )
            .is_none()
        );
        assert!(
            preflight(
                &CorsConfig::default(),
                &request(&[("access-control-request-method", "GET")])
            )
            .is_none()
        );
    }

    #[test]
    fn an_unallowed_preflight_is_not_approved() {
        let config = CorsConfig {
            allowed_origins: AllowedOrigins::Only(vec!["https://player.example".into()]),
            ..CorsConfig::default()
        };

        assert!(
            preflight(
                &config,
                &request(&[
                    ("origin", "https://elsewhere.test"),
                    ("access-control-request-method", "GET"),
                ])
            )
            .is_none()
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
