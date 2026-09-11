//! Requests this node makes to services an administrator configured.
//!
//! The mirror of [`server::http`](crate::server::http), which answers viewers.
//! Both admission and lifecycle hooks talk to operator-supplied endpoints, and
//! they share exactly this much: a pooled connection, bounded time, a bounded
//! response, and a bearer credential. What they do with the answer is entirely
//! different — admission fails closed and never retries, hooks queue and retry
//! — so none of that is here.
//!
//! Two properties come from what is *not* enabled rather than from code, and
//! both are load-bearing. `hyper-util`'s client does not follow redirects, so a
//! bearer token can never be replayed to another origin by a redirecting
//! endpoint. Its `client-proxy` feature is off, so no environment variable can
//! silently route these requests through a third party.
//!
//! Cleartext endpoints are permitted. Whether a hop is protected is not
//! knowable from the URL — a service mesh or a private network may be doing it
//! — so [`Endpoint::is_encrypted`] reports the scheme and leaves the judgement
//! to the operator who chose the address.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Request, StatusCode, Uri, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use rustls::client::ResolvesClientCert;
use rustls::pki_types::CertificateDer;
use thiserror::Error;

/// A validated destination.
///
/// Parsed once at start-up so a malformed endpoint stops the process rather
/// than failing at the first publisher, and so the checks below happen where an
/// operator can still act on them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint(Uri);

impl Endpoint {
    pub fn parse(value: &str) -> Result<Self, EndpointError> {
        let uri: Uri = value
            .parse()
            .map_err(|_| EndpointError::Malformed(value.to_owned()))?;
        let scheme = uri.scheme_str().unwrap_or_default();
        if !matches!(scheme, "http" | "https") {
            return Err(EndpointError::Scheme(value.to_owned()));
        }
        // Cleartext is permitted anywhere. A sidecar reached over a private
        // network or a service mesh that terminates its own TLS is the ordinary
        // deployment, and `http://auth-sidecar:8081` is not a loopback address
        // — refusing it would reject the common case to catch the careless one.
        uri.host().ok_or(EndpointError::NoHost(value.to_owned()))?;
        Ok(Self(uri))
    }

    pub fn uri(&self) -> &Uri {
        &self.0
    }

    /// Whether credentials sent here are protected in transit by this node.
    ///
    /// Cleartext may still be entirely appropriate, so this is a fact to report
    /// rather than a verdict: what protects the hop may be a mesh or a private
    /// network that this process cannot see.
    pub fn is_encrypted(&self) -> bool {
        self.0.scheme_str() == Some("https")
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum EndpointError {
    #[error("`{0}` is not a URL")]
    Malformed(String),
    #[error("`{0}` must use http or https")]
    Scheme(String),
    #[error("`{0}` names no host")]
    NoHost(String),
}

/// A bearer credential, kept out of anything that prints.
#[derive(Clone, derive_more::Debug)]
#[debug("BearerToken([REDACTED])")]
pub struct BearerToken(#[debug(skip)] HeaderValue);

impl BearerToken {
    pub fn new(token: &str) -> Result<Self, InvalidToken> {
        let mut value =
            HeaderValue::try_from(format!("Bearer {token}")).map_err(|_| InvalidToken)?;
        // Marks the value for redaction in hyper's own logging, which would
        // otherwise print it whenever request tracing is turned on.
        value.set_sensitive(true);
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("a bearer token must be printable ASCII without control characters")]
pub struct InvalidToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientConfig {
    pub connect_timeout: Duration,
    /// Covers connect, send, and reading the whole response.
    pub request_timeout: Duration,
    /// Refused past this, so a misbehaving endpoint cannot exhaust memory.
    pub maximum_response_bytes: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_millis(500),
            request_timeout: Duration::from_secs(2),
            maximum_response_bytes: 64 * 1024,
        }
    }
}

/// What an endpoint answered.
///
/// Reported, not interpreted: whether a status is success, worth retrying, or
/// permanent is the caller's policy, and it differs between admission and
/// hooks. The one exception is [`Self::retry_after`], which parses a header
/// rather than deciding anything with it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Response {
    /// `Retry-After` in its delta-seconds form, capped by the caller.
    ///
    /// Only the seconds form. The HTTP-date alternative needs a parsed clock to
    /// mean anything, and an endpoint that answers an event POST with an
    /// absolute date is not a case worth carrying a date parser for; treating
    /// it as absent falls back to the caller's own backoff, which is safe.
    pub fn retry_after(&self, maximum: Duration) -> Option<Duration> {
        let seconds: u64 = self
            .headers
            .get(header::RETRY_AFTER)?
            .to_str()
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some(Duration::from_secs(seconds).min(maximum))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum OutboundError {
    #[error("the request did not complete within its deadline")]
    Timeout,
    #[error("could not reach the endpoint: {0}")]
    Unreachable(String),
    #[error("the response was not readable: {0}")]
    Response(String),
    #[error("the response exceeded {limit} bytes")]
    ResponseTooLarge { limit: usize },
}

/// A pooled HTTP client shared by everything that calls out.
///
/// Cloning shares the connection pool, which is the point: a per-publisher
/// client would pay a TCP and TLS handshake on every admission.
#[derive(Clone, derive_more::Debug)]
pub struct HttpClient {
    #[debug(skip)]
    inner: Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>,
    config: ClientConfig,
}

impl HttpClient {
    /// The same connection pool under different per-request limits.
    ///
    /// A deadline and a response ceiling are properties of a request, not of a
    /// pool, and admission and hooks want different ones: admission runs inside
    /// a publisher's handshake, while a hook is free to wait. Sharing the
    /// client keeps the platform trust store read once and the TLS
    /// configuration built once, which is the part worth sharing.
    ///
    /// `connect_timeout` is fixed when the connector is built and so is not
    /// overridable here.
    #[must_use]
    pub fn with_limits(&self, request_timeout: Duration, maximum_response_bytes: usize) -> Self {
        Self {
            inner: self.inner.clone(),
            config: ClientConfig {
                request_timeout,
                maximum_response_bytes,
                ..self.config
            },
        }
    }

    /// Builds a client that also presents an identity, and may pin its trust.
    ///
    /// Separate from [`Self::new`] rather than an option on it, because the
    /// two answer different questions: `new` is "who do I trust", this is
    /// additionally "who am I". A caller that needs neither should not have to
    /// say so.
    ///
    /// `identity` resolves per handshake, so a rotated certificate is picked up
    /// without rebuilding this client or restarting the process. `roots`
    /// replaces the platform store when present: a deployment pinning the
    /// authority that signed its own services wants exactly that authority and
    /// not every public one as well.
    pub fn with_identity(
        config: ClientConfig,
        identity: Option<Arc<dyn ResolvesClientCert>>,
        roots: Option<Vec<CertificateDer<'static>>>,
    ) -> Result<Self, OutboundError> {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(config.connect_timeout));
        connector.enforce_http(false);

        let builder = rustls::ClientConfig::builder();
        let builder = match roots {
            Some(roots) => {
                let mut store = rustls::RootCertStore::empty();
                for root in roots {
                    store
                        .add(root)
                        .map_err(|error| OutboundError::Unreachable(error.to_string()))?;
                }
                builder.with_root_certificates(store)
            }
            // The platform store, so an endpoint behind an internal authority
            // works once that authority is installed the way everything else
            // on the host already expects.
            None => {
                let mut store = rustls::RootCertStore::empty();
                let loaded = rustls_native_certs::load_native_certs();
                if loaded.certs.is_empty() {
                    let reason = loaded
                        .errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(OutboundError::Unreachable(format!(
                        "no platform trust roots could be loaded: {reason}"
                    )));
                }
                store.add_parsable_certificates(loaded.certs);
                builder.with_root_certificates(store)
            }
        };

        let tls = match identity {
            Some(resolver) => builder.with_client_cert_resolver(resolver),
            None => builder.with_no_client_auth(),
        };

        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);

        Ok(Self {
            inner: Client::builder(TokioExecutor::new()).build(connector),
            config,
        })
    }

    /// Builds a client trusting the platform's certificate store.
    ///
    /// The platform store rather than a bundled root set, so an endpoint behind
    /// an internal certificate authority works once that authority is installed
    /// the way everything else on the host already expects.
    pub fn new(config: ClientConfig) -> Result<Self, OutboundError> {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(config.connect_timeout));
        // The TLS layer wraps this one, so it must not reject an https URL
        // before ever seeing it.
        connector.enforce_http(false);
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|error| OutboundError::Unreachable(error.to_string()))?
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);

        Ok(Self {
            inner: Client::builder(TokioExecutor::new()).build(connector),
            config,
        })
    }

    /// Fetches one URL and reads the whole response.
    ///
    /// Used for rotating key sets: the body is JSON, not a form, and there is
    /// nothing to POST. Redirects are still not followed — a JWKS URL that
    /// 30x's the origin elsewhere would be a configuration error, not a hop
    /// this client should take on its own.
    pub async fn get(&self, endpoint: &Endpoint) -> Result<Response, OutboundError> {
        let request = Request::get(endpoint.uri())
            .body(Full::new(Bytes::new()))
            .map_err(|error| OutboundError::Unreachable(error.to_string()))?;
        tokio::time::timeout(self.config.request_timeout, self.send(request))
            .await
            .map_err(|_| OutboundError::Timeout)?
    }

    /// Sends one request and reads the whole response.
    pub async fn post(
        &self,
        endpoint: &Endpoint,
        content_type: &'static str,
        bearer: Option<&BearerToken>,
        body: Bytes,
    ) -> Result<Response, OutboundError> {
        self.post_with_headers(endpoint, content_type, bearer, body, &HeaderMap::new())
            .await
    }

    /// Sends application headers alongside the ordinary content type and credential.
    /// The latter remain authoritative even if the supplied map contains them.
    pub async fn post_with_headers(
        &self,
        endpoint: &Endpoint,
        content_type: &'static str,
        bearer: Option<&BearerToken>,
        body: Bytes,
        headers: &HeaderMap,
    ) -> Result<Response, OutboundError> {
        let mut request = Request::post(endpoint.uri())
            .header(header::CONTENT_TYPE, HeaderValue::from_static(content_type))
            .body(Full::new(body))
            .map_err(|error| OutboundError::Unreachable(error.to_string()))?;
        request.headers_mut().extend(headers.clone());
        request
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        request.headers_mut().remove(header::AUTHORIZATION);
        if let Some(bearer) = bearer {
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, bearer.0.clone());
        }

        tokio::time::timeout(self.config.request_timeout, self.send(request))
            .await
            .map_err(|_| OutboundError::Timeout)?
    }

    async fn send(&self, request: Request<Full<Bytes>>) -> Result<Response, OutboundError> {
        let response = self
            .inner
            .request(request)
            .await
            .map_err(|error| OutboundError::Unreachable(error.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let limit = self.config.maximum_response_bytes;
        // Bounded while it is read rather than checked afterwards: a
        // Content-Length is only a claim, and by the time an oversized body has
        // been collected the memory is already spent.
        let body = Limited::new(response.into_body(), limit)
            .collect()
            .await
            .map_err(|error| {
                if error.is::<http_body_util::LengthLimitError>() {
                    OutboundError::ResponseTooLarge { limit }
                } else {
                    OutboundError::Response(error.to_string())
                }
            })?
            .to_bytes();

        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests;
