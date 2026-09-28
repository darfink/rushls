//! Asking a service the operator runs whether a publisher may publish.
//!
//! # Fail closed
//!
//! Every way this can go wrong denies the publication. A timeout, an
//! unreachable service, a 500, a body that is not the agreed shape — all of it
//! becomes [`AdmissionError::Service`]. An authenticator that let a publisher
//! through because its authority was unreachable would be worse than one that
//! was never configured.
//!
//! # No retries
//!
//! This sits directly on the publisher's admission deadline, and the service
//! may have side effects — a seat taken, a session recorded. Retrying would
//! spend the deadline twice and duplicate whatever the first attempt did. The
//! `request_id` is there so a service that wants idempotence can have it.
//!
//! # What the response may decide
//!
//! Identity, and a publish profile *by name*. Never the profile itself.
//!
//! The reason is legibility rather than defence. The admission service is
//! trusted — it may select a profile that widens what this node accepts, which
//! is how a deployment expresses "this account may publish 4K" on an origin
//! whose default is smaller. What it may not do is *define* that profile in its
//! response, because then no one could tell what an origin accepts by reading
//! its configuration: the answer would live partly in a service's source.
//!
//! Keeping every admissible set in the file makes it reviewable, diffable, and
//! validated at startup, and leaves the response choosing among sets rather
//! than inventing one. A deployment that needs a new shape adds a profile and
//! restarts, which is the same cost as any other change to what this node
//! admits. Names are looked up locally, and an unknown one denies.

use std::collections::BTreeMap;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    domain::{BoxFuture, StreamId},
    outbound::{BearerToken, Endpoint, HttpClient, OutboundError, Response},
};

use super::{AdmissionError, Authenticator, Principal, PublishGrant, PublishRequest, StreamPolicy};

const JSON: &str = "application/json";
const DEFAULT_POLICY: &str = "default";

/// Where to ask, and what the answer may select.
#[derive(Clone, derive_more::Debug)]
pub struct HttpAuthConfig {
    pub endpoint: Endpoint,
    /// Applied when an allowing response names no policy.
    ///
    /// The unnamed `[publish]` profile itself, rather than an entry a reserved name
    /// points at: naming the default would make it possible to have none.
    pub default: StreamPolicy,
    /// Every policy a response may name, resolved at start-up.
    pub policies: BTreeMap<String, StreamPolicy>,
    #[debug(skip)]
    pub bearer: Option<BearerToken>,
}

/// Delegates admission to an operator-run service.
#[derive(Clone, derive_more::Debug)]
#[debug("HttpAuthenticator {{ endpoint: {} }}", config.endpoint)]
pub struct HttpAuthenticator {
    config: HttpAuthConfig,
    client: HttpClient,
}

impl HttpAuthenticator {
    pub fn new(config: HttpAuthConfig, client: HttpClient) -> Self {
        Self { config, client }
    }

    fn grant(&self, allowed: Allowed) -> Result<PublishGrant, AdmissionError> {
        if allowed.stream_id.trim().is_empty() {
            return Err(service("the response named an empty stream"));
        }
        if allowed.principal.trim().is_empty() {
            return Err(service("the response named an empty principal"));
        }
        let policy =
            match allowed.profile.as_deref() {
                // Naming nothing is the common case and takes `[publish]` itself.
                None => &self.config.default,
                // Fails closed rather than falling back to the default: a service
                // naming a profile this node does not have is either misconfigured
                // or looking at a different version of the configuration, and
                // quietly substituting one would apply limits nobody chose.
                Some(name) => self.config.policies.get(name).ok_or_else(|| {
                    service(format!("the response named unknown profile `{name}`"))
                })?,
            };

        Ok(PublishGrant {
            stream_id: StreamId::new(allowed.stream_id),
            principal: Principal(allowed.principal),
            policy: policy.clone(),
        })
    }
}

impl Authenticator for HttpAuthenticator {
    fn authenticate<'a>(
        &'a self,
        request: &'a PublishRequest,
    ) -> BoxFuture<'a, Result<PublishGrant, AdmissionError>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&AdmissionRequest::new(request))
                .map_err(|error| service(format!("the request could not be built: {error}")))?;

            let response = self
                .client
                .post(
                    &self.config.endpoint,
                    JSON,
                    self.config.bearer.as_ref(),
                    Bytes::from(body),
                )
                .await
                .map_err(|error| unreachable(&error))?;

            match decision(&response)? {
                Decision::Allow(allowed) => self.grant(allowed),
                // A deny is the service working, so its reason is the
                // publisher's answer rather than a service failure. The reason
                // is deliberately not relayed to the publisher: the protocol
                // has no field for it, and it is the service's own vocabulary.
                Decision::Deny { .. } => Err(AdmissionError::InvalidCredential),
            }
        })
    }
}

/// Reads the decision, treating anything unexpected as a failure to decide.
fn decision(response: &Response) -> Result<Decision, AdmissionError> {
    if !response.status.is_success() {
        return Err(service(format!("the service answered {}", response.status)));
    }
    serde_json::from_slice(&response.body)
        .map_err(|error| service(format!("the response was not a decision: {error}")))
}

fn unreachable(error: &OutboundError) -> AdmissionError {
    service(error.to_string())
}

fn service(reason: impl Into<String>) -> AdmissionError {
    AdmissionError::Service(reason.into().into_boxed_str())
}

/// What this node asks.
///
/// Versioned so a service can tell which shape it is being sent without
/// guessing from which fields are present.
/// Additive request fields keep version 1; a breaking shape change bumps it.
#[derive(Debug, Serialize)]
struct AdmissionRequest<'a> {
    version: u8,
    /// Unique per attempt, so a service that wants idempotence has a key.
    request_id: String,
    protocol: crate::domain::IngestProtocol,
    resource: &'a crate::domain::PublishResource,
    credential: Credential,
    client: &'a crate::domain::ClientInfo,
}

impl<'a> AdmissionRequest<'a> {
    fn new(request: &'a PublishRequest) -> Self {
        Self {
            version: 1,
            request_id: Uuid::now_v7().to_string(),
            protocol: request.protocol,
            resource: &request.resource,
            credential: Credential {
                // A credential is bytes, not text: SRT and RTMP both let a
                // publisher present whatever it likes, so encoding it keeps a
                // non-UTF-8 key from being mangled or from failing the whole
                // request.
                encoding: "base64",
                value: BASE64.encode(request.credential.expose()),
            },
            client: &request.client,
        }
    }
}

#[derive(Debug, Serialize)]
struct Credential {
    encoding: &'static str,
    value: String,
}

/// What the service answers.
///
/// Narrow on purpose. Everything absent here is something this node keeps
/// authority over.
#[derive(Debug, Deserialize)]
#[serde(tag = "decision", rename_all = "lowercase", deny_unknown_fields)]
enum Decision {
    Allow(Allowed),
    Deny {
        #[allow(dead_code, reason = "the service's own vocabulary, read by a human")]
        reason: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allowed {
    stream_id: String,
    principal: String,
    /// A `[publish.profile.<name>]` to apply instead of `[publish]`.
    profile: Option<String>,
}

impl Default for HttpAuthConfig {
    fn default() -> Self {
        Self {
            endpoint: Endpoint::parse("http://127.0.0.1:8081/admit")
                .expect("a constant endpoint is valid"),
            default: StreamPolicy::permissive(),
            policies: BTreeMap::from([(DEFAULT_POLICY.into(), StreamPolicy::permissive())]),
            bearer: None,
        }
    }
}

#[cfg(test)]
mod tests;
