//! Operator-facing configuration and its translation into runtime policy.
//!
//! This module describes administrator intent, not the shape of the internal
//! pipeline. Low-level configuration starts from its library defaults and only
//! the deliberately supported operator choices are applied here.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt, fs,
    net::SocketAddr,
    num::NonZeroU32,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use bytesize::ByteSize;
use conf::{Conf, find_parameter};
use scuffle_rtmp::session::server::ServerSessionTimeouts;
use serde::Deserialize;
use thiserror::Error;

use crate::{
    admission::{
        Authenticator, HttpAuthConfig, HttpAuthenticator, IngestTimingPolicy,
        OpenStreamAuthenticator, Principal, PublishGrant, StaticPublisher,
        StaticStreamAuthenticator, StreamPolicy, TakeoverPolicy,
    },
    delivery::hls::uri::UriBase,
    delivery::store::{DurationRule, TargetDurationMultiple},
    domain::{Codec, FrameRate, StreamId},
    hooks::{HookConfig, HooksConfig},
    observe::lifecycle::Kind,
    outbound::{BearerToken, ClientConfig, Endpoint, HttpClient},
    segment::SegmentationPolicy,
    server::{
        NodeConfig,
        http::{AllowedOrigins, CorsConfig, HttpConfig, OriginPattern, TlsSettings},
        metrics::{ExportPolicy, MetricsConfig, MetricsToken},
    },
    source::transport::srt::{SrtEncryption, SrtKeyLength},
};

/// Configuration after all external values have been validated and translated.
pub struct ResolvedAppConfig {
    pub node: NodeConfig,
    pub authenticator: Arc<dyn Authenticator>,
    /// `None` unless `[hooks.endpoints]` names at least one destination.
    pub hooks: Option<ResolvedHooks>,
    /// Settings that are legal but probably not what was meant.
    ///
    /// Returned rather than printed: configuration is read before a `Node`
    /// exists, so there is no observer yet, and the caller owns its output.
    pub warnings: Vec<String>,
}

/// Hooks and the client they deliver with, which carries their own deadline.
pub struct ResolvedHooks {
    pub config: HooksConfig,
    pub client: HttpClient,
}

/// Failures while locating, reading, parsing, or resolving configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse configuration file {path}: {source}")]
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("could not read {secret} from {path}: {source}")]
    SecretRead {
        secret: String,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Sources(#[from] conf::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("invalid SRT encryption configuration: {0}")]
    SrtEncryption(crate::source::TransportError),
}

impl ConfigError {
    /// Prints a source-appropriate diagnostic and terminates with its intended
    /// exit status. In particular, `--help` and `--version` remain successful.
    pub fn exit(self) -> ! {
        match self {
            Self::Sources(error) => error.exit(),
            // The one place in the library that writes to stderr itself, and
            // deliberately: configuration is read before a `Node` exists, so
            // there is no observer to report through and nothing downstream
            // that could route this anywhere. Everything after start-up goes
            // through `Events`.
            error => {
                eprintln!("error: {error}");
                std::process::exit(2);
            }
        }
    }
}

/// The supported administrator-facing configuration.
///
/// Values resolve in the order `defaults < TOML < environment < CLI`.
#[derive(Conf)]
#[conf(serde, name = "rushls", version_fn = crate::version, env_prefix = "RUSHLS_")]
pub struct AppConfig {
    /// TOML configuration file to load.
    #[conf(parameter, long, env = "CONFIG", serde(skip))]
    pub config: Option<PathBuf>,

    #[conf(flatten, prefix)]
    pub server: ServerAppConfig,
    #[conf(flatten, prefix)]
    pub auth: AuthAppConfig,
    #[conf(flatten, prefix)]
    pub ingest: IngestAppConfig,
    #[conf(flatten, prefix)]
    pub hls: HlsAppConfig,
    #[conf(flatten, prefix)]
    pub storage: StorageAppConfig,
    #[conf(flatten, prefix)]
    pub http: HttpAppConfig,
    #[conf(flatten, prefix)]
    pub metrics: MetricsAppConfig,
    #[conf(flatten, prefix)]
    pub hooks: HooksAppConfig,
}

impl AppConfig {
    /// Loads process arguments, environment, and an optional TOML document.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(std::env::args_os(), std::env::vars_os())
    }

    /// Loads from explicit source snapshots, keeping configuration tests free
    /// from process-global environment mutation.
    pub fn load_from(
        args: impl IntoIterator<Item = OsString>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, ConfigError> {
        let args: Vec<OsString> = args.into_iter().collect();
        let env: Vec<(OsString, OsString)> = env.into_iter().collect();
        let path = find_parameter("config", args.clone())
            .map(PathBuf::from)
            .or_else(|| env_value(&env, "RUSHLS_CONFIG").map(PathBuf::from));
        let builder = Self::conf_builder().args(args).env(env);

        match path {
            Some(path) => {
                let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                    path: path.clone(),
                    source,
                })?;
                let document =
                    toml::from_str::<toml::Value>(&text).map_err(|source| ConfigError::Toml {
                        path: path.clone(),
                        source,
                    })?;
                builder
                    .doc(path.display().to_string(), document)
                    .try_parse()
                    .map_err(Into::into)
            }
            None => builder.try_parse().map_err(Into::into),
        }
    }

    /// Applies supported operator choices to independently evolving runtime
    /// defaults.
    pub fn resolve(self) -> Result<ResolvedAppConfig, ConfigError> {
        let defaults = NodeConfig::default();
        let mut client = LazyHttpClient::default();
        let mut warnings = Vec::new();
        let part_duration = self.hls.part_duration;
        let authenticator = self.auth.resolve(
            defaults.session.maximum_admission_time,
            part_duration,
            &mut client,
            &mut warnings,
        )?;
        let mut node = NodeConfig {
            maximum_sessions: self.server.maximum_concurrent_publishers,
            maximum_pending_publishers_per_listener: self
                .server
                .maximum_pending_publishers_per_listener,
            rtmp_address: self.ingest.rtmp.listen,
            srt_address: self.ingest.srt.listen,
            http_address: self.http.listen,
            ..NodeConfig::default()
        };
        self.ingest.rtmp.apply(&mut node, &mut warnings)?;
        self.ingest.srt.apply(&mut node)?;
        self.hls.apply(&mut node)?;
        self.storage.apply(&mut node)?;
        node.http = self.http.resolve()?;
        node.metrics = self.metrics.resolve()?;
        let hooks = self.hooks.resolve(&mut client)?;

        Ok(ResolvedAppConfig {
            node,
            authenticator,
            hooks,
            warnings,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct ServerAppConfig {
    /// Maximum publishers that may be active at the same time.
    #[conf(parameter, long, env, default_value = "256")]
    pub maximum_concurrent_publishers: usize,
    /// Maximum publishers each ingest listener may be authenticating at once.
    #[conf(parameter, long, env, default_value = "64")]
    pub maximum_pending_publishers_per_listener: usize,
}

#[derive(Conf)]
#[conf(serde)]
pub struct AuthAppConfig {
    /// Authentication implementation used for publisher admission.
    #[conf(parameter, long, env, default_value = "open", serde(use_value_parser))]
    provider: AuthProviderValue,
    /// Named policy profiles selected by configured publishers.
    #[conf(parameter, value_parser = TomlTable::<PolicyAppConfig>::from_str)]
    policies: Option<TomlTable<PolicyAppConfig>>,
    #[conf(flatten, prefix = "static", serde(rename = "static"))]
    static_provider: Option<StaticAuthAppConfig>,
    #[conf(flatten, prefix = "open", serde(rename = "open"))]
    open: Option<OpenAuthAppConfig>,
    #[conf(flatten, prefix = "http", serde(rename = "http"))]
    http: Option<HttpAuthAppConfig>,
}

impl AuthAppConfig {
    fn resolve(
        self,
        admission_deadline: Duration,
        part_duration: Duration,
        client: &mut LazyHttpClient,
        warnings: &mut Vec<String>,
    ) -> Result<Arc<dyn Authenticator>, ConfigError> {
        let mut profiles = self.policies.unwrap_or_default();
        profiles.0.entry("default".into()).or_default();
        let policies = resolve_policies(profiles)?;
        warn_about_tight_pacing(&policies, part_duration, warnings);

        match self.provider {
            AuthProviderValue::Static => {
                unselected(self.open.is_some(), "static", "open")?;
                unselected(self.http.is_some(), "static", "http")?;
                self.static_provider
                    .ok_or_else(|| {
                        invalid("auth provider `static` requires `[auth.static.publishers]`")
                    })?
                    .resolve(&policies)
                    .map(|authenticator| Arc::new(authenticator) as Arc<dyn Authenticator>)
            }
            AuthProviderValue::Open => {
                unselected(self.static_provider.is_some(), "open", "static")?;
                unselected(self.http.is_some(), "open", "http")?;
                let policy_name = self
                    .open
                    .and_then(|provider| provider.policy)
                    .unwrap_or_else(|| "default".into());
                let policy = policies.get(&policy_name).ok_or_else(|| {
                    invalid(format!(
                        "open auth provider selects unknown policy `{policy_name}`"
                    ))
                })?;
                Ok(Arc::new(OpenStreamAuthenticator::new(policy.clone())))
            }
            AuthProviderValue::Http => {
                unselected(self.static_provider.is_some(), "http", "static")?;
                unselected(self.open.is_some(), "http", "open")?;
                self.http
                    .ok_or_else(|| invalid("auth provider `http` requires `[auth.http]`"))?
                    .resolve(policies, admission_deadline, client)
                    .map(|authenticator| Arc::new(authenticator) as Arc<dyn Authenticator>)
            }
        }
    }
}

/// Refuses a provider table that is configured but not selected.
///
/// A table nobody reads is a typo, and reporting it is free.
fn unselected(present: bool, selected: &str, other: &str) -> Result<(), ConfigError> {
    if present {
        return Err(invalid(format!(
            "auth provider `{selected}` cannot be combined with `[auth.{other}]`"
        )));
    }
    Ok(())
}

#[derive(Conf)]
#[conf(serde)]
pub struct HttpAuthAppConfig {
    /// Service asked to admit each publisher.
    #[conf(parameter, long, env)]
    url: String,
    /// Deadline for the whole call, connection included.
    #[conf(
        parameter,
        long,
        env,
        default_value = "2s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    request_timeout: Duration,
    /// Largest decision this node will read.
    #[conf(parameter, long, env, default_value = "64KiB", serde(use_value_parser))]
    maximum_response_bytes: ByteSize,
    /// Policy applied when an allowing response names none.
    #[conf(parameter, long, env, default_value = "default")]
    default_policy: String,
    /// Bearer credential presented to the service.
    #[conf(parameter, env, secret)]
    token: Option<String>,
    /// Reads the bearer credential from a mounted secret instead.
    #[conf(parameter, env)]
    token_file: Option<PathBuf>,
}

impl HttpAuthAppConfig {
    fn resolve(
        self,
        policies: BTreeMap<String, StreamPolicy>,
        admission_deadline: Duration,
        client: &mut LazyHttpClient,
    ) -> Result<HttpAuthenticator, ConfigError> {
        // A call allowed to outlive the admission deadline never gets to fail
        // on its own terms: the session times out first and reports a stage
        // rather than the service that did not answer.
        if self.request_timeout >= admission_deadline {
            return Err(invalid(format!(
                "the auth request timeout ({:?}) must be shorter than the admission deadline ({admission_deadline:?})",
                self.request_timeout
            )));
        }
        if !policies.contains_key(&self.default_policy) {
            return Err(invalid(format!(
                "the http auth provider selects unknown default policy `{}`",
                self.default_policy
            )));
        }
        let token = resolve_optional_text_secret(
            "the auth service token",
            self.token.as_ref(),
            self.token_file.as_ref(),
        )?;

        Ok(HttpAuthenticator::new(
            HttpAuthConfig {
                endpoint: Endpoint::parse(&self.url).map_err(|error| invalid(error.to_string()))?,
                default_policy: self.default_policy,
                policies,
                bearer: token
                    .map(|token| BearerToken::new(&token))
                    .transpose()
                    .map_err(|error| invalid(error.to_string()))?,
            },
            client.with_limits(
                self.request_timeout,
                nonzero_bytes(
                    "the maximum auth response size",
                    self.maximum_response_bytes,
                )?,
            )?,
        ))
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct HooksAppConfig {
    /// CloudEvents `source`, identifying this deployment to consumers.
    ///
    /// With the per-event id it identifies an occurrence, so it must be stable
    /// across restarts. Nodes may share one, in which case consumers see a
    /// single logical producer.
    #[conf(parameter, long, env, default_value = "urn:rushls:node")]
    source: String,
    /// How long a shutdown waits for queued events before abandoning them.
    #[conf(
        parameter,
        long,
        env,
        default_value = "5s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    drain_timeout: Duration,
    /// Deadline for one delivery attempt, connection included.
    #[conf(
        parameter,
        long,
        env,
        default_value = "5s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    request_timeout: Duration,
    /// Largest response this node will read from an endpoint.
    #[conf(parameter, long, env, default_value = "64KiB", serde(use_value_parser))]
    maximum_response_bytes: ByteSize,
    /// Destinations, keyed by a name that identifies them in logs and metrics.
    #[conf(parameter, value_parser = TomlTable::<HookEndpointAppConfig>::from_str)]
    endpoints: Option<TomlTable<HookEndpointAppConfig>>,
}

impl HooksAppConfig {
    fn resolve(self, client: &mut LazyHttpClient) -> Result<Option<ResolvedHooks>, ConfigError> {
        let endpoints = self.endpoints.unwrap_or_default();
        if endpoints.0.is_empty() {
            // Nothing configured, so nothing is built — including the outbound
            // client, which a node delivering no events should not pay for.
            return Ok(None);
        }

        let mut hooks = Vec::with_capacity(endpoints.0.len());
        for (name, endpoint) in endpoints.0 {
            hooks.push(endpoint.resolve(&name)?);
        }

        Ok(Some(ResolvedHooks {
            config: HooksConfig {
                source: self.source,
                drain_timeout: self.drain_timeout,
                hooks,
                ..HooksConfig::default()
            },
            // A hook may wait far longer than admission may, which is why the
            // limits are per-request rather than baked into a shared client.
            client: client.with_limits(
                self.request_timeout,
                nonzero_bytes(
                    "the maximum hook response size",
                    self.maximum_response_bytes,
                )?,
            )?,
        }))
    }
}

/// A TOML table of entries keyed by the name an operator chose.
///
/// `conf` hands a table-valued parameter over as its own TOML text rather than
/// as a parsed value, so each of these has to parse itself. Generic because the
/// three that exist — hook endpoints, static publishers, policy profiles —
/// differ in nothing but what they hold.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct TomlTable<T>(BTreeMap<String, T>);

// Hand-written rather than derived: an empty table is meaningful for every `T`,
// and deriving would demand `T: Default` for no reason.
impl<T> Default for TomlTable<T> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<T: serde::de::DeserializeOwned> FromStr for TomlTable<T> {
    type Err = toml::de::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        toml::from_str(value)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookEndpointAppConfig {
    /// Where deliveries are posted.
    url: String,
    /// Which events this endpoint receives.
    ///
    /// Required rather than defaulting to everything, so a consumer written
    /// today cannot be sent an event type added after it.
    events: Vec<String>,
    /// Events held for this endpoint before the oldest is dropped.
    #[serde(default = "default_queue_capacity")]
    queue_capacity: usize,
    /// Distinct streams delivered at once. One request per stream is the
    /// ordering rule, so this is also the concurrency.
    #[serde(default = "default_maximum_in_flight")]
    maximum_in_flight: usize,
    /// Attempts per event, the first included.
    #[serde(default = "default_maximum_attempts")]
    maximum_attempts: u32,
    /// Bearer credential presented to this endpoint.
    token: Option<String>,
    /// Reads the bearer credential from a mounted secret instead.
    token_file: Option<PathBuf>,
}

fn default_queue_capacity() -> usize {
    1_000
}

fn default_maximum_in_flight() -> usize {
    8
}

fn default_maximum_attempts() -> u32 {
    5
}

impl HookEndpointAppConfig {
    fn resolve(self, name: &str) -> Result<HookConfig, ConfigError> {
        if self.events.is_empty() {
            return Err(invalid(format!(
                "hook `{name}` subscribes to no events, so it would never be called"
            )));
        }
        let mut events = BTreeSet::new();
        for event in &self.events {
            events.insert(
                Kind::from_str(event)
                    .map_err(|error| invalid(format!("hook `{name}`: {error}")))?,
            );
        }
        for (label, value) in [
            ("queue_capacity", self.queue_capacity),
            ("maximum_in_flight", self.maximum_in_flight),
        ] {
            if value == 0 {
                return Err(invalid(format!("hook `{name}` sets {label} to zero")));
            }
        }
        if self.maximum_attempts == 0 {
            return Err(invalid(format!(
                "hook `{name}` sets maximum_attempts to zero, so nothing would be sent"
            )));
        }
        let token = resolve_optional_text_secret(
            &format!("the token for hook `{name}`"),
            self.token.as_ref(),
            self.token_file.as_ref(),
        )?;

        Ok(HookConfig {
            name: Arc::from(name),
            endpoint: Endpoint::parse(&self.url).map_err(|error| invalid(error.to_string()))?,
            events,
            queue_capacity: self.queue_capacity,
            maximum_in_flight: self.maximum_in_flight,
            maximum_attempts: self.maximum_attempts,
            bearer: token
                .map(|token| BearerToken::new(&token))
                .transpose()
                .map_err(|error| invalid(error.to_string()))?,
        })
    }
}

/// Builds at most one outbound client, and only if something needs it.
///
/// Reading the platform trust store and building a TLS configuration is real
/// work, and a node that calls nothing out should not pay for it — nor should
/// the tests covering those configurations. Callers ask for their own deadline
/// and response ceiling, which are per-request and so cost nothing to vary;
/// what they share is the connector underneath.
#[derive(Default)]
pub struct LazyHttpClient(Option<HttpClient>);

impl LazyHttpClient {
    /// A client with the caller's limits, over the shared connector.
    fn with_limits(
        &mut self,
        request_timeout: Duration,
        maximum_response_bytes: usize,
    ) -> Result<HttpClient, ConfigError> {
        let shared = if let Some(client) = &self.0 {
            client
        } else {
            let client = HttpClient::new(ClientConfig::default())
                .map_err(|error| invalid(error.to_string()))?;
            self.0.insert(client)
        };
        Ok(shared.with_limits(request_timeout, maximum_response_bytes))
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct OpenAuthAppConfig {
    /// Named policy applied to every unauthenticated publisher.
    #[conf(parameter, long, env)]
    policy: Option<String>,
}

#[derive(Conf)]
#[conf(serde)]
pub struct StaticAuthAppConfig {
    /// Statically authorized publishers, keyed by their stable principal name.
    #[conf(parameter, value_parser = TomlTable::<StaticPublisherAppConfig>::from_str)]
    publishers: Option<TomlTable<StaticPublisherAppConfig>>,
}

impl StaticAuthAppConfig {
    fn resolve(
        self,
        policies: &BTreeMap<String, StreamPolicy>,
    ) -> Result<StaticStreamAuthenticator, ConfigError> {
        let configured_publishers = self.publishers.unwrap_or_default().0;
        if configured_publishers.is_empty() {
            return Err(invalid(
                "auth provider `static` requires at least one publisher",
            ));
        }

        let mut credentials: Vec<Vec<u8>> = Vec::with_capacity(configured_publishers.len());
        let mut publishers = Vec::with_capacity(configured_publishers.len());
        for (name, configured) in configured_publishers {
            if name.is_empty() {
                return Err(invalid("static publisher name must not be empty"));
            }
            if configured.stream.is_empty() {
                return Err(invalid(format!(
                    "static publisher `{name}` has an empty stream"
                )));
            }

            let credential = configured.resolve_key(&name)?;
            if credential.is_empty() {
                return Err(invalid(format!(
                    "static publisher `{name}` has an empty key"
                )));
            }
            if credentials.iter().any(|existing| existing == &credential) {
                return Err(invalid(
                    "the same static publishing key is configured more than once",
                ));
            }
            credentials.push(credential.clone());

            let policy_name = configured.policy.as_deref().unwrap_or("default");
            let policy = policies.get(policy_name).ok_or_else(|| {
                invalid(format!(
                    "static publisher `{name}` selects unknown policy `{policy_name}`"
                ))
            })?;
            publishers.push(StaticPublisher::new(
                credential,
                PublishGrant {
                    stream_id: StreamId::new(configured.stream),
                    principal: Principal(name),
                    policy: policy.clone(),
                },
            ));
        }
        Ok(StaticStreamAuthenticator::new(publishers))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StaticPublisherAppConfig {
    /// Authoritative stream populated by this publisher.
    stream: String,
    /// Inline publishing key. Prefer `key_file` in managed deployments.
    key: Option<String>,
    /// File containing the publishing key.
    key_file: Option<PathBuf>,
    /// Named policy profile; omitted selects `default`.
    policy: Option<String>,
}

impl StaticPublisherAppConfig {
    fn resolve_key(&self, name: &str) -> Result<Vec<u8>, ConfigError> {
        match (&self.key, &self.key_file) {
            (Some(_), Some(_)) => Err(invalid(format!(
                "static publisher `{name}` must configure only one of `key` and `key_file`"
            ))),
            (None, None) => Err(invalid(format!(
                "static publisher `{name}` must configure one of `key` and `key_file`"
            ))),
            (Some(key), None) => Ok(key.as_bytes().to_vec()),
            (None, Some(path)) => fs::read(path).map_err(|source| ConfigError::SecretRead {
                secret: format!("static publisher `{name}` key"),
                path: path.clone(),
                source,
            }),
        }
    }
}

/// Turns each configured profile into the policy publishers select by name.
fn resolve_policies(
    profiles: TomlTable<PolicyAppConfig>,
) -> Result<BTreeMap<String, StreamPolicy>, ConfigError> {
    profiles
        .0
        .into_iter()
        .map(|(name, configured)| {
            if name.is_empty() {
                return Err(invalid("auth policy name must not be empty"));
            }
            configured.resolve(&name).map(|policy| (name, policy))
        })
        .collect()
}

/// What happens when a publisher's media runs ahead of wall clock.
///
/// Named for the condition rather than the mechanism, because that is the
/// question an operator is answering: a file pushed at full speed and an
/// encoder catching up after a stall both look like this, and only the
/// operator knows which of the two their publishers are.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum FasterThanRealtimeValue {
    /// Sleep the publisher until wall clock catches up.
    Pace,
    /// Refuse the publication instead.
    Reject,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyAppConfig {
    takeovers: Option<TakeoversValue>,
    faster_than_realtime: Option<FasterThanRealtimeValue>,
    maximum_lead: Option<String>,
    maximum_timestamp_jump: Option<String>,
    video_codecs: Option<Vec<CodecValue>>,
    audio_codecs: Option<Vec<CodecValue>>,
    subtitle_codecs: Option<Vec<CodecValue>>,
    maximum_video_tracks: Option<usize>,
    maximum_audio_tracks: Option<usize>,
    maximum_subtitle_tracks: Option<usize>,
    maximum_video_resolution: Option<String>,
    maximum_video_frame_rate: Option<String>,
}

impl PolicyAppConfig {
    /// Builds the timing rule from one threshold and one choice of response.
    ///
    /// Both modes tolerate the same thing — media running ahead of wall clock
    /// — so `maximum_lead` means one thing regardless of which is selected,
    /// and only the consequence of exceeding it changes.
    fn resolve_timing(
        &self,
        name: &str,
        current: IngestTimingPolicy,
    ) -> Result<IngestTimingPolicy, ConfigError> {
        let inherited = match current {
            IngestTimingPolicy::RequireRealtime { maximum_lead }
            | IngestTimingPolicy::PaceToRealtime { maximum_lead, .. } => maximum_lead,
        };
        let maximum_lead = match &self.maximum_lead {
            Some(value) => duration(name, "maximum_lead", value)?,
            None => inherited,
        };
        if maximum_lead.is_zero() {
            return Err(invalid(format!(
                "auth policy `{name}`: maximum_lead must be nonzero; a \
                 publisher cannot be required to never run ahead at all"
            )));
        }

        let mode = self.faster_than_realtime.unwrap_or(match current {
            IngestTimingPolicy::PaceToRealtime { .. } => FasterThanRealtimeValue::Pace,
            IngestTimingPolicy::RequireRealtime { .. } => FasterThanRealtimeValue::Reject,
        });

        match mode {
            FasterThanRealtimeValue::Reject => {
                // A jump limit only makes sense where the response is to wait.
                // Rejecting already catches a broken timeline through the lead
                // itself, so this key would silently do nothing.
                if self.maximum_timestamp_jump.is_some() {
                    return Err(invalid(format!(
                        "auth policy `{name}`: maximum_timestamp_jump applies \
                         only to `faster_than_realtime = \"pace\"`, because \
                         rejecting already catches a forward jump through \
                         maximum_lead"
                    )));
                }
                Ok(IngestTimingPolicy::RequireRealtime { maximum_lead })
            }
            FasterThanRealtimeValue::Pace => {
                let inherited_jump = match current {
                    IngestTimingPolicy::PaceToRealtime {
                        maximum_timestamp_jump,
                        ..
                    } => maximum_timestamp_jump,
                    IngestTimingPolicy::RequireRealtime { .. } => default_maximum_timestamp_jump(),
                };
                let maximum_timestamp_jump = match &self.maximum_timestamp_jump {
                    Some(value) => duration(name, "maximum_timestamp_jump", value)?,
                    None => inherited_jump,
                };
                if maximum_timestamp_jump <= maximum_lead {
                    return Err(invalid(format!(
                        "auth policy `{name}`: maximum_timestamp_jump must \
                         exceed maximum_lead, or every tolerated lead would \
                         also be a broken timeline"
                    )));
                }
                Ok(IngestTimingPolicy::PaceToRealtime {
                    maximum_lead,
                    maximum_timestamp_jump,
                })
            }
        }
    }

    fn resolve(self, name: &str) -> Result<StreamPolicy, ConfigError> {
        let mut policy = StreamPolicy::permissive();
        if let Some(takeovers) = self.takeovers {
            policy.takeovers = takeovers.into();
        }
        policy.ingest_timing = self.resolve_timing(name, policy.ingest_timing)?;
        if let Some(codecs) = self.video_codecs {
            policy.accepted_video_codecs = validate_codecs(
                name,
                "video codecs",
                codecs,
                &[Codec::H264, Codec::Hevc, Codec::Av1],
            )?;
        }
        if let Some(codecs) = self.audio_codecs {
            policy.accepted_audio_codecs =
                validate_codecs(name, "audio codecs", codecs, &[Codec::Aac, Codec::Opus])?;
        }
        if let Some(codecs) = self.subtitle_codecs {
            policy.accepted_subtitle_codecs = validate_codecs(
                name,
                "subtitle codecs",
                codecs,
                &[Codec::WebVtt, Codec::SubRip, Codec::Text],
            )?;
        }
        if let Some(maximum) = self.maximum_video_tracks {
            policy.maximum_video_tracks = maximum;
        }
        if let Some(maximum) = self.maximum_audio_tracks {
            policy.maximum_audio_tracks = maximum;
        }
        if let Some(maximum) = self.maximum_subtitle_tracks {
            policy.maximum_subtitle_tracks = maximum;
        }
        if let Some(resolution) = self.maximum_video_resolution {
            let resolution = resolution
                .parse::<ResolutionValue>()
                .map_err(|error| invalid(format!("auth policy `{name}`: {error}")))?;
            policy.maximum_video_width = resolution.width;
            policy.maximum_video_height = resolution.height;
        }
        if let Some(frame_rate) = self.maximum_video_frame_rate {
            policy.maximum_video_frame_rate = frame_rate
                .parse::<FrameRateValue>()
                .map_err(|error| invalid(format!("auth policy `{name}`: {error}")))?
                .0;
        }
        Ok(policy)
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct IngestAppConfig {
    #[conf(flatten, prefix)]
    pub rtmp: RtmpAppConfig,
    #[conf(flatten, prefix)]
    pub srt: SrtAppConfig,
}

#[derive(Conf)]
#[conf(serde)]
pub struct RtmpAppConfig {
    /// Address receiving RTMP publishers.
    #[conf(parameter, long, env, default_value = "0.0.0.0:1935")]
    pub listen: SocketAddr,
    /// How long an RTMP peer may produce nothing before its session is closed.
    ///
    /// One value for connection liveness, which is what most operators want to
    /// think about: a peer that connects and then stalls costs a socket until
    /// something reclaims it. The per-phase settings below inherit this, and
    /// each may be overridden on its own.
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = parse_optional_duration,
        serde(use_value_parser)
    )]
    peer_timeout: OptionalDuration,
    /// Maximum time allowed for each handshake read.
    ///
    /// This one is worth tightening below `peer_timeout`: the peer is
    /// unauthenticated here, so a stalled handshake is the cheapest way to
    /// hold a socket open.
    #[conf(
        parameter,
        long,
        env,
        value_parser = parse_optional_duration,
        serde(use_value_parser)
    )]
    handshake_read_timeout: Option<OptionalDuration>,
    /// Maximum time allowed for each established-session read.
    ///
    /// Worth keeping generous: an established publisher on a poor network is
    /// still a publisher, and this is where an over-tight timeout turns
    /// hardening into an outage.
    #[conf(
        parameter,
        long,
        env,
        value_parser = parse_optional_duration,
        serde(use_value_parser)
    )]
    session_read_timeout: Option<OptionalDuration>,
    /// Maximum time allowed for each socket write.
    #[conf(
        parameter,
        long,
        env,
        value_parser = parse_optional_duration,
        serde(use_value_parser)
    )]
    write_timeout: Option<OptionalDuration>,
}

impl RtmpAppConfig {
    /// Resolve the per-phase timeouts, applying `peer_timeout` where a phase
    /// names no value of its own.
    fn timeouts(&self) -> ServerSessionTimeouts {
        ServerSessionTimeouts {
            handshake_read: self.handshake_read_timeout.unwrap_or(self.peer_timeout).0,
            session_read: self.session_read_timeout.unwrap_or(self.peer_timeout).0,
            write: self.write_timeout.unwrap_or(self.peer_timeout).0,
        }
    }

    fn apply(
        &self,
        node: &mut NodeConfig,
        warnings: &mut Vec<String>,
    ) -> Result<(), ConfigError> {
        let timeouts = self.timeouts();

        // A handshake read that outlives an established read inverts the
        // intent: the unauthenticated phase would be the more patient one.
        if let (Some(handshake), Some(session)) = (timeouts.handshake_read, timeouts.session_read)
            && handshake > session
        {
            return Err(ConfigError::Invalid(format!(
                "the RTMP handshake read timeout ({handshake:?}) must not exceed the \
                 established-session read timeout ({session:?}): the unauthenticated \
                 phase should be the stricter one"
            )));
        }

        // A session read shorter than a keyframe interval drops publishers
        // mid-GOP. Nothing here knows the publisher's cadence, so this only
        // catches values too small to be deliberate.
        if let Some(session) = timeouts.session_read
            && session < Duration::from_secs(1)
        {
            return Err(ConfigError::Invalid(format!(
                "the RTMP established-session read timeout ({session:?}) is below one \
                 second, which drops publishers between ordinary keyframes"
            )));
        }

        node.rtmp.timeouts = timeouts;

        // Disabling a timeout is legitimate on a trusted link and a liability
        // on a public one, and nothing here can tell which this is. Say so
        // rather than let an unbounded wait be invisible.
        for (name, value) in [
            ("handshake_read_timeout", timeouts.handshake_read),
            ("session_read_timeout", timeouts.session_read),
            ("write_timeout", timeouts.write),
        ] {
            if value.is_none() {
                warnings.push(format!(
                    "RTMP {name} is disabled: a peer that stops responding holds its \
                     connection until it is closed from the other end"
                ));
            }
        }

        Ok(())
    }
}

/// A duration that may be explicitly disabled.
///
/// "No timeout" has to stay expressible: a trusted link on a controlled
/// network is a legitimate reason to wait indefinitely, and without `"off"` an
/// operator who wants that is pushed into writing an absurd number instead —
/// which reads as a mistake and behaves like one if it is ever reached.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OptionalDuration(pub Option<Duration>);

impl std::fmt::Display for OptionalDuration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(duration) => write!(formatter, "{}", humantime::format_duration(duration)),
            None => formatter.write_str("off"),
        }
    }
}

fn parse_optional_duration(value: &str) -> Result<OptionalDuration, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("off") || trimmed.eq_ignore_ascii_case("none") {
        return Ok(OptionalDuration(None));
    }
    humantime::parse_duration(trimmed)
        .map(|duration| OptionalDuration(Some(duration)))
        .map_err(|error| error.to_string())
}

#[derive(Conf)]
#[conf(serde)]
pub struct SrtAppConfig {
    /// Address receiving SRT publishers.
    #[conf(parameter, long, env, default_value = "[::]:9000")]
    pub listen: SocketAddr,
    /// SRT receive latency; increase for unstable or long-distance networks.
    #[conf(
        parameter,
        long,
        env,
        default_value = "120ms",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    latency: Duration,
    /// Optional passphrase; absent accepts unencrypted SRT.
    #[conf(parameter, env, secret)]
    passphrase: Option<String>,
    /// File containing the optional SRT passphrase.
    #[conf(parameter, long, env)]
    passphrase_file: Option<PathBuf>,
    /// Encryption strength used when a passphrase is configured.
    #[conf(
        parameter,
        long,
        env,
        default_value = "aes256",
        serde(use_value_parser)
    )]
    encryption_key_length: SrtKeyLengthValue,
}

impl SrtAppConfig {
    fn apply(&self, node: &mut NodeConfig) -> Result<(), ConfigError> {
        if self.latency.is_zero() {
            return Err(invalid("SRT latency must be nonzero"));
        }
        let passphrase = resolve_optional_text_secret(
            "SRT passphrase",
            self.passphrase.as_ref(),
            self.passphrase_file.as_ref(),
        )?;
        node.srt.latency = self.latency;
        node.srt.encryption = passphrase
            .as_ref()
            .map(|passphrase| {
                SrtEncryption::new(passphrase.clone(), self.encryption_key_length.into())
            })
            .transpose()
            .map_err(ConfigError::SrtEncryption)?;
        Ok(())
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct HlsAppConfig {
    /// Absolute URL prefix for names emitted by playlists; empty is relative.
    #[conf(parameter, long, env, default_value = "")]
    public_base_url: String,
    /// Desired HLS segment duration. Keyframe cadence may adjust the result.
    #[conf(
        parameter,
        long,
        env,
        default_value = "6s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    segment_duration: Duration,
    /// Desired low-latency HLS partial-segment duration.
    #[conf(
        parameter,
        long,
        env,
        default_value = "1s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    part_duration: Duration,
    /// Minimum completed media retained in each live playlist.
    ///
    /// A fixed duration (`"18s"`) or a multiple of the segment duration
    /// (`"6x"`), which adapts to `segment_duration`. The media playlist must
    /// never drop below three times the target duration
    /// (draft-pantos-hls-rfc8216bis-22, section 6.2.1), so a fixed window
    /// shorter than that is refused.
    #[conf(
        parameter,
        long,
        env,
        default_value = "6x",
        value_parser = parse_playlist_window,
        serde(use_value_parser)
    )]
    playlist_window: DurationRule,
}

impl HlsAppConfig {
    fn apply(&self, node: &mut NodeConfig) -> Result<(), ConfigError> {
        if self.segment_duration.is_zero() || self.part_duration.is_zero() {
            return Err(invalid("HLS segment and part durations must be nonzero"));
        }
        if self.part_duration > self.segment_duration {
            return Err(invalid(
                "HLS part duration must not exceed the segment duration",
            ));
        }
        let window = self.playlist_window;
        let window_duration = window.resolve(self.segment_duration);
        if window_duration.is_zero() {
            return Err(invalid("HLS playlist window must be nonzero"));
        }
        if let DurationRule::Fixed(fixed) = window
            && fixed < self.segment_duration.saturating_mul(3)
        {
            return Err(invalid(
                "HLS playlist window must be at least three times the segment duration",
            ));
        }
        node.session.segmentation =
            SegmentationPolicy::latency_first(self.segment_duration, self.part_duration);
        // The store keeps a tag-count floor beside the duration floor; the
        // count is derived from the same window so the knob means one thing,
        // whatever cadence a rendition actually locks.
        let minimum_segments = window_duration
            .as_nanos()
            .div_ceil(self.segment_duration.as_nanos().max(1));
        node.store.retention.minimum_playlist_segments =
            usize::try_from(minimum_segments.max(1)).unwrap_or(usize::MAX);
        node.store.retention.minimum_playlist_duration = window;
        node.hls.uri_base = UriBase::new(self.public_base_url.clone());
        Ok(())
    }
}

/// Parses a playlist window: a multiple of the segment duration (`"6x"`) or
/// a fixed duration (`"18s"`).
///
/// The `x` suffix is the multiple form, so the two can never be confused
/// with each other or with a bare count.
fn parse_playlist_window(value: &str) -> Result<DurationRule, String> {
    if let Some(multiple) = value.strip_suffix('x') {
        let (numerator, denominator) = decimal_fraction(multiple)?;
        if numerator == 0 {
            return Err("a playlist window multiple must be nonzero".into());
        }
        let denominator = NonZeroU32::new(denominator)
            .ok_or_else(|| "a playlist window multiple must be nonzero".to_owned())?;
        return Ok(DurationRule::MultipleOfTarget(TargetDurationMultiple::new(
            numerator,
            denominator,
        )));
    }
    humantime::parse_duration(value)
        .map(DurationRule::Fixed)
        .map_err(|error| format!("invalid playlist window: {error}"))
}

/// Parses a decimal into a reduced fraction, so `"1.5"` becomes `3/2`.
///
/// Bounded to nine fractional digits so the denominator always fits a `u32`.
fn decimal_fraction(value: &str) -> Result<(u32, u32), String> {
    let (whole, fraction) = value.split_once('.').map_or((value, ""), |parts| parts);
    let fraction_digits = fraction.len();
    if whole.is_empty()
        || fraction_digits > 9
        || whole.bytes().any(|byte| !byte.is_ascii_digit())
        || fraction.bytes().any(|byte| !byte.is_ascii_digit())
    {
        return Err(format!("`{value}` is not a decimal number"));
    }
    let whole: u32 = whole
        .parse()
        .map_err(|_| format!("`{value}` is too large"))?;
    let fraction: u32 = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse()
            .map_err(|_| format!("`{value}` is too large"))?
    };
    let denominator = 10_u32
        .checked_pow(u32::try_from(fraction_digits).unwrap_or(u32::MAX))
        .ok_or_else(|| format!("`{value}` is too large"))?;
    let numerator = whole
        .checked_mul(denominator)
        .and_then(|scaled| scaled.checked_add(fraction))
        .ok_or_else(|| format!("`{value}` is too large"))?;
    let divisor = greatest_common_divisor(numerator, denominator);
    Ok((numerator / divisor, denominator / divisor))
}

fn greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

#[derive(Conf)]
#[conf(serde)]
pub struct StorageAppConfig {
    /// Maximum published or recently inactive streams retained by the process.
    #[conf(parameter, long, env, default_value = "1024")]
    maximum_streams: usize,
    /// Time an inactive stream remains available for a publisher to reconnect.
    #[conf(
        parameter,
        long,
        env,
        default_value = "30s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    inactive_stream_retention: Duration,
    /// Maximum retained media payload for one stream.
    #[conf(
        parameter,
        long,
        env,
        default_value = "512MiB",
        serde(use_value_parser)
    )]
    maximum_media_per_stream: ByteSize,
}

impl StorageAppConfig {
    fn apply(&self, node: &mut NodeConfig) -> Result<(), ConfigError> {
        if self.maximum_streams == 0 {
            return Err(invalid("storage must allow at least one stream"));
        }
        node.store.maximum_streams = self.maximum_streams;
        node.store.idle_retention = self.inactive_stream_retention;
        node.store.retention.maximum_payload_bytes = nonzero_bytes(
            "maximum media retained per stream",
            self.maximum_media_per_stream,
        )?;
        Ok(())
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct HttpAppConfig {
    /// Address serving HLS, health probes, and optional metrics.
    #[conf(parameter, long, env, default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,
    #[conf(flatten, prefix)]
    cors: CorsAppConfig,
    #[conf(flatten, prefix)]
    tls: Option<TlsAppConfig>,
}

impl HttpAppConfig {
    fn resolve(&self) -> Result<HttpConfig, ConfigError> {
        let config = HttpConfig {
            cors: self.cors.resolve()?,
            tls: self.tls.as_ref().map(TlsAppConfig::resolve),
        };
        config.validate().map_err(invalid)?;
        Ok(config)
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct CorsAppConfig {
    /// `*`, `off`, or a list of origins, each optionally starting with a
    /// wildcard label: `https://*.example.com` for one label, or
    /// `https://**.example.com` for any depth.
    #[conf(
        parameter,
        long,
        env,
        default(OriginsValue::any()),
        default_help_str = "*"
    )]
    origins: OriginsValue,
    /// Permit cookies or browser authorization on cross-origin requests.
    #[conf(parameter, long, env, default_value = "false")]
    allow_credentials: bool,
    /// How long browsers may cache a successful CORS preflight.
    #[conf(
        parameter,
        long,
        env,
        default_value = "10min",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    max_age: Duration,
}

impl CorsAppConfig {
    fn resolve(&self) -> Result<CorsConfig, ConfigError> {
        Ok(CorsConfig {
            allowed_origins: self.origins.resolve()?,
            allow_credentials: self.allow_credentials,
            max_age: self.max_age,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct TlsAppConfig {
    /// PEM certificate chain, leaf first.
    #[conf(parameter, long, env)]
    certificate: PathBuf,
    /// PEM private key.
    #[conf(parameter, long, env)]
    key: PathBuf,
}

impl TlsAppConfig {
    fn resolve(&self) -> TlsSettings {
        TlsSettings {
            certificate: self.certificate.clone(),
            key: self.key.clone(),
            ..TlsSettings::default()
        }
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct MetricsAppConfig {
    /// Expose Prometheus metrics at `/metrics` on the HTTP listener.
    #[conf(parameter, long, env, default_value = "false")]
    enabled: bool,
    /// Optional bearer token required to scrape `/metrics`.
    #[conf(parameter, env, secret)]
    token: Option<String>,
    /// File containing the optional metrics bearer token.
    #[conf(parameter, long, env)]
    token_file: Option<PathBuf>,
    /// Include per-stream series. Avoid this with unbounded stream names.
    #[conf(parameter, long, env, default_value = "false")]
    per_stream: bool,
}

impl MetricsAppConfig {
    fn resolve(self) -> Result<MetricsConfig, ConfigError> {
        let token = resolve_optional_text_secret(
            "metrics token",
            self.token.as_ref(),
            self.token_file.as_ref(),
        )?;
        if token.as_ref().is_some_and(String::is_empty) {
            return Err(invalid("metrics token must not be empty"));
        }
        Ok(MetricsConfig {
            enabled: self.enabled,
            token: token.map(MetricsToken::new),
            export: ExportPolicy {
                per_stream: self.per_stream,
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum OriginsValue {
    Text(String),
    List(Vec<String>),
}

impl OriginsValue {
    fn any() -> Self {
        Self::Text("*".into())
    }

    fn resolve(&self) -> Result<AllowedOrigins, ConfigError> {
        match self {
            Self::Text(value) if value == "*" => Ok(AllowedOrigins::Any),
            Self::Text(value) if value.eq_ignore_ascii_case("off") => Ok(AllowedOrigins::Disabled),
            Self::Text(value) => {
                let values: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|origin| !origin.is_empty())
                    .map(str::to_owned)
                    .collect();
                origins(&values)
            }
            Self::List(values) => origins(values),
        }
    }
}

impl FromStr for OriginsValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::Text(value.to_owned()))
    }
}

impl fmt::Display for OriginsValue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(value) => output.write_str(value),
            Self::List(values) => output.write_str(&values.join(",")),
        }
    }
}

#[derive(Clone, Copy, Debug, derive_more::Display)]
#[display(rename_all = "lowercase")]
enum AuthProviderValue {
    Static,
    Open,
    Http,
}

impl AuthProviderValue {
    const ALL: [Self; 3] = [Self::Static, Self::Open, Self::Http];
}

impl FromStr for AuthProviderValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Matched against the names `Display` produces, so the accepted set and
        // the printed one cannot drift, and the diagnostic lists what is
        // actually available rather than a fixed sentence someone has to
        // remember to update. The previous one still said "static or open"
        // long after `http` was added.
        one_of(&Self::ALL, value, "auth provider")
    }
}

/// Resolves a value against the spellings `Display` gives a fixed set of
/// variants, reporting the whole set when nothing matches.
fn one_of<T: Copy + fmt::Display>(all: &[T], value: &str, label: &str) -> Result<T, String> {
    all.iter()
        .find(|candidate| candidate.to_string() == value)
        .copied()
        .ok_or_else(|| {
            let names: Vec<String> = all.iter().map(ToString::to_string).collect();
            format!(
                "unknown {label} `{value}`; expected one of: {}",
                names.join(", ")
            )
        })
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TakeoversValue {
    Allow,
    Deny,
}

impl From<TakeoversValue> for TakeoverPolicy {
    fn from(value: TakeoversValue) -> Self {
        match value {
            TakeoversValue::Allow => Self::Allow,
            TakeoversValue::Deny => Self::Deny,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ResolutionValue {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl FromStr for ResolutionValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .split_once(['x', 'X'])
            .ok_or_else(|| "a resolution must be written as `WIDTHxHEIGHT`".to_owned())?;
        Ok(Self {
            width: parse_nonzero_u32("resolution width", width)?,
            height: parse_nonzero_u32("resolution height", height)?,
        })
    }
}

impl fmt::Display for ResolutionValue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{}x{}", self.width, self.height)
    }
}

#[derive(Clone, Copy, Debug)]
struct FrameRateValue(FrameRate);

impl FromStr for FrameRateValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (numerator, denominator) = value.split_once('/').unwrap_or((value, "1"));
        Ok(Self(FrameRate::new(
            parse_nonzero_u32("frame-rate numerator", numerator)?,
            parse_nonzero_u32("frame-rate denominator", denominator)?,
        )))
    }
}

impl fmt::Display for FrameRateValue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        let numerator = self.0.numerator();
        let denominator = self.0.denominator().get();
        if denominator == 1 {
            write!(output, "{numerator}")
        } else {
            write!(output, "{numerator}/{denominator}")
        }
    }
}

#[derive(Clone, Copy, Debug, derive_more::Display)]
#[display(rename_all = "lowercase")]
enum SrtKeyLengthValue {
    Aes128,
    Aes192,
    Aes256,
}

impl FromStr for SrtKeyLengthValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        one_of(
            &[Self::Aes128, Self::Aes192, Self::Aes256],
            value,
            "SRT key length",
        )
    }
}

impl From<SrtKeyLengthValue> for SrtKeyLength {
    fn from(value: SrtKeyLengthValue) -> Self {
        match value {
            SrtKeyLengthValue::Aes128 => Self::Aes128,
            SrtKeyLengthValue::Aes192 => Self::Aes192,
            SrtKeyLengthValue::Aes256 => Self::Aes256,
        }
    }
}

/// The codecs an operator may admit.
///
/// Deliberately narrower than [`Codec`]: `MovText` is something the pipeline
/// recognises on input rather than something anyone configures, and `Unknown`
/// is not a name at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
#[display(rename_all = "lowercase")]
enum CodecValue {
    Aac,
    Av1,
    H264,
    Hevc,
    Opus,
    SubRip,
    Text,
    WebVtt,
}

impl CodecValue {
    const ALL: [Self; 8] = [
        Self::Aac,
        Self::Av1,
        Self::H264,
        Self::Hevc,
        Self::Opus,
        Self::SubRip,
        Self::Text,
        Self::WebVtt,
    ];

    /// The spellings accepted besides the canonical one `Display` produces.
    ///
    /// Only aliases, because the canonical names now have one home. They used
    /// to have three — a `serde` attribute, a `FromStr` match and a `Display`
    /// match — so a name could be accepted in a TOML file and refused on the
    /// command line with neither place looking wrong.
    const ALIASES: [(&'static str, Self); 4] = [
        ("avc", Self::H264),
        ("h265", Self::Hevc),
        ("srt", Self::SubRip),
        ("vtt", Self::WebVtt),
    ];
}

impl FromStr for CodecValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.to_ascii_lowercase();
        match Self::ALIASES.iter().find(|(alias, _)| *alias == value) {
            Some((_, codec)) => Ok(*codec),
            None => one_of(&Self::ALL, &value, "codec"),
        }
    }
}

impl<'de> Deserialize<'de> for CodecValue {
    /// Delegates to [`FromStr`] so a TOML file and a command line accept
    /// exactly the same names.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl From<CodecValue> for Codec {
    fn from(value: CodecValue) -> Self {
        match value {
            CodecValue::Aac => Self::Aac,
            CodecValue::Av1 => Self::Av1,
            CodecValue::H264 => Self::H264,
            CodecValue::Hevc => Self::Hevc,
            CodecValue::Opus => Self::Opus,
            CodecValue::SubRip => Self::SubRip,
            CodecValue::Text => Self::Text,
            CodecValue::WebVtt => Self::WebVtt,
        }
    }
}

fn env_value<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_os_str())
}

/// Flags a pacing threshold tight enough to throttle a well-behaved publisher.
///
/// The pacer works on individual samples, so an encoder that hands over a whole
/// group of pictures at once is legitimately that far ahead the instant it does
/// — through no fault of its own. A lead below the node's own part duration is
/// therefore near-certainly too tight, and the symptom is not an error but
/// backpressure and rising latency, which reads as a network problem.
///
/// Only for `pace`. Under `reject` a tight lead is the entire point: that is
/// how an operator says a stream must be genuinely live.
fn warn_about_tight_pacing(
    policies: &BTreeMap<String, StreamPolicy>,
    part_duration: Duration,
    warnings: &mut Vec<String>,
) {
    for (name, policy) in policies {
        if let IngestTimingPolicy::PaceToRealtime { maximum_lead, .. } = policy.ingest_timing
            && maximum_lead < part_duration
        {
            warnings.push(format!(
                "auth policy `{name}` paces at a maximum_lead of {maximum_lead:?}, \
                 below the {part_duration:?} part duration; a publisher that \
                 emits a group of pictures at a time will be slowed even when \
                 it is running at realtime"
            ));
        }
    }
}

/// Parses one policy duration, naming the field an operator mistyped.
fn duration(policy: &str, field: &str, value: &str) -> Result<Duration, ConfigError> {
    humantime::parse_duration(value)
        .map_err(|error| invalid(format!("auth policy `{policy}`: {field} {error}")))
}

/// Used when a policy switches to pacing without naming a jump limit.
///
/// Read from the built-in permissive policy rather than written twice, so the
/// default cannot drift from the one the library ships.
fn default_maximum_timestamp_jump() -> Duration {
    match StreamPolicy::permissive().ingest_timing {
        IngestTimingPolicy::PaceToRealtime {
            maximum_timestamp_jump,
            ..
        } => maximum_timestamp_jump,
        IngestTimingPolicy::RequireRealtime { .. } => Duration::from_secs(10),
    }
}

fn resolve_optional_text_secret(
    label: &str,
    inline: Option<&String>,
    file: Option<&PathBuf>,
) -> Result<Option<String>, ConfigError> {
    match (inline, file) {
        (Some(_), Some(_)) => Err(invalid(format!(
            "{label} must configure only one of its inline value and `_file`"
        ))),
        (Some(value), None) => Ok(Some(value.clone())),
        (None, Some(path)) => {
            fs::read_to_string(path)
                .map(Some)
                .map_err(|source| ConfigError::SecretRead {
                    secret: label.to_owned(),
                    path: path.clone(),
                    source,
                })
        }
        (None, None) => Ok(None),
    }
}

fn nonzero_bytes(label: &str, value: ByteSize) -> Result<usize, ConfigError> {
    let value = usize::try_from(value.as_u64()).map_err(|_| {
        invalid(format!(
            "{label} does not fit this platform's address space"
        ))
    })?;
    if value == 0 {
        Err(invalid(format!("{label} must be nonzero")))
    } else {
        Ok(value)
    }
}

fn origins(values: &[String]) -> Result<AllowedOrigins, ConfigError> {
    if values.is_empty() {
        return Err(invalid("a CORS origin allowlist must not be empty"));
    }
    // The pattern grammar is checked here, at startup, rather than per
    // request: a malformed entry should stop the process, not quietly match
    // nothing for as long as nobody notices.
    let patterns = values
        .iter()
        .map(|value| OriginPattern::parse(value).map_err(|error| invalid(error.to_string())))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AllowedOrigins::Only(patterns))
}

fn validate_codecs(
    policy: &str,
    label: &str,
    configured: Vec<CodecValue>,
    permitted: &[Codec],
) -> Result<Vec<Codec>, ConfigError> {
    let configured: Vec<Codec> = configured.into_iter().map(Into::into).collect();
    if let Some(codec) = configured.iter().find(|codec| !permitted.contains(codec)) {
        return Err(invalid(format!(
            "auth policy `{policy}` {label} contains {codec:?}, which is not valid for that media kind"
        )));
    }
    Ok(configured)
}

fn parse_nonzero_u32(label: &str, value: &str) -> Result<NonZeroU32, String> {
    value
        .parse::<u32>()
        .map_err(|error| format!("{label} is not an unsigned integer: {error}"))
        .and_then(|value| NonZeroU32::new(value).ok_or_else(|| format!("{label} must be positive")))
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(message.into())
}

#[cfg(test)]
mod tests;
