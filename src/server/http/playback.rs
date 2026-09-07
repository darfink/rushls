//! Local JWT verification for viewers.
//!
//! Publishers are admitted once, by a sidecar. Viewers present a token on
//! every playlist reload and every segment, so this path must stay in-process
//! and cheap: decode, compare claims, done. A JWKS URL is the one outbound
//! exception, and it is a key-set refresh, not a per-request hook.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use axum::http::{HeaderMap, StatusCode, header};
use cc_metrics::bearer_token;
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm},
};
use serde::Deserialize;
use serde_json::Value;
use tokio::task::AbortHandle;
use urlencoding::decode as percent_decode;

use crate::{
    delivery::{
        hls::uri::{TOKEN_QUERYPARAM, parse_path},
        uri::parse_media_path,
    },
    domain::StreamId,
    outbound::{Endpoint, HttpClient, Response as OutboundResponse},
};

const DEFAULT_JWKS_REFRESH: Duration = Duration::from_mins(5);
const MINIMUM_JWKS_REFRESH: Duration = Duration::from_secs(30);
const MAXIMUM_JWKS_REFRESH: Duration = Duration::from_hours(1);

/// A scalar a token must present exactly, except `aud` which is membership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimValue {
    String(String),
    Integer(i64),
    Boolean(bool),
}

impl ClaimValue {
    fn matches(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::String(expected), Value::String(got)) => expected == got,
            (Self::Integer(expected), Value::Number(got)) => got.as_i64() == Some(*expected),
            (Self::Boolean(expected), Value::Bool(got)) => expected == got,
            _ => false,
        }
    }
}

impl<'de> Deserialize<'de> for ClaimValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = toml::Value::deserialize(deserializer)?;
        match value {
            toml::Value::String(value) => Ok(Self::String(value)),
            toml::Value::Integer(value) => Ok(Self::Integer(value)),
            toml::Value::Boolean(value) => Ok(Self::Boolean(value)),
            toml::Value::Float(_)
            | toml::Value::Datetime(_)
            | toml::Value::Array(_)
            | toml::Value::Table(_) => Err(serde::de::Error::custom(
                "playback claims must be a string, integer, or boolean",
            )),
        }
    }
}

/// Why a viewer request was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackDenial {
    /// No token, a malformed token, or a token that failed signature or claims.
    Unauthorized,
    /// A valid token that does not admit the requested stream.
    Forbidden,
}

impl PlaybackDenial {
    pub fn status(self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
        }
    }
}

/// What the denial counters read, as one value.
///
/// Named fields rather than a pair of `u64`: the two counts share a type, so
/// an exporter that transposed them would still compile and the mistake would
/// surface only as a metric quietly meaning its opposite.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaybackDenials {
    pub unauthorized: u64,
    pub forbidden: u64,
}

/// Denials counted for operators, labelled only by status.
///
/// The counters live together behind one handle, so cloning this for the
/// exporter shares both or neither — there is no way to hold half of it.
#[derive(Clone, Debug, Default)]
pub struct PlaybackMeters(Arc<DenialCounters>);

#[derive(Debug, Default)]
struct DenialCounters {
    unauthorized: AtomicU64,
    forbidden: AtomicU64,
}

impl PlaybackMeters {
    pub fn deny(&self, denial: PlaybackDenial) {
        let counter = match denial {
            PlaybackDenial::Unauthorized => &self.0.unauthorized,
            PlaybackDenial::Forbidden => &self.0.forbidden,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> PlaybackDenials {
        PlaybackDenials {
            unauthorized: self.0.unauthorized.load(Ordering::Relaxed),
            forbidden: self.0.forbidden.load(Ordering::Relaxed),
        }
    }
}

/// Resolved `[auth.playback]`, before any JWKS fetch.
pub struct PlaybackSettings {
    pub issuer: String,
    pub audience: String,
    pub extra: BTreeMap<String, ClaimValue>,
    pub stream_claim: String,
    pub leeway: Duration,
    pub keys: PlaybackKeyMaterial,
}

/// Exactly one key source, already checked at configuration resolve.
pub enum PlaybackKeyMaterial {
    Secret(Vec<u8>),
    PublicPem(Vec<u8>),
    Jwks {
        endpoint: Endpoint,
        client: Box<HttpClient>,
    },
}

enum Keys {
    Static {
        key: DecodingKey,
        algorithm: Algorithm,
    },
    Jwks {
        keys: Arc<ArcSwap<VerifyingKeys>>,
        _refresh: Option<AbortOnDrop>,
    },
}

/// A key set already in the form verification needs.
///
/// The swapped value is decoded keys rather than the JWKS as fetched, because
/// a viewer presents a token on every segment: parsing a JWK per request would
/// put an RSA key decode on the hot path, and this way a key the origin cannot
/// use is discovered when the set is fetched rather than once per viewer.
#[derive(Default)]
struct VerifyingKeys(Vec<(String, DecodingKey, Algorithm)>);

impl VerifyingKeys {
    /// Keeps the members this origin can verify with, by `kid`.
    ///
    /// A member without a `kid` is skipped because `kid` is what selects it,
    /// and one naming an algorithm outside the documented three is skipped
    /// because no token here may be signed with it. An issuer publishing only
    /// such keys fails the fetch rather than silently admitting nothing.
    fn from_set(set: &JwkSet) -> Self {
        Self(
            set.keys
                .iter()
                .filter_map(|jwk| {
                    let kid = jwk.common.key_id.clone()?;
                    let algorithm = jwk_algorithm(jwk)?;
                    let key = DecodingKey::from_jwk(jwk).ok()?;
                    Some((kid, key, algorithm))
                })
                .collect(),
        )
    }

    fn find(&self, kid: &str) -> Option<(&DecodingKey, Algorithm)> {
        self.0
            .iter()
            .find(|(candidate, ..)| candidate == kid)
            .map(|(_, key, algorithm)| (key, *algorithm))
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Verifies viewer JWTs on the request path.
#[derive(Clone)]
pub struct PlaybackGate {
    keys: Arc<Keys>,
    issuer: Arc<str>,
    audience: Arc<str>,
    extra: Arc<BTreeMap<String, ClaimValue>>,
    stream_claim: Arc<str>,
    leeway: Duration,
    meters: PlaybackMeters,
}

impl PlaybackGate {
    /// Builds a gate, fetching JWKS once when that is the key source.
    pub async fn start(settings: PlaybackSettings) -> Result<Self, PlaybackStartError> {
        let keys = match settings.keys {
            PlaybackKeyMaterial::Secret(secret) => Keys::Static {
                key: DecodingKey::from_secret(&secret),
                algorithm: Algorithm::HS256,
            },
            PlaybackKeyMaterial::PublicPem(pem) => {
                let (key, algorithm) = decoding_key_from_pem(&pem)?;
                Keys::Static { key, algorithm }
            }
            PlaybackKeyMaterial::Jwks { endpoint, client } => {
                let fetched = fetch_jwks(&client, &endpoint).await?;
                let keys = Arc::new(ArcSwap::from_pointee(fetched.keys));
                let refresh =
                    spawn_jwks_refresh(Arc::clone(&keys), *client, endpoint, fetched.refresh_after);
                Keys::Jwks {
                    keys,
                    _refresh: Some(AbortOnDrop(refresh)),
                }
            }
        };
        Ok(Self {
            keys: Arc::new(keys),
            issuer: Arc::from(settings.issuer),
            audience: Arc::from(settings.audience),
            extra: Arc::new(settings.extra),
            stream_claim: Arc::from(settings.stream_claim),
            leeway: settings.leeway,
            meters: PlaybackMeters::default(),
        })
    }

    /// Symmetric-secret gate for tests that do not go through configuration.
    pub fn hmac(
        secret: impl AsRef<[u8]>,
        issuer: &str,
        audience: &str,
        stream_claim: &str,
        leeway: Duration,
        extra: BTreeMap<String, ClaimValue>,
    ) -> Self {
        Self {
            keys: Arc::new(Keys::Static {
                key: DecodingKey::from_secret(secret.as_ref()),
                algorithm: Algorithm::HS256,
            }),
            issuer: Arc::from(issuer),
            audience: Arc::from(audience),
            extra: Arc::new(extra),
            stream_claim: Arc::from(stream_claim),
            leeway,
            meters: PlaybackMeters::default(),
        }
    }

    /// Asymmetric verifying-key gate for tests.
    pub fn public_pem(
        pem: &[u8],
        issuer: &str,
        audience: &str,
        stream_claim: &str,
        leeway: Duration,
        extra: BTreeMap<String, ClaimValue>,
    ) -> Result<Self, PlaybackStartError> {
        let (key, algorithm) = decoding_key_from_pem(pem)?;
        Ok(Self {
            keys: Arc::new(Keys::Static { key, algorithm }),
            issuer: Arc::from(issuer),
            audience: Arc::from(audience),
            extra: Arc::new(extra),
            stream_claim: Arc::from(stream_claim),
            leeway,
            meters: PlaybackMeters::default(),
        })
    }

    pub fn meters(&self) -> PlaybackMeters {
        self.meters.clone()
    }

    /// Authorizes one viewer request, or names why it was refused.
    ///
    /// Which playlist form answers the request is deliberately not decided
    /// here. Delivery reads that from the query string, so the choice cannot
    /// drift from the bytes it keys its cache by.
    pub fn authorize(
        &self,
        path: &str,
        query: Option<&str>,
        headers: &HeaderMap,
    ) -> Result<(), PlaybackDenial> {
        self.authorize_inner(path, query, headers)
            .inspect_err(|denial| self.meters.deny(*denial))
    }

    fn authorize_inner(
        &self,
        path: &str,
        query: Option<&str>,
        headers: &HeaderMap,
    ) -> Result<(), PlaybackDenial> {
        let token = presented_token(headers, query)?;
        let claims = self.decode(&token)?;
        if !self.extra_claims_match(&claims) {
            return Err(PlaybackDenial::Unauthorized);
        }
        if let Some(stream) = requested_stream(path)
            && !stream_claim_matches(&claims, &self.stream_claim, stream.as_str())
        {
            return Err(PlaybackDenial::Forbidden);
        }
        Ok(())
    }

    /// Verifies signature and registered claims, yielding the token's body.
    ///
    /// The key is borrowed rather than cloned: a static key lives as long as
    /// the gate, and a JWKS key lives as long as the guard held across the
    /// call, so neither costs an allocation per request.
    fn decode(&self, token: &str) -> Result<Value, PlaybackDenial> {
        match self.keys.as_ref() {
            Keys::Static { key, algorithm } => self.validate(token, key, *algorithm),
            Keys::Jwks { keys, .. } => {
                let header = decode_header(token).map_err(|_| PlaybackDenial::Unauthorized)?;
                let kid = header.kid.ok_or(PlaybackDenial::Unauthorized)?;
                let set = keys.load();
                let (key, algorithm) = set.find(&kid).ok_or(PlaybackDenial::Unauthorized)?;
                self.validate(token, key, algorithm)
            }
        }
    }

    fn validate(
        &self,
        token: &str,
        key: &DecodingKey,
        algorithm: Algorithm,
    ) -> Result<Value, PlaybackDenial> {
        // `Validation::new` admits exactly one algorithm, which is what keeps a
        // token from naming a weaker one than the key was configured for.
        let mut validation = Validation::new(algorithm);
        validation.set_issuer(std::slice::from_ref(&self.issuer));
        validation.set_audience(std::slice::from_ref(&self.audience));
        validation.leeway = self.leeway.as_secs();
        validation.validate_nbf = true;
        decode::<Value>(token, key, &validation)
            .map(|data| data.claims)
            .map_err(|_| PlaybackDenial::Unauthorized)
    }

    fn extra_claims_match(&self, claims: &Value) -> bool {
        let Some(object) = claims.as_object() else {
            return false;
        };
        self.extra.iter().all(|(name, expected)| {
            object
                .get(name)
                .is_some_and(|value| expected.matches(value))
        })
    }
}

/// The token this request presents, preferring the header a client can set.
///
/// An `Authorization` that is not a Bearer JWT is a refusal rather than a
/// reason to look at the query: a client that meant to authenticate one way
/// and got it wrong should be told so, not silently answered from elsewhere.
fn presented_token<'a>(
    headers: &'a HeaderMap,
    query: Option<&'a str>,
) -> Result<Cow<'a, str>, PlaybackDenial> {
    if let Some(header) = headers.get(header::AUTHORIZATION) {
        let value = header.to_str().map_err(|_| PlaybackDenial::Unauthorized)?;
        return bearer_token(value)
            .map(Cow::Borrowed)
            .ok_or(PlaybackDenial::Unauthorized);
    }
    let encoded = first_query_value(query, TOKEN_QUERYPARAM).ok_or(PlaybackDenial::Unauthorized)?;
    // Emptiness is checked after decoding, so `token=` and a value that
    // percent-decodes to nothing are the same refusal.
    let token = percent_decode(encoded).map_err(|_| PlaybackDenial::Unauthorized)?;
    if token.is_empty() {
        return Err(PlaybackDenial::Unauthorized);
    }
    Ok(token)
}

fn first_query_value<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query.and_then(|query| {
        query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .find_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (key == name).then_some(value)
            })
    })
}

fn requested_stream(path: &str) -> Option<StreamId> {
    parse_path(path).ok().map(|named| named.stream).or_else(|| {
        parse_media_path(path)
            .ok()
            .flatten()
            .map(|named| named.stream)
    })
}

fn stream_claim_matches(claims: &Value, claim: &str, stream: &str) -> bool {
    claims
        .get(claim)
        .and_then(Value::as_str)
        .is_some_and(|value| value == stream)
}

fn decoding_key_from_pem(pem: &[u8]) -> Result<(DecodingKey, Algorithm), PlaybackStartError> {
    if let Ok(key) = DecodingKey::from_rsa_pem(pem) {
        return Ok((key, Algorithm::RS256));
    }
    DecodingKey::from_ec_pem(pem)
        .map(|key| (key, Algorithm::ES256))
        .map_err(|_| PlaybackStartError::PublicKey)
}

/// Which algorithm a key from the set verifies with.
///
/// `alg` is optional in a JWK (RFC 7517 § 4.4) and a good many issuers omit
/// it, so the key's own type answers when the member does not. Refusing those
/// keys would fail closed against correct key sets, which reads to an operator
/// as "the JWKS is ignored" rather than as a missing member.
///
/// The set stays the three algorithms this origin documents: a key naming
/// anything else is not one a token here may be signed with.
fn jwk_algorithm(jwk: &Jwk) -> Option<Algorithm> {
    if let Some(algorithm) = jwk.common.key_algorithm {
        return match algorithm {
            KeyAlgorithm::RS256 => Some(Algorithm::RS256),
            KeyAlgorithm::ES256 => Some(Algorithm::ES256),
            KeyAlgorithm::HS256 => Some(Algorithm::HS256),
            _ => None,
        };
    }
    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Some(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(key) if key.curve == EllipticCurve::P256 => {
            Some(Algorithm::ES256)
        }
        AlgorithmParameters::OctetKey(_) => Some(Algorithm::HS256),
        AlgorithmParameters::EllipticCurve(_) | AlgorithmParameters::OctetKeyPair(_) => None,
    }
}

struct FetchedJwks {
    keys: VerifyingKeys,
    refresh_after: Duration,
}

async fn fetch_jwks(
    client: &HttpClient,
    endpoint: &Endpoint,
) -> Result<FetchedJwks, PlaybackStartError> {
    let response = client
        .get(endpoint)
        .await
        .map_err(PlaybackStartError::JwksFetch)?;
    if !response.status.is_success() {
        return Err(PlaybackStartError::JwksStatus(response.status.as_u16()));
    }
    let set: JwkSet =
        serde_json::from_slice(&response.body).map_err(|_| PlaybackStartError::JwksBody)?;
    let keys = VerifyingKeys::from_set(&set);
    if keys.is_empty() {
        return Err(PlaybackStartError::JwksEmpty);
    }
    Ok(FetchedJwks {
        keys,
        refresh_after: jwks_refresh_after(&response),
    })
}

fn jwks_refresh_after(response: &OutboundResponse) -> Duration {
    cache_control_max_age(&response.headers).unwrap_or(DEFAULT_JWKS_REFRESH)
}

fn cache_control_max_age(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(header::CACHE_CONTROL)?.to_str().ok()?;
    value.split(',').find_map(|directive| {
        // Directive names are case-insensitive (RFC 9111 § 5.2), and an issuer
        // spelling it `Max-Age` should not silently get the compiled default.
        let (name, age) = directive.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("max-age") {
            return None;
        }
        let seconds: u64 = age.trim().parse().ok()?;
        Some(Duration::from_secs(seconds).clamp(MINIMUM_JWKS_REFRESH, MAXIMUM_JWKS_REFRESH))
    })
}

fn spawn_jwks_refresh(
    keys: Arc<ArcSwap<VerifyingKeys>>,
    client: HttpClient,
    endpoint: Endpoint,
    initial: Duration,
) -> AbortHandle {
    tokio::spawn(async move {
        let mut wait = initial;
        loop {
            tokio::time::sleep(wait).await;
            match fetch_jwks(&client, &endpoint).await {
                Ok(fetched) => {
                    keys.store(Arc::new(fetched.keys));
                    wait = fetched.refresh_after;
                }
                Err(_) => wait = DEFAULT_JWKS_REFRESH,
            }
        }
    })
    .abort_handle()
}

/// Failures while building a gate, including the first JWKS fetch.
#[derive(Debug, thiserror::Error)]
pub enum PlaybackStartError {
    #[error("the playback public key is not an RS256 or ES256 PEM key")]
    PublicKey,
    #[error("could not fetch the playback JWKS: {0}")]
    JwksFetch(crate::outbound::OutboundError),
    #[error("the playback JWKS endpoint answered {0}")]
    JwksStatus(u16),
    #[error("the playback JWKS was not a JSON key set")]
    JwksBody,
    #[error("the playback JWKS contained no key this origin can verify with")]
    JwksEmpty,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    use super::*;

    const SECRET: &[u8] = b"playback-hmac-secret";
    const ISSUER: &str = "https://issuer.example";
    const AUDIENCE: &str = "rushls-origin";

    fn gate() -> PlaybackGate {
        PlaybackGate::hmac(
            SECRET,
            ISSUER,
            AUDIENCE,
            "stream",
            Duration::from_secs(30),
            BTreeMap::new(),
        )
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_secs()
    }

    fn token(claims: &Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(SECRET),
        )
        .expect("a test token encodes")
    }

    fn valid_claims(stream: &str) -> Value {
        json!({
            "iss": ISSUER,
            "aud": AUDIENCE,
            "exp": now() + 60,
            "nbf": now() - 5,
            "stream": stream,
        })
    }

    fn authorize(
        gate: &PlaybackGate,
        path: &str,
        query: Option<&str>,
        bearer: Option<&str>,
    ) -> Result<(), PlaybackDenial> {
        let mut headers = HeaderMap::new();
        if let Some(bearer) = bearer {
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {bearer}").parse().expect("ascii"),
            );
        }
        gate.authorize(path, query, &headers)
    }

    #[test]
    fn a_bearer_token_admits_the_named_stream() {
        let jwt = token(&valid_claims("live/camera"));
        assert_eq!(
            authorize(&gate(), "/live/camera/index.m3u8", None, Some(&jwt)),
            Ok(())
        );
    }

    #[test]
    fn a_query_token_admits_the_named_stream_without_a_header() {
        let jwt = token(&valid_claims("live/camera"));
        assert_eq!(
            authorize(
                &gate(),
                "/live/camera/index.m3u8",
                Some(&format!("token={jwt}")),
                None
            ),
            Ok(())
        );
    }

    #[test]
    fn an_aud_array_containing_the_configured_audience_is_accepted() {
        let mut claims = valid_claims("live/camera");
        claims["aud"] = json!([AUDIENCE, "other-service"]);
        let jwt = token(&claims);
        assert_eq!(
            authorize(&gate(), "/live/camera/index.m3u8", None, Some(&jwt)),
            Ok(())
        );
    }

    #[test]
    fn a_token_for_another_stream_is_forbidden() {
        let jwt = token(&valid_claims("live/other"));
        assert_eq!(
            authorize(&gate(), "/live/camera/index.m3u8", None, Some(&jwt)),
            Err(PlaybackDenial::Forbidden)
        );
    }

    #[test]
    fn an_expired_token_is_unauthorized() {
        let mut claims = valid_claims("live/camera");
        claims["exp"] = json!(now() - 120);
        let jwt = token(&claims);
        assert_eq!(
            authorize(&gate(), "/live/camera/index.m3u8", None, Some(&jwt)),
            Err(PlaybackDenial::Unauthorized)
        );
    }

    #[test]
    fn leeway_covers_clock_skew_on_nbf() {
        let mut claims = valid_claims("live/camera");
        claims["nbf"] = json!(now() + 10);
        let jwt = token(&claims);
        assert_eq!(
            authorize(&gate(), "/live/camera/index.m3u8", None, Some(&jwt)),
            Ok(())
        );
    }

    #[test]
    fn a_missing_custom_claim_is_unauthorized() {
        let gate = PlaybackGate::hmac(
            SECRET,
            ISSUER,
            AUDIENCE,
            "stream",
            Duration::from_secs(30),
            BTreeMap::from([("tier".to_owned(), ClaimValue::String("premium".into()))]),
        );
        let jwt = token(&valid_claims("live/camera"));
        assert_eq!(
            authorize(&gate, "/live/camera/index.m3u8", None, Some(&jwt)),
            Err(PlaybackDenial::Unauthorized)
        );
    }

    #[test]
    fn an_empty_query_token_is_unauthorized() {
        assert_eq!(
            authorize(&gate(), "/live/camera/index.m3u8", Some("token="), None),
            Err(PlaybackDenial::Unauthorized)
        );
    }

    #[test]
    fn a_non_bearer_authorization_does_not_fall_through_to_the_query() {
        let jwt = token(&valid_claims("live/camera"));
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Basic abc".parse().expect("ascii"));
        assert_eq!(
            gate().authorize(
                "/live/camera/index.m3u8",
                Some(&format!("token={jwt}")),
                &headers
            ),
            Err(PlaybackDenial::Unauthorized)
        );
    }

    #[test]
    fn the_header_is_verified_when_a_query_token_is_also_present() {
        let header_jwt = token(&valid_claims("live/camera"));
        let query_jwt = token(&valid_claims("live/other"));
        assert_eq!(
            authorize(
                &gate(),
                "/live/camera/index.m3u8",
                Some(&format!("token={query_jwt}")),
                Some(&header_jwt)
            ),
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_jwks_kid_swap_is_visible_without_rebuilding_the_gate() {
        let mut first = jsonwebtoken::jwk::Jwk {
            common: jsonwebtoken::jwk::CommonParameters {
                key_id: Some("one".into()),
                key_algorithm: Some(jsonwebtoken::jwk::KeyAlgorithm::HS256),
                ..jsonwebtoken::jwk::CommonParameters::default()
            },
            algorithm: jsonwebtoken::jwk::AlgorithmParameters::OctetKey(
                jsonwebtoken::jwk::OctetKeyParameters {
                    key_type: jsonwebtoken::jwk::OctetKeyType::Octet,
                    value: base64::Engine::encode(
                        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                        SECRET,
                    ),
                },
            ),
        };
        let keys = Arc::new(ArcSwap::from_pointee(VerifyingKeys::from_set(&JwkSet {
            keys: vec![first.clone()],
        })));
        let gate = PlaybackGate {
            keys: Arc::new(Keys::Jwks {
                keys: Arc::clone(&keys),
                _refresh: None,
            }),
            issuer: Arc::from(ISSUER),
            audience: Arc::from(AUDIENCE),
            extra: Arc::new(BTreeMap::new()),
            stream_claim: Arc::from("stream"),
            leeway: Duration::from_secs(30),
            meters: PlaybackMeters::default(),
        };

        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("one".into());
        let jwt = encode(
            &header,
            &valid_claims("live/camera"),
            &EncodingKey::from_secret(SECRET),
        )
        .expect("encodes");
        assert_eq!(
            authorize(&gate, "/live/camera/index.m3u8", None, Some(&jwt)),
            Ok(())
        );

        first.common.key_id = Some("two".into());
        keys.store(Arc::new(VerifyingKeys::from_set(&JwkSet {
            keys: vec![first],
        })));
        assert_eq!(
            authorize(&gate, "/live/camera/index.m3u8", None, Some(&jwt)),
            Err(PlaybackDenial::Unauthorized),
            "the previous kid is no longer in the set"
        );
    }

    #[test]
    fn a_jwks_key_that_omits_alg_is_read_from_its_key_type() {
        // RFC 7517 § 4.4 makes `alg` optional, and issuers that omit it would
        // otherwise have every token refused with no way to see why.
        let key = jsonwebtoken::jwk::Jwk {
            common: jsonwebtoken::jwk::CommonParameters {
                key_id: Some("one".into()),
                ..jsonwebtoken::jwk::CommonParameters::default()
            },
            algorithm: jsonwebtoken::jwk::AlgorithmParameters::OctetKey(
                jsonwebtoken::jwk::OctetKeyParameters {
                    key_type: jsonwebtoken::jwk::OctetKeyType::Octet,
                    value: base64::Engine::encode(
                        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                        SECRET,
                    ),
                },
            ),
        };
        let gate = PlaybackGate {
            keys: Arc::new(Keys::Jwks {
                keys: Arc::new(ArcSwap::from_pointee(VerifyingKeys::from_set(&JwkSet {
                    keys: vec![key],
                }))),
                _refresh: None,
            }),
            issuer: Arc::from(ISSUER),
            audience: Arc::from(AUDIENCE),
            extra: Arc::new(BTreeMap::new()),
            stream_claim: Arc::from("stream"),
            leeway: Duration::from_secs(30),
            meters: PlaybackMeters::default(),
        };

        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("one".into());
        let jwt = encode(
            &header,
            &valid_claims("live/camera"),
            &EncodingKey::from_secret(SECRET),
        )
        .expect("encodes");
        assert_eq!(
            authorize(&gate, "/live/camera/index.m3u8", None, Some(&jwt)),
            Ok(())
        );
    }

    #[test]
    fn a_jwks_member_without_a_kid_cannot_be_selected() {
        let key = jsonwebtoken::jwk::Jwk {
            common: jsonwebtoken::jwk::CommonParameters::default(),
            algorithm: jsonwebtoken::jwk::AlgorithmParameters::OctetKey(
                jsonwebtoken::jwk::OctetKeyParameters {
                    key_type: jsonwebtoken::jwk::OctetKeyType::Octet,
                    value: base64::Engine::encode(
                        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                        SECRET,
                    ),
                },
            ),
        };
        assert!(
            VerifyingKeys::from_set(&JwkSet { keys: vec![key] }).is_empty(),
            "without a kid there is nothing a token header can name"
        );
    }

    #[test]
    fn jwks_refresh_reads_max_age_case_insensitively() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CACHE_CONTROL,
            "public, MAX-AGE = 120".parse().expect("ascii"),
        );
        assert_eq!(
            cache_control_max_age(&headers),
            Some(Duration::from_mins(2))
        );
    }
}
