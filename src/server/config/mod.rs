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
    num::{NonZeroU16, NonZeroU32},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use bytesize::ByteSize;
use cc_rtmp::ServerSessionTimeouts;
use cc_tls::{ClientIdentity, load_roots};
use conf::{Conf, find_parameter, introspection::ProgramOptionMeta};
use rustls::pki_types::CertificateDer;
use serde::Deserialize;
use thiserror::Error;

use crate::{
    admission::{
        Authenticator, Bounds, Ceiling, Codecs, Floor, FrameBox, HttpAuthConfig, HttpAuthenticator,
        OpenStreamAuthenticator, Pace, Resolution, StreamPolicy, TakeoverPolicy,
    },
    delivery::hls::uri::UriBase,
    delivery::store::{DiskLimits, DurationRule, TargetDurationMultiple},
    domain::{Codec, FrameRate},
    hooks::{HookConfig, HooksConfig},
    observe::lifecycle::Kind,
    outbound::{BearerToken, ClientConfig, Endpoint, HttpClient},
    segment::SegmentationPolicy,
    server::{
        NodeConfig,
        http::{
            AllowedOrigins, CorsConfig, HttpConfig, OriginPattern, TlsSettings,
            playback::{ClaimValue, PlaybackKeyMaterial, PlaybackSettings},
        },
        metrics::{MetricsConfig, MetricsToken},
    },
    source::transport::srt::{SrtEncryption, SrtKeyLength},
};

/// Configuration after all external values have been validated and translated.
pub struct ResolvedAppConfig {
    pub node: NodeConfig,
    pub authenticator: Arc<dyn Authenticator>,
    /// `None` unless `[hook.<name>]` names at least one destination.
    pub hooks: Option<ResolvedHooks>,
    /// Settings that are legal but probably not what was meant.
    ///
    /// Returned rather than printed: configuration is read before a `Node`
    /// exists, so there is no observer yet, and the caller owns its output.
    pub warnings: Vec<String>,
    /// Outbound mutual-TLS material, held for its rotation watches.
    ///
    /// Carried rather than dropped after building the clients: each entry owns
    /// a filesystem watch, and dropping it would leave the client presenting
    /// whatever certificate it started with until the process restarted --
    /// which is the failure the watch exists to prevent, and a silent one.
    pub outbound_tls: Vec<OutboundTls>,
    /// Viewer JWT settings, present when `[auth.playback]` is configured.
    ///
    /// The JWKS fetch, when that is the key source, happens when the gate is
    /// started rather than here: configuration resolve is synchronous and a
    /// URL being well-formed is a different question from the issuer being
    /// reachable.
    pub playback: Option<PlaybackSettings>,
    /// File this process actually loaded, if any.
    ///
    /// Chosen before values overlay. CLI `--config` and `RUSHLS_CONFIG` win;
    /// otherwise the first existing well-known path is used. `None` means
    /// compiled defaults plus environment and CLI flags.
    pub config_file: Option<PathBuf>,
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
    #[error("could not interpolate {path}: {source}")]
    Interpolation {
        path: PathBuf,
        source: interpolate::InterpolationError,
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
            // Configuration is resolved before a `Node` exists, so this cannot
            // use the observer. The binary initializes tracing before loading
            // configuration, which keeps this startup diagnostic timestamped.
            error => {
                tracing::error!(error = %error, "configuration error");
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

    #[conf(flatten, serde(flatten))]
    pub node: ServerAppConfig,
    #[conf(flatten, prefix)]
    pub auth: AuthAppConfig,
    #[conf(flatten, prefix)]
    pub rtmp: RtmpAppConfig,
    #[conf(flatten, prefix)]
    pub srt: SrtAppConfig,
    #[conf(flatten, prefix)]
    pub accept: AcceptAppConfig,
    #[conf(flatten, prefix)]
    pub hls: HlsAppConfig,
    #[conf(flatten, prefix)]
    pub capacity: CapacityAppConfig,
    #[conf(flatten, prefix)]
    pub http: HttpAppConfig,
    #[conf(flatten, prefix)]
    pub metrics: MetricsAppConfig,
    /// Local segment archive. Specified in the reference and **not built**.
    ///
    /// Present so the key is refused by name rather than silently ignored: a
    /// configuration that writes no files while an operator believes it does
    /// is the failure mode this design exists to remove.
    #[conf(parameter, value_parser = TomlValue::<toml::Value>::from_str)]
    pub record: Option<TomlValue<toml::Value>>,
    /// Hook destinations, keyed by the name that identifies each in logs and
    /// metrics.
    ///
    /// An open namespace, and one of only two: the sub-table names are the
    /// operator's own. A name is a key rather than a value -- it appears in
    /// logs and metrics and must be unique -- so a table keyed by it makes
    /// uniqueness structural, since TOML rejects a duplicate key. An array of
    /// tables with a `name` field would turn that into a validation rule that
    /// can be forgotten, and demote the name from the heading to a line inside
    /// the block. Unknown keys *within* one still refuse.
    #[conf(parameter, value_parser = TomlTable::<HookEndpointAppConfig>::from_str)]
    pub hook: Option<TomlTable<HookEndpointAppConfig>>,
}

impl AppConfig {
    /// Loads process arguments, environment, and a configuration file.
    ///
    /// File discovery walks `--config`, `RUSHLS_CONFIG`, then well-known
    /// locations. Tests that must not see a `rushls.toml` in the working
    /// directory use [`Self::load_from`].
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from_with(
            std::env::args_os(),
            std::env::vars_os(),
            paths::ConfigSearch::WellKnown,
        )
        .map(|(config, _)| config)
    }

    /// Loads and resolves process arguments, environment, and a configuration
    /// file from the live process snapshot, including well-known search paths.
    pub fn load_and_resolve() -> Result<ResolvedAppConfig, ConfigError> {
        Self::load_and_resolve_from_with(
            std::env::args_os(),
            std::env::vars_os(),
            paths::ConfigSearch::WellKnown,
        )
    }

    /// As [`Self::load_and_resolve`], over explicit source snapshots.
    ///
    /// Does not search well-known paths: a test that asked for compiled
    /// defaults must not pick up a `rushls.toml` sitting in the crate tree.
    pub fn load_and_resolve_from(
        args: impl IntoIterator<Item = OsString>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<ResolvedAppConfig, ConfigError> {
        Self::load_and_resolve_from_with(args, env, paths::ConfigSearch::ExplicitOnly)
    }

    fn load_and_resolve_from_with(
        args: impl IntoIterator<Item = OsString>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
        search: paths::ConfigSearch,
    ) -> Result<ResolvedAppConfig, ConfigError> {
        let env: Vec<(OsString, OsString)> = env.into_iter().collect();
        let (config, config_file) = Self::load_from_with(args, env.iter().cloned(), search)?;
        let mut resolved = config.resolve_from(env)?;
        resolved.config_file = config_file;
        Ok(resolved)
    }

    /// Loads from explicit source snapshots, keeping configuration tests free
    /// from process-global environment mutation and from well-known files.
    pub fn load_from(
        args: impl IntoIterator<Item = OsString>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, ConfigError> {
        Self::load_from_with(args, env, paths::ConfigSearch::ExplicitOnly).map(|(config, _)| config)
    }

    fn load_from_with(
        args: impl IntoIterator<Item = OsString>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
        search: paths::ConfigSearch,
    ) -> Result<(Self, Option<PathBuf>), ConfigError> {
        let args: Vec<OsString> = args.into_iter().collect();
        let env: Vec<(OsString, OsString)> = env.into_iter().collect();
        let path = explicit_config_path(&args, &env).or_else(|| match search {
            paths::ConfigSearch::ExplicitOnly => None,
            paths::ConfigSearch::WellKnown => {
                paths::first_existing_file(paths::well_known_config_paths())
            }
        });
        let builder = Self::conf_builder().args(args).env(env.clone());

        match path {
            Some(path) => {
                let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                    path: path.clone(),
                    source,
                })?;
                let mut document =
                    toml::from_str::<toml::Value>(&text).map_err(|source| ConfigError::Toml {
                        path: path.clone(),
                        source,
                    })?;
                // Before the settings layer sees the tree, so `${VAR}` in the
                // file and a `RUSHLS_` override compose rather than compete:
                // the reference is resolved here and an override still
                // replaces the result.
                interpolate::Environment::new(env)
                    .interpolate(&mut document)
                    .map_err(|source| ConfigError::Interpolation {
                        path: path.clone(),
                        source,
                    })?;
                let config = builder
                    .doc(path.display().to_string(), document)
                    .try_parse()
                    .map_err(ConfigError::from)?;
                Ok((config, Some(path)))
            }
            None => Ok((builder.try_parse().map_err(ConfigError::from)?, None)),
        }
    }

    /// Applies supported operator choices to independently evolving runtime
    /// defaults, over the process environment.
    pub fn resolve(self) -> Result<ResolvedAppConfig, ConfigError> {
        self.resolve_from(std::env::vars_os())
    }

    /// As [`Self::resolve`], over an explicit environment snapshot.
    ///
    /// Mirrors [`Self::load_from`]: the environment is inspected here so a
    /// configuration test exercises the same inputs the loader used, rather
    /// than whatever happens to be set in the test runner's shell.
    pub fn resolve_from(
        self,
        env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<ResolvedAppConfig, ConfigError> {
        let defaults = NodeConfig::default();
        let mut client = LazyHttpClient::default();
        let mut warnings = Vec::new();
        warnings.extend(Self::unrecognized_environment(env));

        let (default_policy, policies) = self.accept.resolve()?;
        let stall = self.accept.stall;
        let open_admission = self.auth.is_open();
        let mut outbound_tls = Vec::new();
        let AuthAppConfig { publish, playback } = self.auth;
        let authenticator = match publish {
            Some(publish) => publish
                .resolve(
                    default_policy,
                    policies,
                    defaults.session.maximum_admission_time,
                    &mut client,
                    &mut outbound_tls,
                )
                .map(|authenticator| Arc::new(authenticator) as Arc<dyn Authenticator>)?,
            None => Arc::new(OpenStreamAuthenticator::new(default_policy)),
        };
        let playback = playback
            .map(|playback| playback.resolve(&mut client))
            .transpose()?;

        let mut node = NodeConfig {
            maximum_sessions: self.capacity.publishers,
            shutdown: self.node.shutdown,
            rtmp_address: self.rtmp.listen,
            srt_address: self.srt.listen,
            http_address: self.http.listen.0,
            ..NodeConfig::default()
        };
        if let Some(name) = self
            .node
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            node.name = std::sync::Arc::from(name);
        }
        self.rtmp.apply(&mut node, &mut warnings)?;
        self.srt.apply(&mut node)?;
        self.hls.apply(&mut node, &mut warnings)?;
        // After HLS: a stall expressed as a multiple is sized by the segment
        // duration, which only `hls.apply` establishes.
        apply_stall(&mut node, stall, self.hls.segment_duration())?;
        self.capacity.apply(&mut node)?;
        node.hls.uri_base = UriBase::new(self.http.public_url.clone());
        node.http = self.http.resolve()?;
        node.https_address = node.http.tls_address;
        node.metrics = self.metrics.resolve()?;
        if node.http_address.is_none() && node.https_address.is_none() {
            return Err(invalid(
                "both listeners are off, so this node could serve nothing",
            ));
        }
        if self.record.is_some() {
            return Err(invalid(
                "[record] is specified in the reference configuration but not yet implemented; \
                 remove it rather than run a node that writes no archive",
            ));
        }
        let hooks = resolve_hooks(self.hook, &node, &mut client, &mut outbound_tls)?;
        warnings.extend(startup_warnings(&node, open_admission));

        Ok(ResolvedAppConfig {
            node,
            authenticator,
            playback,
            hooks,
            warnings,
            outbound_tls,
            config_file: None,
        })
    }

    /// `RUSHLS_`-prefixed environment variables that no option reads.
    ///
    /// An override that misses its name by a typo falls back to the compiled
    /// default silently, so the misspelling is named at startup rather than
    /// left to be discovered in the behavior it never controlled.
    fn unrecognized_environment(
        env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Vec<String> {
        let known: BTreeSet<String> = Self::program_options()
            .filter_map(|option| option.env_form().map(ToString::to_string))
            .collect();
        let mut unrecognized: Vec<String> = env
            .into_iter()
            .filter_map(|(key, _)| key.into_string().ok())
            .filter(|key| key.starts_with("RUSHLS_") && !known.contains(key))
            .map(|key| format!("unrecognized environment variable {key} is ignored"))
            .collect();
        // Environment iteration order is unspecified; startup warnings should
        // read the same way run to run.
        unrecognized.sort();
        unrecognized
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct ServerAppConfig {
    /// Identifies this node in logs, metrics, and hook events.
    ///
    /// Defaults to the hostname. Several nodes may share one deliberately, in
    /// which case consumers of hook events see a single logical producer.
    #[conf(parameter, long, env)]
    pub name: Option<String>,
    /// How long a restart waits for parked viewer requests and queued hook
    /// events before exiting anyway.
    ///
    /// Keep it below the grace period the scheduler allows, or a hard kill
    /// arrives mid-drain and the graceful path buys nothing. Deliberately has
    /// no "off": waiting indefinitely would mean staying alive to serve
    /// retained media, a wait bounded by `retain` that no scheduler grants.
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    pub shutdown: Duration,
}

#[derive(Conf)]
#[conf(serde)]
pub struct AuthAppConfig {
    /// Optional external admission service. When omitted, admission is open
    /// and every publisher gets the default accept set.
    #[conf(flatten, prefix = "publish", serde(rename = "publish"))]
    publish: Option<HttpAuthAppConfig>,
    /// Optional local JWT verification for viewers. When omitted, anyone with
    /// the URL may watch.
    #[conf(flatten, prefix = "playback", serde(rename = "playback"))]
    playback: Option<PlaybackAuthAppConfig>,
}

impl AuthAppConfig {
    /// Whether anyone who can reach an ingest listener may publish.
    fn is_open(&self) -> bool {
        self.publish.is_none()
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct PlaybackAuthAppConfig {
    #[conf(parameter, env, secret)]
    public_key: Option<String>,
    #[conf(parameter, long, env)]
    public_key_file: Option<PathBuf>,
    #[conf(parameter, long, env)]
    jwks_url: Option<String>,
    #[conf(parameter, env, secret)]
    secret: Option<String>,
    #[conf(parameter, long, env)]
    secret_file: Option<PathBuf>,
    #[conf(parameter, long, env, default_value = "stream")]
    stream_claim: String,
    #[conf(
        parameter,
        long,
        env,
        default_value = "30s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    leeway: Duration,
    #[conf(parameter, value_parser = TomlTable::<ClaimValue>::from_str)]
    claims: Option<TomlTable<ClaimValue>>,
}

impl PlaybackAuthAppConfig {
    fn resolve(self, client: &mut LazyHttpClient) -> Result<PlaybackSettings, ConfigError> {
        let public_key = resolve_optional_text_secret(
            "the playback public key",
            self.public_key.as_ref(),
            self.public_key_file.as_ref(),
        )?;
        let secret = resolve_optional_text_secret(
            "the playback secret",
            self.secret.as_ref(),
            self.secret_file.as_ref(),
        )?;
        let keys = match (public_key, self.jwks_url.as_deref(), secret) {
            (Some(pem), None, None) => PlaybackKeyMaterial::PublicPem(pem.into_bytes()),
            (None, Some(url), None) => PlaybackKeyMaterial::Jwks {
                endpoint: Endpoint::parse(url).map_err(|error| invalid(error.to_string()))?,
                client: Box::new(client.with_limits(Duration::from_secs(5), 64 * 1024)?),
            },
            (None, None, Some(secret)) => {
                if secret.is_empty() {
                    return Err(invalid("the playback secret must not be empty"));
                }
                PlaybackKeyMaterial::Secret(secret.into_bytes())
            }
            _ => {
                return Err(invalid(
                    "[auth.playback] must set exactly one of public_key, public_key_file, \
                     jwks_url, secret, or secret_file",
                ));
            }
        };

        let claims = self.claims.unwrap_or_default().0;
        let issuer = required_string_claim(&claims, "iss")?;
        let audience = required_string_claim(&claims, "aud")?;
        let mut extra = claims;
        extra.remove("iss");
        extra.remove("aud");

        if self.stream_claim.is_empty() {
            return Err(invalid("stream_claim must not be empty"));
        }

        Ok(PlaybackSettings {
            issuer,
            audience,
            extra,
            stream_claim: self.stream_claim,
            leeway: self.leeway,
            keys,
        })
    }
}

fn required_string_claim(
    claims: &BTreeMap<String, ClaimValue>,
    name: &str,
) -> Result<String, ConfigError> {
    match claims.get(name) {
        Some(ClaimValue::String(value)) if !value.is_empty() => Ok(value.clone()),
        Some(_) => Err(invalid(format!(
            "[auth.playback.claims] {name} must be a non-empty string"
        ))),
        None => Err(invalid(format!(
            "[auth.playback.claims] must include {name}"
        ))),
    }
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
    /// Bearer credential presented to the service.
    #[conf(parameter, env, secret)]
    token: Option<String>,
    /// File containing the bearer credential presented to the service.
    #[conf(parameter, long, env)]
    token_file: Option<PathBuf>,
    /// Path to a PEM certificate chain this node presents to the service.
    #[conf(parameter, long, env)]
    client_certificate: Option<PathBuf>,
    /// Path to the PEM private key for that chain.
    #[conf(parameter, long, env)]
    client_key: Option<PathBuf>,
    /// Path to a PEM authority to trust instead of the platform store.
    #[conf(parameter, long, env)]
    ca: Option<PathBuf>,
}

impl HttpAuthAppConfig {
    fn resolve(
        self,
        default: StreamPolicy,
        policies: BTreeMap<String, StreamPolicy>,
        admission_deadline: Duration,
        client: &mut LazyHttpClient,
        outbound_tls: &mut Vec<OutboundTls>,
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
        let token = resolve_optional_text_secret(
            "the auth service token",
            self.token.as_ref(),
            self.token_file.as_ref(),
        )?;

        Ok(HttpAuthenticator::new(
            HttpAuthConfig {
                endpoint: Endpoint::parse(&self.url).map_err(|error| invalid(error.to_string()))?,
                // `[accept]` itself is the unnamed default, so a response
                // naming no policy gets it. The reserved `default` policy
                // name is gone with the table that needed one.
                default,
                policies,
                bearer: token
                    .map(|token| BearerToken::new(&token))
                    .transpose()
                    .map_err(|error| invalid(error.to_string()))?,
            },
            {
                let limit = nonzero_bytes(
                    "the maximum auth response size",
                    self.maximum_response_bytes,
                )?;
                let tls = OutboundTlsAppConfig {
                    certificate: self.client_certificate.clone(),
                    key: self.client_key.clone(),
                    ca: self.ca.clone(),
                };
                if tls.is_configured() {
                    let tls = tls.resolve("[auth.publish]")?;
                    let built = tls.client(self.request_timeout, limit)?;
                    // The watch lives as long as the resolved configuration,
                    // because dropping it stops rotations being noticed.
                    outbound_tls.push(tls);
                    built
                } else {
                    client.with_limits(self.request_timeout, limit)?
                }
            },
        ))
    }
}

/// Deadline for one hook delivery attempt, connection included.
///
/// Compiled rather than configured: the reference deliberately exposes no
/// per-hook queue depth, retry count, or response ceiling, because none of
/// them is a question an operator can answer better than the node can.
const HOOK_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest response this node will read from a hook endpoint.
const HOOK_MAXIMUM_RESPONSE_BYTES: usize = 64 * 1024;

/// Builds the configured destinations, or nothing when none are named.
fn resolve_hooks(
    endpoints: Option<TomlTable<HookEndpointAppConfig>>,
    node: &NodeConfig,
    client: &mut LazyHttpClient,
    outbound_tls: &mut Vec<OutboundTls>,
) -> Result<Option<ResolvedHooks>, ConfigError> {
    let endpoints = endpoints.unwrap_or_default();
    if endpoints.0.is_empty() {
        // Nothing configured, so nothing is built — including the outbound
        // client, which a node delivering no events should not pay for.
        return Ok(None);
    }

    let mut hooks = Vec::with_capacity(endpoints.0.len());
    for (name, endpoint) in endpoints.0 {
        hooks.push(endpoint.resolve(&name, client, outbound_tls)?);
    }

    Ok(Some(ResolvedHooks {
        config: HooksConfig {
            // Both drains answer one operator question and share one budget;
            // the producer identity is the node name rather than a second
            // spelling of the same thing.
            drain_timeout: node.shutdown,
            hooks,
            ..HooksConfig::new(node.name.to_string())
        },
        // The pool every destination that configured no identity of its own
        // uses. A hook may wait far longer than admission may, which is why
        // the limits are per-request rather than baked into a shared client.
        client: client.with_limits(HOOK_REQUEST_TIMEOUT, HOOK_MAXIMUM_RESPONSE_BYTES)?,
    }))
}

/// A TOML table of entries keyed by the name an operator chose.
///
/// `conf` hands a table-valued parameter over as its own TOML text rather than
/// as a parsed value, so each of these has to parse itself. Generic because the
/// three that exist — hook endpoints, static publishers, policy profiles —
/// differ in nothing but what they hold.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct TomlTable<T>(BTreeMap<String, T>);

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
pub struct HookEndpointAppConfig {
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
    /// Path to a PEM certificate chain this node presents to this endpoint.
    client_certificate: Option<PathBuf>,
    /// Path to the PEM private key for that chain.
    client_key: Option<PathBuf>,
    /// Path to a PEM authority to trust instead of the platform store.
    ca: Option<PathBuf>,
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
    fn resolve(
        self,
        name: &str,
        client: &mut LazyHttpClient,
        outbound_tls: &mut Vec<OutboundTls>,
    ) -> Result<HookConfig, ConfigError> {
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

        // Only a destination that asked for an identity or a pinned authority
        // gets a client of its own; everything else shares the process pool.
        // The material is held in `outbound_tls` for the same reason
        // admission's is: dropping it stops rotations being noticed.
        let tls = OutboundTlsAppConfig {
            certificate: self.client_certificate.clone(),
            key: self.client_key.clone(),
            ca: self.ca.clone(),
        };
        let client = if tls.is_configured() {
            let tls = tls.resolve(&format!("hook `{name}`"))?;
            let built = tls.client(HOOK_REQUEST_TIMEOUT, HOOK_MAXIMUM_RESPONSE_BYTES)?;
            outbound_tls.push(tls);
            Some(built)
        } else {
            // Built now rather than at the first delivery, so a trust store
            // this node cannot read fails startup instead of an event.
            client.with_limits(HOOK_REQUEST_TIMEOUT, HOOK_MAXIMUM_RESPONSE_BYTES)?;
            None
        };

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
            client,
        })
    }
}

/// The mutual-TLS material one outbound destination presents and trusts.
///
/// Optional on every destination, and shared in shape between admission and
/// hooks: both call operator-run services over a network the operator may not
/// consider private, and the trust question is the same on each.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundTlsAppConfig {
    /// Path to a PEM certificate chain this node presents, leaf first.
    certificate: Option<PathBuf>,
    /// Path to the PEM private key for that chain.
    key: Option<PathBuf>,
    /// Path to a PEM authority to trust instead of the platform store.
    ///
    /// Pinning matters as much as presenting: a client certificate proves this
    /// node to the service, and pinning proves the service to this node. An
    /// admission service that may widen what this origin accepts should be
    /// authenticated in both directions.
    ca: Option<PathBuf>,
}

impl OutboundTlsAppConfig {
    /// Whether anything here needs a connector of its own.
    fn is_configured(&self) -> bool {
        self.certificate.is_some() || self.key.is_some() || self.ca.is_some()
    }

    /// Loads the identity and roots, starting a watch for rotations.
    fn resolve(&self, label: &str) -> Result<OutboundTls, ConfigError> {
        // Half a pair is a misconfiguration rather than a partial identity:
        // presenting a certificate needs its key, and a key alone proves
        // nothing. Refused rather than ignored, because a node that silently
        // presented no identity would be rejected by the service later, with
        // nothing here to explain why.
        let identity = match (&self.certificate, &self.key) {
            (Some(certificate), Some(key)) => Some(
                ClientIdentity::new(
                    certificate.clone(),
                    key.clone(),
                    Arc::new(cc_tls::IgnoreTlsEvents),
                )
                .map_err(|error| invalid(format!("{label}: {error}")))?,
            ),
            (None, None) => None,
            (Some(_), None) => {
                return Err(invalid(format!(
                    "{label} sets client_certificate without client_key"
                )));
            }
            (None, Some(_)) => {
                return Err(invalid(format!(
                    "{label} sets client_key without client_certificate"
                )));
            }
        };
        let roots = self
            .ca
            .as_deref()
            .map(|path| load_roots(path).map_err(|error| invalid(format!("{label}: {error}"))))
            .transpose()?;

        Ok(OutboundTls { identity, roots })
    }
}

/// Resolved outbound TLS material, holding its own rotation watch.
pub struct OutboundTls {
    /// Held for its watch as much as its resolver: dropping it stops reloads.
    identity: Option<ClientIdentity>,
    roots: Option<Vec<CertificateDer<'static>>>,
}

impl OutboundTls {
    /// A client presenting this destination's identity, under its own limits.
    ///
    /// Deliberately not drawn from the shared connector: an identity and a
    /// pinned authority belong to the destination that configured them, and
    /// pooling connections across destinations would mean presenting one
    /// service's certificate to another. The cost is one connection pool per
    /// destination that asks for mutual TLS, which is what asking for it means.
    fn client(
        &self,
        request_timeout: Duration,
        maximum_response_bytes: usize,
    ) -> Result<HttpClient, ConfigError> {
        HttpClient::with_identity(
            ClientConfig::default(),
            self.identity.as_ref().map(ClientIdentity::resolver),
            self.roots.clone(),
        )
        .map(|client| client.with_limits(request_timeout, maximum_response_bytes))
        .map_err(|error| invalid(error.to_string()))
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

/// Resolves the one stall deadline against the cadence it may be relative to.
fn apply_stall(
    node: &mut NodeConfig,
    stall: OptionalDuration,
    segment_duration: Duration,
) -> Result<(), ConfigError> {
    let Some(stall) = stall.0 else {
        // "off" means no idle cap at all. Legitimate on a trusted link; the
        // public-bind warning is what makes it visible elsewhere.
        node.session.supervision.health.stall = Duration::MAX;
        return Ok(());
    };
    if stall.is_zero() {
        return Err(invalid("accept.stall must be nonzero"));
    }
    // Sampling cannot observe a deadline shorter than its own period, so such
    // a value is not the tighter detection it looks like.
    let interval = node.session.supervision.health_interval;
    if stall < interval {
        return Err(invalid(format!(
            "accept.stall ({stall:?}) is shorter than the {interval:?} health interval, so it \
             cannot be observed"
        )));
    }
    // A stall shorter than one segment fails a publisher that is merely
    // between keyframes.
    if stall < segment_duration {
        return Err(invalid(format!(
            "accept.stall ({stall:?}) is shorter than the {segment_duration:?} segment duration, \
             which drops publishers between ordinary keyframes"
        )));
    }
    node.session.supervision.health.stall = stall;
    Ok(())
}

/// Settings that are legal but probably not what was meant.
///
/// Warned rather than refused: each is a deliberate choice somewhere, and
/// bouncing an administrator over a relationship they did not know existed is
/// the nagging this design avoids. Hard refusals are kept for the genuinely
/// unbootable.
fn startup_warnings(node: &NodeConfig, open_admission: bool) -> Vec<String> {
    let mut warnings = Vec::new();
    let public = |address: &SocketAddr| !address.ip().is_loopback();

    if open_admission && (public(&node.rtmp_address) || public(&node.srt_address)) {
        warnings.push(
            "an ingest listener is on a public address with no [auth.publish]: anyone who can \
             reach it may publish"
                .to_owned(),
        );
    }
    if node.store.maximum_streams >= usize::MAX / 2 {
        warnings.push("capacity.streams is effectively uncapped".to_owned());
    }
    if node.session.supervision.health.stall == Duration::MAX {
        warnings.push(
            "accept.stall is off: a publisher that goes quiet holds its stream name until it \
             disconnects"
                .to_owned(),
        );
    }
    warnings
}

#[derive(Conf)]
#[conf(serde)]
pub struct AcceptAppConfig {
    /// Throttle applied to a publisher offering media faster than `pace`.
    ///
    /// Omit the table for no ceiling, which is the compiled default: a
    /// publisher pushing as fast as its link allows is taken to be asking for
    /// exactly that.
    #[conf(flatten, prefix)]
    ceiling: Option<CeilingAppConfig>,
    /// Minimum rate a publisher must sustain. Omit the table for no floor.
    #[conf(flatten, prefix)]
    floor: Option<FloorAppConfig>,
    /// How long nothing usable may arrive before the publisher is dropped.
    #[conf(
        parameter,
        long,
        env,
        default_value = "12s",
        value_parser = parse_optional_duration,
        serde(use_value_parser)
    )]
    stall: OptionalDuration,
    /// Whether a second publisher may replace the one holding a stream name.
    ///
    /// Refusal by default, because silent replacement turns an encoder
    /// reconnect or a leaked credential into a hijack with no signal. The cost
    /// is a reconnect blackout after a half-open socket, bounded by `stall`.
    #[conf(parameter, long, env, default_value = "false")]
    takeover: bool,
    #[conf(parameter, value_parser = TomlValue::<VideoAcceptValue>::from_str)]
    video: Option<TomlValue<VideoAcceptValue>>,
    #[conf(parameter, value_parser = TomlValue::<AudioAcceptValue>::from_str)]
    audio: Option<TomlValue<AudioAcceptValue>>,
    #[conf(parameter, value_parser = TomlValue::<SubtitleAcceptValue>::from_str)]
    subtitles: Option<TomlValue<SubtitleAcceptValue>>,
    /// Named alternatives an admission response may select by name.
    #[conf(parameter, value_parser = TomlTable::<PolicyValue>::from_str)]
    policy: Option<TomlTable<PolicyValue>>,
}

impl AcceptAppConfig {
    /// The default policy, and every named alternative resolved beside it.
    fn resolve(&self) -> Result<(StreamPolicy, BTreeMap<String, StreamPolicy>), ConfigError> {
        let default = self.base()?;
        let mut policies = BTreeMap::new();
        for (name, configured) in self
            .policy
            .as_ref()
            .map(|table| &table.0)
            .into_iter()
            .flatten()
        {
            if name.trim().is_empty() {
                return Err(invalid("an accept policy name must not be empty"));
            }
            // A policy replaces `[accept]` wholesale rather than inheriting
            // from it: reading one table must answer what it admits, without
            // replaying a merge against another.
            policies.insert(name.clone(), configured.resolve(name)?);
        }
        Ok((default, policies))
    }

    fn base(&self) -> Result<StreamPolicy, ConfigError> {
        let policy = PolicyValue {
            ceiling: self.ceiling.as_ref().map(CeilingValue::from),
            floor: self.floor.as_ref().map(FloorValue::from),
            takeover: Some(self.takeover),
            video: self.video.as_ref().map(|value| value.0.clone()),
            audio: self.audio.as_ref().map(|value| value.0.clone()),
            subtitles: self.subtitles.as_ref().map(|value| value.0.clone()),
        };
        policy.resolve("accept")
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct CapacityAppConfig {
    /// Concurrent ingest sessions.
    #[conf(parameter, long, env, default_value = "256")]
    publishers: usize,
    /// Streams live, plus those still held by `retain`.
    ///
    /// Separate from `publishers` because they answer different questions: a
    /// publisher is an ingest session, a stream is a named presentation in the
    /// store. Once `retain` can be long the two decouple hard.
    #[conf(parameter, long, env, default_value = "1024")]
    streams: usize,
    /// Retained parts and segments for one stream.
    ///
    /// Bounds retained media only. A publisher also holds a fixed pipeline
    /// cost while ingesting, reported as `rushls_session_pipeline_bytes`.
    #[conf(
        parameter,
        long,
        env,
        default_value = "512MiB",
        serde(use_value_parser)
    )]
    memory_per_stream: ByteSize,
    /// Overflow of the same retain window. Omit to stay in memory.
    #[conf(parameter, long, env, serde(use_value_parser))]
    disk_per_stream: Option<ByteSize>,
    /// Generation directory for spilled media. Defaults to the platform cache.
    #[conf(parameter, long, env)]
    dir: Option<PathBuf>,
}

impl CapacityAppConfig {
    fn apply(&self, node: &mut NodeConfig) -> Result<(), ConfigError> {
        if self.publishers == 0 {
            return Err(invalid("capacity.publishers must be at least one"));
        }
        if self.streams == 0 {
            return Err(invalid("capacity.streams must be at least one"));
        }
        node.maximum_sessions = self.publishers;
        node.store.maximum_streams = self.streams;
        node.store.retention.maximum_payload_bytes =
            nonzero_bytes("capacity.memory_per_stream", self.memory_per_stream)?;
        node.store.disk = match (&self.disk_per_stream, &self.dir) {
            (None, None) => None,
            (Some(bytes), directory) => Some(DiskLimits {
                directory: match directory {
                    Some(directory) => directory.clone(),
                    None => paths::default_disk_directory().ok_or_else(|| {
                        invalid(
                            "could not determine a cache directory for disk overflow; set capacity.dir",
                        )
                    })?,
                },
                maximum_payload_bytes: nonzero_bytes("capacity.disk_per_stream", *bytes)?,
            }),
            (None, Some(_)) => {
                return Err(invalid(
                    "capacity.disk_per_stream is required when dir is set",
                ));
            }
        };
        Ok(())
    }
}

/// One TOML value handed over as its own text.
///
/// `conf` gives a table-valued parameter as raw TOML rather than as a parsed
/// value, so each of these parses itself. The same shape also lets an
/// environment override carry a TOML fragment, which is what keeps a
/// structured predicate overridable rather than file-only.
#[derive(Clone, Debug, Deserialize)]
#[serde(transparent)]
pub struct TomlValue<T>(T);

impl<T: serde::de::DeserializeOwned> FromStr for TomlValue<T> {
    type Err = toml::de::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Wrapped in a key so a bare inline table parses as a document, which
        // is the form both a file fragment and an environment value take.
        toml::from_str::<Wrapper<T>>(&format!("value = {value}")).map(|wrapper| Self(wrapper.value))
    }
}

#[derive(Deserialize)]
struct Wrapper<T> {
    value: T,
}

/// A rate written as a multiple of wall clock: "1x", "0.5x", "2x".
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "String")]
struct PaceValue(Pace);

impl FromStr for PaceValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let multiple = value
            .trim()
            .strip_suffix('x')
            .ok_or_else(|| format!("a pace must end in `x`, as in 1x; got `{value}`"))?;
        let (numerator, denominator) = decimal_fraction(multiple)?;
        let numerator = NonZeroU32::new(numerator)
            .ok_or_else(|| "a pace must be greater than zero".to_owned())?;
        let denominator = NonZeroU32::new(denominator)
            .ok_or_else(|| "a pace must be greater than zero".to_owned())?;
        Ok(Self(Pace::new(numerator, denominator)))
    }
}

impl TryFrom<String> for PaceValue {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_str(&value)
    }
}

fn parse_pace(value: &str) -> Result<PaceValue, String> {
    PaceValue::from_str(value)
}

/// Flattened `[accept.ceiling]`, so `--accept-ceiling-pace` and
/// `--accept-ceiling-burst` exist as ordinary flags.
///
/// `pace` is required once this table is present. `burst` is not: omitted
/// means an empty bucket, which is still realtime at `pace`.
#[derive(Clone, Copy, Conf)]
#[conf(serde)]
pub struct CeilingAppConfig {
    /// Long-run rate the bucket refills at, as a multiple of wall clock.
    #[conf(
        parameter,
        long,
        env,
        value_parser = parse_pace,
        serde(use_value_parser)
    )]
    pace: PaceValue,
    /// Head start, and the most media time the bucket may hold.
    ///
    /// Omit or `"0s"` for no head start: a publisher may not run ahead of
    /// wall clock. Idle time still cannot bank more than this.
    #[conf(
        parameter,
        long,
        env,
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    burst: Option<Duration>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CeilingValue {
    pace: PaceValue,
    #[serde(default)]
    burst: Option<String>,
}

impl From<&CeilingAppConfig> for CeilingValue {
    fn from(value: &CeilingAppConfig) -> Self {
        Self {
            pace: value.pace,
            burst: value
                .burst
                .map(|duration| humantime::format_duration(duration).to_string()),
        }
    }
}

/// Flattened `[accept.floor]`, so `--accept-floor-pace` and
/// `--accept-floor-window` exist as ordinary flags.
///
/// Both fields are required once this table is present: a rate without a
/// window cannot be judged, and a window without a rate has nothing to judge.
#[derive(Clone, Copy, Conf)]
#[conf(serde)]
pub struct FloorAppConfig {
    /// Minimum media-time progress against wall-time, averaged over `window`.
    #[conf(
        parameter,
        long,
        env,
        value_parser = parse_pace,
        serde(use_value_parser)
    )]
    pace: PaceValue,
    /// Averaging window. The first one is startup grace.
    #[conf(
        parameter,
        long,
        env,
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    window: Duration,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FloorValue {
    pace: PaceValue,
    window: String,
}

impl From<&FloorAppConfig> for FloorValue {
    fn from(value: &FloorAppConfig) -> Self {
        Self {
            pace: value.pace,
            window: humantime::format_duration(value.window).to_string(),
        }
    }
}

/// A media predicate: exact, one of a set, or an inclusive range.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum BoundsValue<T> {
    Exact(T),
    OneOf(Vec<T>),
    Range(RangeValue<T>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeValue<T> {
    min: Option<T>,
    max: Option<T>,
}

impl<T> BoundsValue<T> {
    fn resolve<U>(self, convert: impl Fn(T) -> U) -> Bounds<U> {
        match self {
            Self::Exact(value) => Bounds::Exact(convert(value)),
            Self::OneOf(values) => Bounds::OneOf(values.into_iter().map(convert).collect()),
            Self::Range(range) => Bounds::Range {
                min: range.min.map(&convert),
                max: range.max.map(&convert),
            },
        }
    }
}

/// A frame size: a named preset, or explicit width and height.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum FrameBoxValue {
    Named(String),
    Explicit { width: u32, height: u32 },
}

impl FrameBoxValue {
    /// Named sizes expand to boxes and then compare numerically, so a name is
    /// never a point on a one-dimensional scale.
    fn resolve(&self) -> Result<FrameBox, String> {
        match self {
            Self::Explicit { width, height } => Ok(FrameBox::new(*width, *height)),
            Self::Named(name) => match name.to_ascii_lowercase().as_str() {
                "sd" | "480p" => Ok(FrameBox::new(854, 480)),
                "hd" | "720p" => Ok(FrameBox::new(1280, 720)),
                "fhd" | "1080p" => Ok(FrameBox::new(1920, 1080)),
                "qhd" | "1440p" => Ok(FrameBox::new(2560, 1440)),
                "4k" | "2160p" => Ok(FrameBox::new(3840, 2160)),
                "8k" | "4320p" => Ok(FrameBox::new(7680, 4320)),
                other => match other.split_once('x') {
                    Some((width, height)) => Ok(FrameBox::new(
                        width
                            .parse()
                            .map_err(|_| format!("`{other}` is not a size"))?,
                        height
                            .parse()
                            .map_err(|_| format!("`{other}` is not a size"))?,
                    )),
                    None => Err(format!("unknown resolution `{other}`")),
                },
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum ResolutionValue {
    Bounded(ResolutionBound),
    Exact(FrameBoxValue),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionBound {
    max: Option<FrameBoxValue>,
}

impl ResolutionValue {
    fn resolve(&self) -> Result<Resolution, String> {
        match self {
            Self::Bounded(bound) => match &bound.max {
                Some(max) => Ok(Resolution::AtMost(max.resolve()?)),
                None => Ok(Resolution::Any),
            },
            Self::Exact(size) => Ok(Resolution::Exact(size.resolve()?)),
        }
    }
}

/// A frame rate: an integer, a decimal, or an exact quotient.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum RateValue {
    Integer(u32),
    Decimal(f64),
    Quotient(String),
}

/// The broadcast decimals, and the rationals they actually name.
///
/// `29.97` read literally is `2997/100`, which is not `30000/1001`. The two
/// are the same rate only because the decimal is conventional shorthand, so
/// the mapping is explicit rather than derived. Everything else is its literal
/// value, and comparison stays exact-rational either way.
///
/// Comparing to a tolerance was the alternative and is rejected: it would make
/// `frame_rate = 30` quietly admit 29.97, which is the predicate not doing
/// what it says, and no cutoff between two and three decimal places can be
/// justified over the other.
const NTSC_RATES: [(f64, u32, u32); 4] = [
    (23.976, 24_000, 1_001),
    (29.97, 30_000, 1_001),
    (59.94, 60_000, 1_001),
    (119.88, 120_000, 1_001),
];

impl RateValue {
    fn resolve(&self) -> Result<FrameRate, String> {
        match self {
            Self::Integer(value) => {
                let numerator = NonZeroU32::new(*value)
                    .ok_or_else(|| "a frame rate must be positive".to_owned())?;
                Ok(FrameRate::new(numerator, nz::u32!(1)))
            }
            Self::Decimal(value) => {
                if let Some((_, numerator, denominator)) = NTSC_RATES
                    .iter()
                    .find(|(decimal, _, _)| (decimal - value).abs() < f64::EPSILON)
                {
                    return Ok(FrameRate::new(
                        NonZeroU32::new(*numerator).expect("constant is nonzero"),
                        NonZeroU32::new(*denominator).expect("constant is nonzero"),
                    ));
                }
                let (numerator, denominator) = decimal_fraction(&value.to_string())?;
                Ok(FrameRate::new(
                    NonZeroU32::new(numerator)
                        .ok_or_else(|| "a frame rate must be positive".to_owned())?,
                    NonZeroU32::new(denominator)
                        .ok_or_else(|| "a frame rate must be positive".to_owned())?,
                ))
            }
            Self::Quotient(value) => {
                let (numerator, denominator) = value
                    .split_once('/')
                    .ok_or_else(|| format!("`{value}` is not a frame rate"))?;
                Ok(FrameRate::new(
                    parse_nonzero_u32("frame-rate numerator", numerator)?,
                    parse_nonzero_u32("frame-rate denominator", denominator)?,
                ))
            }
        }
    }
}

/// A sample rate in hertz, accepting the friendly `"48kHz"` spelling anywhere
/// a rate is written — including inside a range, since accepting it in one
/// position and not the other would be the worst of both.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum SampleRateValue {
    Hertz(u32),
    Friendly(String),
}

impl SampleRateValue {
    fn resolve(&self) -> Result<NonZeroU32, String> {
        let hertz = match self {
            Self::Hertz(value) => *value,
            Self::Friendly(value) => {
                let trimmed = value.trim();
                let lowered = trimmed.to_ascii_lowercase();
                match lowered.strip_suffix("khz") {
                    Some(kilohertz) => {
                        let (numerator, denominator) = decimal_fraction(kilohertz.trim())?;
                        numerator.saturating_mul(1_000) / denominator.max(1)
                    }
                    None => lowered
                        .strip_suffix("hz")
                        .unwrap_or(&lowered)
                        .trim()
                        .parse()
                        .map_err(|_| format!("`{trimmed}` is not a sample rate"))?,
                }
            }
        };
        NonZeroU32::new(hertz).ok_or_else(|| "a sample rate must be positive".to_owned())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VideoAcceptValue {
    codecs: Option<Vec<CodecValue>>,
    resolution: Option<ResolutionValue>,
    frame_rate: Option<BoundsValue<RateValue>>,
    tracks: Option<BoundsValue<usize>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioAcceptValue {
    codecs: Option<Vec<CodecValue>>,
    sample_rate: Option<BoundsValue<SampleRateValue>>,
    channels: Option<BoundsValue<u16>>,
    tracks: Option<BoundsValue<usize>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubtitleAcceptValue {
    codecs: Option<Vec<CodecValue>>,
    tracks: Option<BoundsValue<usize>>,
}

/// One complete accept set, whether the default or a named alternative.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyValue {
    ceiling: Option<CeilingValue>,
    floor: Option<FloorValue>,
    takeover: Option<bool>,
    video: Option<VideoAcceptValue>,
    audio: Option<AudioAcceptValue>,
    subtitles: Option<SubtitleAcceptValue>,
}

impl PolicyValue {
    fn resolve(&self, name: &str) -> Result<StreamPolicy, ConfigError> {
        let where_ = |error: String| invalid(format!("{name}: {error}"));
        let mut policy = StreamPolicy::permissive();

        if let Some(ceiling) = &self.ceiling {
            let burst = match &ceiling.burst {
                Some(value) => humantime::parse_duration(value)
                    .map_err(|error| where_(format!("ceiling.burst {error}")))?,
                // No head start: `pace` is permission to continue, not to lead.
                None => Duration::ZERO,
            };
            policy.ceiling = Some(Ceiling {
                pace: ceiling.pace.0,
                burst,
            });
        }
        if let Some(floor) = &self.floor {
            let window = humantime::parse_duration(&floor.window)
                .map_err(|error| where_(format!("floor.window {error}")))?;
            if window.is_zero() {
                return Err(where_("floor.window must be nonzero".to_owned()));
            }
            // A ceiling holding a publisher at exactly its floor makes
            // ordinary jitter fatal, and no value of the pair is usable, so
            // this is a refusal rather than a warning.
            if let Some(ceiling) = policy.ceiling
                && !floor.pace.0.is_slower_than(ceiling.pace)
            {
                return Err(where_(
                    "floor.pace must be slower than ceiling.pace, or the ceiling holds the \
                     publisher at exactly the floor and ordinary jitter trips it"
                        .to_owned(),
                ));
            }
            policy.floor = Some(Floor {
                pace: floor.pace.0,
                window,
            });
        }
        if let Some(takeover) = self.takeover {
            policy.takeovers = if takeover {
                TakeoverPolicy::Allow
            } else {
                TakeoverPolicy::Deny
            };
        }

        if let Some(video) = &self.video {
            if let Some(codecs) = &video.codecs {
                policy.video.codecs = Codecs::OneOf(validate_codecs(
                    name,
                    "video codecs",
                    codecs.clone(),
                    &[Codec::H264, Codec::Hevc, Codec::Av1],
                )?);
            }
            if let Some(resolution) = &video.resolution {
                policy.video.resolution = resolution.resolve().map_err(where_)?;
            }
            if let Some(frame_rate) = &video.frame_rate {
                policy.video.frame_rate =
                    resolve_bounds(frame_rate.clone(), RateValue::resolve).map_err(where_)?;
            }
            if let Some(tracks) = &video.tracks {
                policy.video.tracks = tracks.clone().resolve(|value| value);
            }
        }
        if let Some(audio) = &self.audio {
            if let Some(codecs) = &audio.codecs {
                policy.audio.codecs = Codecs::OneOf(validate_codecs(
                    name,
                    "audio codecs",
                    codecs.clone(),
                    &[Codec::Aac, Codec::Opus],
                )?);
            }
            if let Some(sample_rate) = &audio.sample_rate {
                policy.audio.sample_rate =
                    resolve_bounds(sample_rate.clone(), SampleRateValue::resolve)
                        .map_err(where_)?;
            }
            if let Some(channels) = &audio.channels {
                policy.audio.channels = resolve_bounds(channels.clone(), |value| {
                    NonZeroU16::new(*value).ok_or_else(|| "channels must be positive".to_owned())
                })
                .map_err(where_)?;
            }
            if let Some(tracks) = &audio.tracks {
                policy.audio.tracks = tracks.clone().resolve(|value| value);
            }
        }
        if let Some(subtitles) = &self.subtitles {
            if let Some(codecs) = &subtitles.codecs {
                policy.subtitles.codecs = Codecs::OneOf(validate_codecs(
                    name,
                    "subtitle codecs",
                    codecs.clone(),
                    &[Codec::WebVtt, Codec::SubRip, Codec::Text],
                )?);
            }
            if let Some(tracks) = &subtitles.tracks {
                policy.subtitles.tracks = tracks.clone().resolve(|value| value);
            }
        }
        Ok(policy)
    }
}

/// Resolves a predicate whose values need validating one at a time.
fn resolve_bounds<T, U>(
    bounds: BoundsValue<T>,
    convert: impl Fn(&T) -> Result<U, String>,
) -> Result<Bounds<U>, String> {
    Ok(match bounds {
        BoundsValue::Exact(value) => Bounds::Exact(convert(&value)?),
        BoundsValue::OneOf(values) => {
            Bounds::OneOf(values.iter().map(&convert).collect::<Result<Vec<_>, _>>()?)
        }
        BoundsValue::Range(range) => Bounds::Range {
            min: range.min.as_ref().map(&convert).transpose()?,
            max: range.max.as_ref().map(&convert).transpose()?,
        },
    })
}

#[derive(Conf)]
#[conf(serde)]
pub struct RtmpAppConfig {
    /// Address receiving RTMP publishers.
    #[conf(parameter, long, env, default_value = "0.0.0.0:1935")]
    pub listen: SocketAddr,
    /// How long an established publisher may produce nothing before its
    /// session is closed.
    ///
    /// One operator-facing value. The handshake gets its own, tighter limit,
    /// derived rather than configured: the two phases are not equivalent and
    /// sizing them together would be wrong in one direction or the other. A
    /// handshake covers an unauthenticated peer, which is the cheapest way to
    /// hold a socket open; an established session covers a publisher that has
    /// proved itself, where a tight limit drops a legitimate stream between
    /// keyframes.
    ///
    /// Exposing both invites exactly the pairing the derivation prevents: a
    /// generous session timeout accidentally applied to unauthenticated peers.
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = parse_optional_duration,
        serde(use_value_parser)
    )]
    timeout: OptionalDuration,
}

/// The unauthenticated phase is never more patient than a few seconds, however
/// generous an established session is allowed to be.
const MAXIMUM_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

impl RtmpAppConfig {
    /// Derives the per-phase timeouts from the one operator-facing value.
    fn timeouts(&self) -> ServerSessionTimeouts {
        let session = self.timeout.0;
        ServerSessionTimeouts {
            // The lesser of the session limit and the constant: a disabled
            // session timeout still leaves the handshake bounded, because an
            // unauthenticated peer is the one that should never be trusted to
            // hold a socket indefinitely.
            handshake_read: Some(session.map_or(MAXIMUM_HANDSHAKE_TIMEOUT, |session| {
                session.min(MAXIMUM_HANDSHAKE_TIMEOUT)
            })),
            session_read: session,
            write: session,
        }
    }

    fn apply(&self, node: &mut NodeConfig, warnings: &mut Vec<String>) -> Result<(), ConfigError> {
        let timeouts = self.timeouts();

        // A session read shorter than a keyframe interval drops publishers
        // mid-GOP. Nothing here knows the publisher's cadence, so this only
        // catches values too small to be deliberate.
        if let Some(session) = timeouts.session_read
            && session < Duration::from_secs(1)
        {
            return Err(ConfigError::Invalid(format!(
                "the RTMP timeout ({session:?}) is below one second, which drops publishers \
                 between ordinary keyframes"
            )));
        }

        node.rtmp.timeouts = timeouts;

        // Disabling a timeout is legitimate on a trusted link and a liability
        // on a public one, and nothing here can tell which this is. Say so
        // rather than let an unbounded wait be invisible.
        if timeouts.session_read.is_none() {
            warnings.push(
                "the RTMP timeout is disabled: a publisher that stops responding holds its \
                 connection until it is closed from the other end"
                    .to_owned(),
            );
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

/// An address that may be explicitly disabled.
///
/// Mirrors [`OptionalDuration`]: turning one listener off is how an operator
/// says "HTTPS only", and an absurd address is not a way to spell that.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OptionalAddress(pub Option<SocketAddr>);

impl std::fmt::Display for OptionalAddress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(address) => write!(formatter, "{address}"),
            None => formatter.write_str("off"),
        }
    }
}

fn parse_optional_address(value: &str) -> Result<OptionalAddress, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("off") {
        return Ok(OptionalAddress(None));
    }
    trimmed
        .parse()
        .map(|address| OptionalAddress(Some(address)))
        .map_err(|error| format!("{error}"))
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
    #[conf(parameter, long, env, default_value = "0.0.0.0:9000")]
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
    /// How long an SRT peer may send nothing before its session is dropped.
    ///
    /// Deliberately not folded into the RTMP `timeout`: SRT is connectionless
    /// and keeps its own keepalive, so this bounds a protocol-level idle
    /// rather than a stalled socket read. Sharing a knob would imply the two
    /// move together, and they should not.
    ///
    /// Bounded below by `latency`: a deadline inside the receiver's own
    /// reordering window fires on packets the transport is still legitimately
    /// waiting for, which is why raising `latency` for a long-haul link
    /// without raising this is refused rather than tolerated.
    #[conf(
        parameter,
        long,
        env,
        default_value = "5s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    timeout: Duration,
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
        // An idle deadline inside the receiver's own latency window would fire
        // on packets the transport is still legitimately waiting to reorder.
        if self.timeout <= self.latency {
            return Err(invalid(format!(
                "the SRT timeout ({:?}) must exceed the receive latency ({:?})",
                self.timeout, self.latency
            )));
        }
        let passphrase = resolve_optional_text_secret(
            "SRT passphrase",
            self.passphrase.as_ref(),
            self.passphrase_file.as_ref(),
        )?;
        node.srt.latency = self.latency;
        node.srt.peer_idle_timeout = self.timeout;
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
    /// Desired HLS segment duration. Keyframe cadence may adjust the result.
    #[conf(
        parameter,
        long,
        env,
        default_value = "6s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    segment: Duration,
    /// Desired low-latency HLS partial-segment duration.
    #[conf(
        parameter,
        long,
        env,
        default_value = "1s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    part: Duration,
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
    retain: DurationRule,
    /// How far behind the live edge a player is told to start.
    ///
    /// A multiple of the part duration (`"3x"`) or a fixed duration (`"3s"`).
    /// This is the floor on live-edge latency, and the multiple form is the
    /// default because the quantity it bounds is the part cadence itself.
    #[conf(
        parameter,
        long,
        env,
        default_value = "3x",
        value_parser = parse_playlist_window,
        serde(use_value_parser)
    )]
    hold_back: DurationRule,
}

impl HlsAppConfig {
    /// The segment duration other sections size their relative values by.
    fn segment_duration(&self) -> Duration {
        self.segment
    }

    fn apply(&self, node: &mut NodeConfig, warnings: &mut Vec<String>) -> Result<(), ConfigError> {
        if self.segment.is_zero() || self.part.is_zero() {
            return Err(invalid("HLS segment and part durations must be nonzero"));
        }
        if self.part > self.segment {
            return Err(invalid(
                "HLS part duration must not exceed the segment duration",
            ));
        }
        let window = self.retain;
        let window_duration = window.resolve(self.segment);
        if window_duration.is_zero() {
            return Err(invalid("HLS playlist window must be nonzero"));
        }
        if let DurationRule::Fixed(fixed) = window
            && fixed < self.segment.saturating_mul(3)
        {
            return Err(invalid(
                "HLS playlist window must be at least three times the segment duration",
            ));
        }
        node.session.segmentation = SegmentationPolicy::latency_first(self.segment, self.part);
        // Interim mapping: the advertised window and the retention window are
        // one quantity now, so the old playlist knob resolves straight into
        // it. `[hls] retain` replaces this when the file is rewritten.
        node.store.retention.retain = window_duration;
        // Two thresholds, because the specification has two. Below three parts
        // is a SHOULD, so it is warned: a deployment on a good network may
        // genuinely want the latency, and refusing would deny a legitimate
        // choice. Below two parts is a MUST, so it is refused — a value the
        // protocol forbids cannot be honoured approximately, and advertising
        // it anyway makes clients stall.
        let hold_back = self.hold_back.resolve(self.part);
        if hold_back < self.part.saturating_mul(2) {
            return Err(invalid(format!(
                "HLS hold_back ({hold_back:?}) must be at least twice the part duration ({:?}): \
                 a client given less than two parts of head start runs out of buffered media \
                 on any loss",
                self.part
            )));
        }
        if hold_back < self.part.saturating_mul(3) {
            warnings.push(format!(
                "hls.hold_back ({hold_back:?}) is below the three part durations HLS \
                 recommends; clients on a lossy link may stall at the live edge"
            ));
        }
        node.hls.timing.part_hold_back = self.hold_back;
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
pub struct HttpAppConfig {
    /// Address serving HLS and health probes, or `"off"` to serve HTTPS only.
    #[conf(
        parameter,
        long,
        env,
        default_value = "0.0.0.0:8080",
        value_parser = parse_optional_address,
        serde(use_value_parser)
    )]
    pub listen: OptionalAddress,
    /// Absolute URL prefix for the names playlists emit; empty is relative,
    /// which is right behind a proxy or CDN.
    #[conf(parameter, long, env, default_value = "")]
    public_url: String,
    #[conf(flatten, prefix)]
    cors: CorsAppConfig,
    #[conf(flatten, prefix)]
    tls: Option<TlsAppConfig>,
}

impl HttpAppConfig {
    fn resolve(&self) -> Result<HttpConfig, ConfigError> {
        let config = HttpConfig {
            cors: self.cors.resolve()?,
            tls: self.tls.as_ref().map(TlsAppConfig::resolve).transpose()?,
            tls_address: self.tls.as_ref().map(|tls| tls.listen),
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
    /// Address serving HTTPS, bound independently of the cleartext listener.
    #[conf(parameter, long, env, default_value = "[::]:8443")]
    listen: SocketAddr,
    /// Path to a PEM certificate chain, leaf first.
    #[conf(parameter, long, env)]
    certificate: PathBuf,
    /// Path to a PEM private key.
    #[conf(parameter, long, env)]
    key: PathBuf,
    /// Bounds a connection that completes TCP and then stalls mid-handshake.
    ///
    /// This is the unauthenticated edge of the node, and a TLS handshake is
    /// more expensive to hold open than a plain socket, so it is worth keeping
    /// tight.
    #[conf(
        parameter,
        long,
        env,
        default_value = "5s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    handshake_timeout: Duration,
    /// Handshakes admitted at once, which is what stops a flood from growing
    /// the task set without bound.
    ///
    /// Sized together with `handshake_timeout`: the cost an unauthenticated
    /// peer can impose is the product of the two, so tightening one while
    /// leaving the other untouched buys less than it appears to.
    #[conf(parameter, long, env, default_value = "256")]
    maximum_pending_handshakes: usize,
}

impl TlsAppConfig {
    fn resolve(&self) -> Result<TlsSettings, ConfigError> {
        if self.maximum_pending_handshakes == 0 {
            return Err(ConfigError::Invalid(
                "http.tls.maximum_pending_handshakes must be at least one, or no \
                 TLS connection can be admitted"
                    .to_owned(),
            ));
        }

        Ok(TlsSettings {
            certificate: self.certificate.clone(),
            key: self.key.clone(),
            handshake_timeout: self.handshake_timeout,
            maximum_pending_handshakes: self.maximum_pending_handshakes,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct MetricsAppConfig {
    /// Where metrics are served. Absent exports nothing.
    ///
    /// Its own listener, defaulting to loopback where set: per-stream series
    /// name every stream currently published, which the viewer-facing port
    /// should not offer. Set it to the HTTP or HTTPS address to share that
    /// port instead, which is then a visible choice rather than a magic value.
    /// Sharing the HTTPS address is how scrapes happen over TLS; a dedicated
    /// metrics listener is always cleartext.
    #[conf(parameter, long, env)]
    listen: Option<SocketAddr>,
    /// Optional bearer token required to scrape `/metrics` and `/metrics/streams`.
    #[conf(parameter, env, secret)]
    token: Option<String>,
    /// File containing the optional metrics bearer token.
    #[conf(parameter, long, env)]
    token_file: Option<PathBuf>,
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
            listen: self.listen,
            token: token.map(MetricsToken::new),
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

fn explicit_config_path(args: &[OsString], env: &[(OsString, OsString)]) -> Option<PathBuf> {
    find_parameter("config", args.iter().cloned())
        .map(PathBuf::from)
        .or_else(|| env_value(env, "RUSHLS_CONFIG").map(PathBuf::from))
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

mod interpolate;
mod paths;

#[cfg(test)]
mod tests;
