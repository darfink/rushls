//! Operator-facing configuration and its translation into runtime policy.
//!
//! This module describes administrator intent, not the shape of the internal
//! pipeline. Low-level configuration starts from its library defaults and only
//! the deliberately supported operator choices are applied here.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt,
    net::SocketAddr,
    num::{NonZeroU16, NonZeroU32},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use crate::source::transport::rtmp::RtmpTimeouts;
use conf::Conf;
use rushls_common::config::{
    ByteSize, ConfigSearch, Loader, OptionalAddress, OptionalBytes, OptionalDuration, TextSource,
    TomlTable, TomlValue, parse_optional_address, parse_optional_bytes, parse_optional_duration,
    reference::{Reference, TableKey},
};
use rushls_common::tls::{ClientIdentity, TlsVersion, load_roots};
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
    outbound::{BearerToken, ClientConfig, Endpoint, HttpClient, LazyHttpClient},
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

const LOADER: Loader = Loader::new("rushls", "RUSHLS_");

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
    /// Viewer JWT settings, present when `[playback.auth]` is configured.
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
    /// `--check`: validate, print [`Self::plan`], and exit without serving.
    pub check: bool,
    /// How the process logs, applied once configuration has loaded.
    pub log: LogSettings,
}

/// Resolved `[log]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogSettings {
    /// An `EnvFilter` directive, already validated.
    pub filter: String,
    pub format: LogFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
#[display(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable lines.
    Text,
    /// One JSON object per line, for log collectors.
    Json,
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        one_of(&[Self::Text, Self::Json], value, "log format")
    }
}

impl ResolvedAppConfig {
    /// What `--check` prints: every listener, and the memory the limits can
    /// commit in the worst case, so an unrealistic plan is visible before
    /// the node serves anything.
    pub fn plan(&self) -> String {
        use std::fmt::Write as _;
        let node = &self.node;
        let address = |address: Option<SocketAddr>| {
            address.map_or_else(|| "off".to_owned(), |address| address.to_string())
        };
        let size = |bytes: usize| ByteSize::b(bytes as u64).display().iec().to_string();
        let mut plan = String::new();
        let mut line = |text: String| {
            writeln!(plan, "{text}").expect("writing to a String cannot fail");
        };
        line("listeners".to_owned());
        for (name, value) in [
            ("ingest.rtmp", address(Some(node.rtmp_address))),
            ("ingest.rtmps", address(node.rtmps_address)),
            ("ingest.srt", address(Some(node.srt_address))),
            ("ingest.moq", address(node.moq_address)),
            ("http", address(node.http_address)),
            ("https", address(node.https_address)),
            ("metrics", address(node.metrics.listen)),
        ] {
            line(format!("  {name:<12} {value}"));
        }

        let publishers = node.maximum_sessions;
        let streams = node.store.maximum_streams;
        let per_stream = node.store.retention.maximum_payload_bytes;
        let stream_total = per_stream.saturating_mul(streams);
        line("memory".to_owned());
        let worst = if let Some(per_publisher) = node.session.memory_per_publisher {
            let publisher_total = per_publisher.saturating_mul(publishers);
            line(format!(
                "  publishers   {publishers} × {} = {}",
                size(per_publisher),
                size(publisher_total)
            ));
            Some(publisher_total.saturating_add(stream_total))
        } else {
            line(format!("  publishers   {publishers} × unlimited"));
            None
        };
        line(format!(
            "  streams      {streams} × {} = {}",
            size(per_stream),
            size(stream_total)
        ));
        let worst = worst.map_or_else(|| "unbounded".to_owned(), size);
        match node.memory_total {
            Some(total) => line(format!(
                "  committed    at most {} (memory.total; budgets sum to {worst})",
                size(total)
            )),
            None => line(format!(
                "  committed    up to {worst} (set memory.total to cap it)"
            )),
        }
        match &node.store.disk {
            Some(disk) => line(format!(
                "disk           {streams} × {} = {} in {}",
                size(disk.maximum_payload_bytes),
                size(disk.maximum_payload_bytes.saturating_mul(streams)),
                disk.directory.display()
            )),
            None => line("disk           off".to_owned()),
        }
        plan
    }
}

/// TOML path of a schema option, or `None` for command-line-only flags.
pub fn toml_path(id: &str) -> Option<String> {
    if matches!(id, "config" | "print_config_example" | "check") {
        return None;
    }
    // Server fields flatten into the document root; their Rust IDs retain `node`.
    let path = id.strip_prefix("node.").unwrap_or(id);
    // A credential's file flag is part of that credential's row, not a key.
    if CREDENTIAL_FILE_FLAGS.contains(&path) {
        return None;
    }
    Some(path.to_owned())
}

/// Credentials whose command-line flag takes a file path. `conf` refuses a
/// flag on a secret field, so each has a CLI-only `<name>_file` sibling.
const CREDENTIAL_FILE_FLAGS: &[&str] = &[
    "ingest.srt.passphrase_file",
    "publish.auth.token_file",
    "playback.auth.secret_file",
    "metrics.token_file",
];

/// Renders `docs/configuration-reference.md` from the schema itself, so the reference
/// can only describe settings that exist, with the defaults they really have.
pub fn reference_markdown() -> String {
    rushls_common::config::reference::reference_markdown::<AppConfig>(&Reference {
        preamble: "# Configuration reference\n\n\
         Generated from the configuration schema; do not edit by hand. Regenerate with\n\
         `RUSHLS_UPDATE_CONFIG_REFERENCE=1 cargo test --lib config_reference`.\n\n\
         Values resolve defaults < TOML < environment < CLI. Credentials take a string,\n\
         `\"${VAR}\"`, or `{ file = \"/path\" }`. Their CLI flags take only a file path,\n\
         because arguments are visible in the process list. Structured values\n\
         (tables and lists) use TOML syntax in environment variables and flags too.\n\
         See [rushls.example.toml](../rushls.example.toml) for an annotated file.\n",
        toml_path: &toml_path,
        credential_file_flags: CREDENTIAL_FILE_FLAGS,
        tables: &[("record", RECORD_FIELDS), ("hook", HOOK_FIELDS)],
    })
}

/// `[record]` keys, as `record::Config` accepts them.
const RECORD_FIELDS: &[TableKey<'static>] = &[
    TableKey {
        suffix: ".dir",
        default: "",
        description: "Directory recordings are written under. Setting the table enables recording.",
    },
    TableKey {
        suffix: ".path",
        default: "{stream}/{publication}/{time:%Y/%m/%d}/{rendition}_{segment}.mp4",
        description: "Where each completed segment lands under `dir`.",
    },
    TableKey {
        suffix: ".queue_size",
        default: "128",
        description: "Segments queued for writing before new ones are dropped.",
    },
    TableKey {
        suffix: ".max_pending",
        default: "256MiB",
        description: "Open segments, queued jobs, and the active write together.",
    },
];

/// `[hook.<name>]` keys, as `HookEndpointAppConfig` accepts them.
const HOOK_FIELDS: &[TableKey<'static>] = &[
    TableKey {
        suffix: ".<name>.url",
        default: "",
        description: "Where deliveries are posted.",
    },
    TableKey {
        suffix: ".<name>.events",
        default: "",
        description: "Events this destination receives; required.",
    },
    TableKey {
        suffix: ".<name>.token",
        default: "",
        description: "Bearer credential: inline, `${VAR}`, or `{ file = \"/path\" }`.",
    },
    TableKey {
        suffix: ".<name>.signing_secret",
        default: "",
        description: "`whsec_` key signing each delivery.",
    },
    TableKey {
        suffix: ".<name>.client_cert",
        default: "",
        description: "PEM certificate chain presented to this endpoint.",
    },
    TableKey {
        suffix: ".<name>.client_key",
        default: "",
        description: "PEM private key for that chain.",
    },
    TableKey {
        suffix: ".<name>.ca",
        default: "",
        description: "PEM authority to trust instead of the platform store.",
    },
];

/// Hooks and the client they deliver with, which carries their own deadline.
pub struct ResolvedHooks {
    pub config: HooksConfig,
    pub client: HttpClient,
}

/// Failures while locating, reading, parsing, or resolving configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Loading(#[from] rushls_common::config::ConfigError),
    #[error("could not build outbound client: {0}")]
    Outbound(#[from] rushls_common::outbound::OutboundError),
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
            Self::Loading(rushls_common::config::ConfigError::Sources(error)) => error.exit(),
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
    /// Print the annotated configuration example and exit without loading configuration.
    #[conf(flag, long, serde(skip))]
    pub print_config_example: bool,
    /// Validate the configuration, print the resolved listeners and memory
    /// plan, and exit without serving.
    #[conf(flag, long, serde(skip))]
    pub check: bool,

    #[conf(flatten, serde(flatten))]
    pub node: ServerAppConfig,
    /// How publishers connect: listeners and liveness deadlines.
    #[conf(flatten, prefix)]
    pub ingest: IngestAppConfig,
    /// Who may publish, and what they may send.
    #[conf(flatten, prefix)]
    pub publish: PublishAppConfig,
    /// How many publishers and streams this node holds at once.
    #[conf(flatten, prefix)]
    pub limits: LimitsAppConfig,
    /// Memory budgets for publishers and stored streams.
    #[conf(flatten, prefix)]
    pub memory: MemoryAppConfig,
    /// Optional disk overflow for older stream media.
    #[conf(flatten, prefix)]
    pub disk: DiskAppConfig,
    #[conf(flatten, prefix)]
    pub hls: HlsAppConfig,
    #[conf(flatten, prefix)]
    pub http: HttpAppConfig,
    /// HTTPS listener. Present enables it, with certificates from `[tls]`.
    #[conf(flatten, prefix)]
    pub https: Option<HttpsAppConfig>,
    /// Certificate and key shared by HTTPS, RTMPS, and MoQ ingest.
    #[conf(flatten, prefix)]
    pub tls: Option<TlsAppConfig>,
    /// Who may watch.
    #[conf(flatten, prefix)]
    pub playback: PlaybackAppConfig,
    #[conf(flatten, prefix)]
    pub metrics: MetricsAppConfig,
    /// What the process logs, and in which format.
    #[conf(flatten, prefix)]
    pub log: LogAppConfig,
    /// Persistent local segment exports, independent of DVR retention.
    #[conf(parameter, value_parser = TomlValue::<crate::delivery::record::Config>::from_str)]
    pub record: Option<TomlValue<crate::delivery::record::Config>>,
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
    #[conf(parameter, secret, value_parser = TomlTable::<HookEndpointAppConfig>::from_str)]
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
            ConfigSearch::WellKnown,
        )
        .map(|(config, _)| config)
    }

    /// Loads and resolves process arguments, environment, and a configuration
    /// file from the live process snapshot, including well-known search paths.
    pub fn load_and_resolve() -> Result<ResolvedAppConfig, ConfigError> {
        Self::load_and_resolve_from_with(
            std::env::args_os(),
            std::env::vars_os(),
            ConfigSearch::WellKnown,
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
        Self::load_and_resolve_from_with(args, env, ConfigSearch::ExplicitOnly)
    }

    fn load_and_resolve_from_with(
        args: impl IntoIterator<Item = OsString>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
        search: ConfigSearch,
    ) -> Result<ResolvedAppConfig, ConfigError> {
        let env: Vec<(OsString, OsString)> = env.into_iter().collect();
        let loaded = Loader { search, ..LOADER }.load_from::<Self>(args, env.iter().cloned())?;
        // `RUST_LOG` ranks as an environment value, so a `--log-level` flag
        // still beats it; withholding it from resolution is how.
        let from_cli = matches!(
            loaded.sources.get("log.level"),
            Some(rushls_common::config::Source::Cli)
        );
        let env = env
            .into_iter()
            .filter(|(name, _)| !(from_cli && name == "RUST_LOG"))
            .collect::<Vec<_>>();
        let config_file = loaded.path;
        let mut resolved = loaded.config.resolve_from(env)?;
        resolved.config_file = config_file;
        Ok(resolved)
    }

    /// Loads from explicit source snapshots, keeping configuration tests free
    /// from process-global environment mutation and from well-known files.
    pub fn load_from(
        args: impl IntoIterator<Item = OsString>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, ConfigError> {
        Self::load_from_with(args, env, ConfigSearch::ExplicitOnly).map(|(config, _)| config)
    }

    fn load_from_with(
        args: impl IntoIterator<Item = OsString>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
        search: ConfigSearch,
    ) -> Result<(Self, Option<PathBuf>), ConfigError> {
        let loaded = Loader { search, ..LOADER }.load_from::<Self>(args, env)?;
        Ok((loaded.config, loaded.path))
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
        let env = env.into_iter().collect::<Vec<_>>();
        let mut warnings = LOADER.environment_warnings::<Self>(&env);
        let log = self.log.resolve(&env)?;

        let (default_policy, policies) = self.publish.resolve()?;
        // The manifest promise covers retained media across every takeover. A
        // named permissive profile can be selected on a later publication.
        let independent_segments = default_policy.input_mode == crate::domain::InputMode::Strict
            && policies
                .values()
                .all(|policy| policy.input_mode == crate::domain::InputMode::Strict);
        let stall = self.ingest.stall_timeout;
        let open_admission = self.publish.auth.is_none();
        let mut outbound_tls = Vec::new();
        let authenticator = match self.publish.auth {
            Some(auth) => auth
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
        let playback = self
            .playback
            .auth
            .map(|playback| playback.resolve(&mut client))
            .transpose()?;

        let mut node = NodeConfig {
            shutdown: self.node.shutdown_grace,
            rtmp_address: self.ingest.rtmp.listen,
            rtmp_proxy_protocol: self.ingest.rtmp.proxy_protocol,
            srt_address: self.ingest.srt.listen,
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
        let tls = self.tls.as_ref().map(TlsAppConfig::resolve);
        self.ingest.apply(&mut node, tls.as_ref(), &mut warnings)?;
        self.hls.apply(&mut node, &mut warnings)?;
        node.store.independent_segments = independent_segments;
        // After HLS: a stall expressed as a multiple is sized by the segment
        // duration, which only `hls.apply` establishes.
        apply_stall(&mut node, stall, self.hls.segment_duration())?;
        self.limits.apply(&mut node)?;
        self.memory.apply(&mut node)?;
        self.disk.apply(&mut node)?;
        node.hls.uri_base = UriBase::new(self.http.public_url.clone());
        node.http = self.http.resolve(self.https.as_ref(), tls.as_ref())?;
        node.https_address = node.http.tls_address;
        if tls.is_some()
            && self.https.is_none()
            && node.moq_address.is_none()
            && node.rtmps_address.is_none()
        {
            warnings.push(
                "[tls] is configured but none of [https], ingest.rtmps, or ingest.moq uses it"
                    .to_owned(),
            );
        }
        // After `http.resolve`, which replaces the whole struct: the
        // fingerprint route is mounted from the MOQ listener's own
        // certificate, so it exists exactly when MOQ ingest does.
        node.http.moq_certificate = node
            .moq
            .tls
            .as_ref()
            .map(|settings| settings.certificate.clone());
        node.metrics = self.metrics.resolve()?;
        if node.http_address.is_none() && node.https_address.is_none() {
            return Err(invalid(
                "both listeners are off, so this node could serve nothing",
            ));
        }
        node.record = self.record.map(|value| value.0);
        if let Some(record) = &node.record {
            record.validate().map_err(invalid)?;
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
            check: self.check,
            log,
        })
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
    /// retained media, a wait bounded by `hls.window` that no scheduler grants.
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = rushls_common::config::parse_duration,
        serde(use_value_parser)
    )]
    pub shutdown_grace: Duration,
}

/// Viewer authorization. A table of its own so `[playback.auth]` reads as the
/// counterpart of `[publish.auth]`.
#[derive(Conf)]
#[conf(serde)]
pub struct PlaybackAppConfig {
    /// Optional local JWT verification for viewers. When omitted, anyone with
    /// the URL may watch.
    #[conf(flatten, prefix)]
    auth: Option<PlaybackAuthAppConfig>,
}

#[derive(Conf)]
#[conf(serde)]
pub struct PlaybackAuthAppConfig {
    /// RS256/ES256 verifying key: inline PEM or `{ file = "/path" }`.
    #[conf(parameter, long, env)]
    public_key: Option<TextSource>,
    /// Issuer key set fetched when the gate starts.
    #[conf(parameter, long, env)]
    jwks_url: Option<String>,
    /// HS256 shared secret: inline, `${VAR}`, or `{ file = "/path" }`.
    #[conf(parameter, env, secret)]
    secret: Option<TextSource>,
    /// File holding `secret`; the command-line form is always a path.
    #[conf(parameter, long = "secret", serde(skip))]
    secret_file: Option<PathBuf>,
    /// Claim carrying the stream the token admits.
    #[conf(parameter, long, env, default_value = "stream")]
    stream_claim: String,
    /// Clock skew allowed on `exp` and `nbf`.
    #[conf(
        parameter,
        long,
        env,
        default_value = "30s",
        value_parser = rushls_common::config::parse_duration,
        serde(use_value_parser)
    )]
    leeway: Duration,
    /// Claims that must be present with these values; `iss` and `aud` are required.
    #[conf(parameter, value_parser = TomlTable::<ClaimValue>::from_str)]
    claims: Option<TomlTable<ClaimValue>>,
}

impl PlaybackAuthAppConfig {
    fn resolve(self, client: &mut LazyHttpClient) -> Result<PlaybackSettings, ConfigError> {
        let public_key = read_text("the playback public key", self.public_key.as_ref())?;
        let secret = read_credential(
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
                    "[playback.auth] must set exactly one of public_key, jwks_url, or secret",
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
            "[playback.auth.claims] {name} must be a non-empty string"
        ))),
        None => Err(invalid(format!(
            "[playback.auth.claims] must include {name}"
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
        value_parser = rushls_common::config::parse_duration,
        serde(use_value_parser)
    )]
    timeout: Duration,
    /// Largest decision this node will read.
    #[conf(parameter, long, env, default_value = "64KiB", serde(use_value_parser))]
    max_response: ByteSize,
    /// Bearer credential presented to the service: inline, `${VAR}`, or
    /// `{ file = "/path" }`.
    #[conf(parameter, env, secret)]
    token: Option<TextSource>,
    /// File holding `token`; the command-line form is always a path.
    #[conf(parameter, long = "token", serde(skip))]
    token_file: Option<PathBuf>,
    /// Path to a PEM certificate chain this node presents to the service.
    #[conf(parameter, long, env)]
    client_cert: Option<PathBuf>,
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
        if self.timeout >= admission_deadline {
            return Err(invalid(format!(
                "the auth request timeout ({:?}) must be shorter than the admission deadline ({admission_deadline:?})",
                self.timeout
            )));
        }
        let token = read_credential(
            "the publish auth token",
            self.token.as_ref(),
            self.token_file.as_ref(),
        )?;

        Ok(HttpAuthenticator::new(
            HttpAuthConfig {
                endpoint: Endpoint::parse(&self.url).map_err(|error| invalid(error.to_string()))?,
                // `[publish]` itself is the unnamed default, so a response
                // naming no profile gets it.
                default,
                policies,
                bearer: token
                    .map(|token| BearerToken::new(&token))
                    .transpose()
                    .map_err(|error| invalid(error.to_string()))?,
            },
            {
                let limit = nonzero_bytes("publish.auth.max_response", self.max_response)?;
                let tls = OutboundTlsAppConfig {
                    certificate: self.client_cert.clone(),
                    key: self.client_key.clone(),
                    ca: self.ca.clone(),
                };
                if tls.is_configured() {
                    let tls = tls.resolve("[publish.auth]")?;
                    let built = tls.client(self.timeout, limit)?;
                    // The watch lives as long as the resolved configuration,
                    // because dropping it stops rotations being noticed.
                    outbound_tls.push(tls);
                    built
                } else {
                    client.with_limits(self.timeout, limit)?
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

/// Events held for one destination before the oldest is dropped. Delivery is
/// best-effort, so this bounds memory for a destination that is down rather
/// than promising a backlog.
const HOOK_QUEUE_CAPACITY: usize = 1_000;

/// Distinct streams delivered at once. One request per stream is the ordering
/// rule, so this is also the concurrency.
const HOOK_MAX_IN_FLIGHT: usize = 8;

/// Attempts per event, the first included.
const HOOK_MAX_ATTEMPTS: u32 = 5;

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
    /// Bearer credential presented to this endpoint.
    #[serde(default)]
    token: Option<TextSource>,
    /// `whsec_` key signing each delivery.
    #[serde(default)]
    signing_secret: Option<TextSource>,
    /// Path to a PEM certificate chain this node presents to this endpoint.
    client_cert: Option<PathBuf>,
    /// Path to the PEM private key for that chain.
    client_key: Option<PathBuf>,
    /// Path to a PEM authority to trust instead of the platform store.
    ca: Option<PathBuf>,
}

impl HookEndpointAppConfig {
    fn resolve(
        self,
        name: &str,
        client: &mut LazyHttpClient,
        outbound_tls: &mut Vec<OutboundTls>,
    ) -> Result<HookConfig, ConfigError> {
        let mut events = BTreeSet::new();
        for event in &self.events {
            events.insert(
                Kind::from_str(event)
                    .map_err(|error| invalid(format!("hook `{name}`: {error}")))?,
            );
        }
        let token = read_text(&format!("the token for hook `{name}`"), self.token.as_ref())?;

        let signing_secret = read_text(
            &format!("the signing secret for hook `{name}`"),
            self.signing_secret.as_ref(),
        )?
        .map(|secret| rushls_common::hooks::SigningSecret::parse(&secret))
        .transpose()
        .map_err(|error| invalid(error.to_string()))?;

        // Only a destination that asked for an identity or a pinned authority
        // gets a client of its own; everything else shares the process pool.
        // The material is held in `outbound_tls` for the same reason
        // admission's is: dropping it stops rotations being noticed.
        let tls = OutboundTlsAppConfig {
            certificate: self.client_cert.clone(),
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

        let config = HookConfig {
            name: Arc::from(name),
            endpoint: Endpoint::parse(&self.url).map_err(|error| invalid(error.to_string()))?,
            events,
            queue_capacity: HOOK_QUEUE_CAPACITY,
            maximum_in_flight: HOOK_MAX_IN_FLIGHT,
            maximum_attempts: HOOK_MAX_ATTEMPTS,
            bearer: token
                .map(|token| BearerToken::new(&token))
                .transpose()
                .map_err(|error| invalid(error.to_string()))?,
            client,
            signing_secret,
        };
        config
            .validate()
            .map_err(|error| invalid(error.to_string()))?;
        Ok(config)
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
                    Arc::new(rushls_common::tls::IgnoreTlsEvents),
                )
                .map_err(|error| invalid(format!("{label}: {error}")))?,
            ),
            (None, None) => None,
            (Some(_), None) => {
                return Err(invalid(format!(
                    "{label} sets client_cert without client_key"
                )));
            }
            (None, Some(_)) => {
                return Err(invalid(format!(
                    "{label} sets client_key without client_cert"
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
        return Err(invalid("ingest.stall_timeout must be nonzero"));
    }
    // Sampling cannot observe a deadline shorter than its own period, so such
    // a value is not the tighter detection it looks like.
    let interval = node.session.supervision.health_interval;
    if stall < interval {
        return Err(invalid(format!(
            "ingest.stall_timeout ({stall:?}) is shorter than the {interval:?} health interval, so it \
             cannot be observed"
        )));
    }
    // A stall shorter than one segment fails a publisher that is merely
    // between keyframes.
    if stall < segment_duration {
        return Err(invalid(format!(
            "ingest.stall_timeout ({stall:?}) is shorter than the {segment_duration:?} segment duration, \
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

    let ingest_is_public = public(&node.rtmp_address)
        || node.rtmps_address.is_some_and(|address| public(&address))
        || public(&node.srt_address)
        || node.moq_address.is_some_and(|address| public(&address));
    if open_admission && ingest_is_public {
        warnings.push(
            "an ingest listener is on a public address with no [publish.auth]: anyone who can \
             reach it may publish"
                .to_owned(),
        );
    }
    if node.store.maximum_streams >= usize::MAX / 2 {
        warnings.push("limits.streams is effectively uncapped".to_owned());
    }
    if node.session.supervision.health.stall == Duration::MAX {
        warnings.push(
            "ingest.stall_timeout is off: a publisher that goes quiet holds its stream name until it \
             disconnects"
                .to_owned(),
        );
    }
    warnings
}

/// Who may publish and what they may send.
///
/// The table itself is the default profile. `[publish.profile.<name>]` holds
/// named alternatives an admission service may select, and `[publish.auth]`
/// configures that service. A profile uses exactly these keys, so reading one
/// table answers what it admits.
#[derive(Conf)]
#[conf(serde)]
pub struct PublishAppConfig {
    /// Reject timing violations and dependent segment starts.
    /// False permits bounded, reported gap recovery.
    #[conf(parameter, long, env, default_value = "true")]
    strict: bool,
    /// Whether a second publisher may replace the one holding a stream name.
    ///
    /// Refusal by default, because silent replacement turns an encoder
    /// reconnect or a leaked credential into a hijack with no signal. The cost
    /// is a reconnect blackout after a half-open socket, bounded by
    /// `ingest.stall_timeout`.
    #[conf(parameter, long, env, default_value = "false")]
    takeover: bool,
    /// Media pace as multiples of wall clock:
    /// `{ max = "1x", burst = "10s", min = "0.5x", window = "30s" }`.
    ///
    /// `max` throttles a publisher running ahead of realtime, allowing `burst`
    /// of head start. `min` disconnects one averaging slower than it across
    /// any `window`. Omit either half, or the whole value, for no limit.
    #[conf(parameter, long, env, value_parser = TomlValue::<PublishRateValue>::from_str)]
    rate: Option<TomlValue<PublishRateValue>>,
    /// Video predicates: codecs, resolution, frame_rate, tracks.
    #[conf(parameter, long, env, value_parser = TomlValue::<VideoAcceptValue>::from_str)]
    video: Option<TomlValue<VideoAcceptValue>>,
    /// Audio predicates: codecs, sample_rate, channels, tracks.
    #[conf(parameter, long, env, value_parser = TomlValue::<AudioAcceptValue>::from_str)]
    audio: Option<TomlValue<AudioAcceptValue>>,
    /// Subtitle predicates: codecs, tracks.
    #[conf(parameter, long, env, value_parser = TomlValue::<SubtitleAcceptValue>::from_str)]
    subtitles: Option<TomlValue<SubtitleAcceptValue>>,
    /// Named alternatives, selected by an auth response `{"profile": "<name>"}`.
    #[conf(parameter, value_parser = TomlTable::<ProfileValue>::from_str)]
    profile: Option<TomlTable<ProfileValue>>,
    /// External admission service. When omitted, anyone who can reach an
    /// ingest listener may publish under the default profile.
    #[conf(flatten, prefix)]
    auth: Option<HttpAuthAppConfig>,
}

impl PublishAppConfig {
    /// The default profile, and every named alternative resolved beside it.
    fn resolve(&self) -> Result<(StreamPolicy, BTreeMap<String, StreamPolicy>), ConfigError> {
        let default = ProfileValue {
            strict: Some(self.strict),
            takeover: Some(self.takeover),
            rate: self.rate.as_ref().map(|value| value.0.clone()),
            video: self.video.as_ref().map(|value| value.0.clone()),
            audio: self.audio.as_ref().map(|value| value.0.clone()),
            subtitles: self.subtitles.as_ref().map(|value| value.0.clone()),
        }
        .resolve("publish")?;
        let mut profiles = BTreeMap::new();
        for (name, configured) in self
            .profile
            .as_ref()
            .map(|table| &table.0)
            .into_iter()
            .flatten()
        {
            if name.trim().is_empty() {
                return Err(invalid("a publish profile name must not be empty"));
            }
            // A profile replaces `[publish]` wholesale rather than inheriting
            // from it: reading one table must answer what it admits, without
            // replaying a merge against another.
            profiles.insert(
                name.clone(),
                configured.resolve(&format!("publish.profile.{name}"))?,
            );
        }
        Ok((default, profiles))
    }
}

/// How many publishers and streams this node holds at once.
#[derive(Conf)]
#[conf(serde)]
pub struct LimitsAppConfig {
    /// Concurrent ingest sessions.
    #[conf(parameter, long, env, default_value = "256")]
    publishers: usize,
    /// Streams held at once: live ones, plus ended ones still inside
    /// `hls.window`. When full, a new stream name is refused.
    ///
    /// Separate from `publishers` because they answer different questions: a
    /// publisher is an ingest session, a stream is a named presentation in the
    /// store. Once the window is long the two decouple.
    #[conf(parameter, long, env, default_value = "1024")]
    streams: usize,
    /// Publishers one client address may hold at once, across every ingest
    /// protocol and including those still being admitted. Omit for no limit.
    ///
    /// `publishers` is node-wide, and a connection takes a slot before
    /// admission finishes, so one host that keeps connecting and stalling
    /// can fill them all even with `[publish.auth]` configured. IPv6 counts
    /// per `/64`. Behind an RTMP proxy, enable `ingest.rtmp.proxy_protocol`
    /// or every RTMP publisher shares the proxy's address.
    #[conf(parameter, long, env)]
    publishers_per_address: Option<usize>,
}

impl LimitsAppConfig {
    fn apply(&self, node: &mut NodeConfig) -> Result<(), ConfigError> {
        if self.publishers == 0 {
            return Err(invalid("limits.publishers must be at least one"));
        }
        if self.streams == 0 {
            return Err(invalid("limits.streams must be at least one"));
        }
        node.maximum_sessions = self.publishers;
        node.store.maximum_streams = self.streams;
        node.maximum_publishers_per_address = self
            .publishers_per_address
            .map(|maximum| {
                std::num::NonZeroUsize::new(maximum).ok_or_else(|| {
                    invalid(
                        "limits.publishers_per_address must be at least one; omit it for no limit",
                    )
                })
            })
            .transpose()?;
        if let Some(maximum) = node.maximum_publishers_per_address
            && maximum.get() > self.publishers
        {
            return Err(invalid(format!(
                "limits.publishers_per_address ({maximum}) exceeds limits.publishers ({}), \
                 so it could never apply",
                self.publishers
            )));
        }
        Ok(())
    }
}

/// Memory budgets. Worst case is `limits.publishers × per_publisher +
/// limits.streams × per_stream`, capped by `total` when it is set.
#[derive(Conf)]
#[conf(serde)]
pub struct MemoryAppConfig {
    /// Memory this node may commit across publishers and streams, or
    /// "unlimited".
    ///
    /// Each active publisher commits `per_publisher` and each stored stream
    /// `per_stream`. A publication that would take the committed total past
    /// this is refused, so the budgets below can never add up to more.
    #[conf(
        parameter,
        long,
        env,
        default_value = "unlimited",
        value_parser = parse_optional_bytes,
        serde(use_value_parser)
    )]
    total: OptionalBytes,
    /// Media one publisher holds in flight before storage, or "unlimited".
    /// Minimum 64MiB, which fits a maximum-sized packet and its output.
    #[conf(
        parameter,
        long,
        env,
        default_value = "128MiB",
        value_parser = parse_optional_bytes,
        serde(use_value_parser)
    )]
    per_publisher: OptionalBytes,
    /// Stored media and cached playlists for one stream. With `[disk]`, one
    /// eighth is kept for playlists and older media spills to disk.
    #[conf(
        parameter,
        long,
        env,
        default_value = "512MiB",
        serde(use_value_parser)
    )]
    per_stream: ByteSize,
}

impl MemoryAppConfig {
    fn apply(&self, node: &mut NodeConfig) -> Result<(), ConfigError> {
        let per_stream = nonzero_bytes("memory.per_stream", self.per_stream)?;
        node.store.retention.maximum_payload_bytes = per_stream;
        let per_publisher = self
            .per_publisher
            .0
            .map(|limit| nonzero_bytes("memory.per_publisher", limit))
            .transpose()?;
        if per_publisher.is_some_and(|bytes| bytes < crate::domain::PipelineBudget::MIN_LIMIT) {
            return Err(invalid(
                "memory.per_publisher must be at least 64MiB to fit a maximum-sized packet and its output",
            ));
        }
        node.session.memory_per_publisher = per_publisher;
        node.memory_total = match self.total.0 {
            None => None,
            Some(total) => {
                let total = nonzero_bytes("memory.total", total)?;
                // Committing an unbounded publisher against a bounded total
                // would make the total a fiction.
                let Some(per_publisher) = per_publisher else {
                    return Err(invalid(
                        "memory.total needs a finite memory.per_publisher to commit per publisher",
                    ));
                };
                if total < per_publisher.saturating_add(per_stream) {
                    return Err(invalid(format!(
                        "memory.total ({}) cannot hold one publisher and its stream \
                         (per_publisher + per_stream = {})",
                        ByteSize::b(total as u64),
                        ByteSize::b(per_publisher.saturating_add(per_stream) as u64),
                    )));
                }
                Some(total)
            }
        };
        Ok(())
    }
}

/// Optional disk overflow: older window media moves here once memory is full.
#[derive(Conf)]
#[conf(serde)]
pub struct DiskAppConfig {
    /// Disk for one stream's older media. Setting it enables spilling; omit
    /// to stay in memory.
    #[conf(parameter, long, env, serde(use_value_parser))]
    per_stream: Option<ByteSize>,
    /// Directory for spilled media. Defaults to the platform cache.
    #[conf(parameter, long, env)]
    dir: Option<PathBuf>,
}

impl DiskAppConfig {
    fn apply(&self, node: &mut NodeConfig) -> Result<(), ConfigError> {
        node.store.disk = match (&self.per_stream, &self.dir) {
            (None, None) => None,
            (Some(bytes), directory) => Some(DiskLimits {
                directory: match directory {
                    Some(directory) => directory.clone(),
                    None => paths::default_disk_directory().ok_or_else(|| {
                        invalid(
                            "could not determine a cache directory for disk overflow; set disk.dir",
                        )
                    })?,
                },
                maximum_payload_bytes: nonzero_bytes("disk.per_stream", *bytes)?,
            }),
            (None, Some(_)) => {
                return Err(invalid("disk.dir needs disk.per_stream to enable spilling"));
            }
        };
        Ok(())
    }
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

/// `rate = { max = "1x", burst = "10s", min = "0.5x", window = "30s" }`.
///
/// One value rather than separate ceiling and floor tables: both halves are
/// the same kind of quantity, and they constrain each other.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishRateValue {
    /// Long-run ceiling a publisher is throttled to.
    max: Option<PaceValue>,
    /// Head start `max` allows, and the most media time idle can bank.
    burst: Option<String>,
    /// Floor below which a publisher is disconnected.
    min: Option<PaceValue>,
    /// Averaging window for `min`. The first one is startup grace.
    window: Option<String>,
}

impl PublishRateValue {
    fn resolve(&self) -> Result<(Option<Ceiling>, Option<Floor>), String> {
        let duration = |label: &str, value: &str| {
            humantime::parse_duration(value).map_err(|error| format!("rate.{label} {error}"))
        };
        let ceiling = match (self.max, &self.burst) {
            (Some(max), burst) => Some(Ceiling {
                pace: max.0,
                // No head start: `max` is permission to continue, not to lead.
                burst: burst
                    .as_deref()
                    .map(|burst| duration("burst", burst))
                    .transpose()?
                    .unwrap_or(Duration::ZERO),
            }),
            (None, Some(_)) => return Err("rate.burst needs rate.max".into()),
            (None, None) => None,
        };
        let floor = match (self.min, &self.window) {
            (Some(min), Some(window)) => {
                let window = duration("window", window)?;
                if window.is_zero() {
                    return Err("rate.window must be nonzero".into());
                }
                Some(Floor {
                    pace: min.0,
                    window,
                })
            }
            (Some(_), None) => {
                return Err("rate.min needs rate.window to average over".into());
            }
            (None, Some(_)) => return Err("rate.window needs rate.min".into()),
            (None, None) => None,
        };
        // A ceiling holding a publisher at exactly its floor makes ordinary
        // jitter fatal, and no value of the pair is usable, so this is a
        // refusal rather than a warning.
        if let (Some(ceiling), Some(floor)) = (ceiling, floor)
            && !floor.pace.is_slower_than(ceiling.pace)
        {
            return Err(
                "rate.min must be slower than rate.max, or the ceiling holds the publisher \
                 at exactly the floor and ordinary jitter trips it"
                    .into(),
            );
        }
        Ok((ceiling, floor))
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

/// One complete publish profile, whether the default or a named alternative.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileValue {
    strict: Option<bool>,
    takeover: Option<bool>,
    rate: Option<PublishRateValue>,
    video: Option<VideoAcceptValue>,
    audio: Option<AudioAcceptValue>,
    subtitles: Option<SubtitleAcceptValue>,
}

impl ProfileValue {
    fn input_mode(&self) -> crate::domain::InputMode {
        if self.strict.unwrap_or(true) {
            crate::domain::InputMode::Strict
        } else {
            crate::domain::InputMode::Permissive
        }
    }

    fn resolve(&self, name: &str) -> Result<StreamPolicy, ConfigError> {
        let where_ = |error: String| invalid(format!("{name}: {error}"));
        let mut policy = StreamPolicy::permissive();
        policy.input_mode = self.input_mode();

        if let Some(rate) = &self.rate {
            (policy.ceiling, policy.floor) = rate.resolve().map_err(where_)?;
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
                    &[Codec::Aac, Codec::Opus, Codec::Flac],
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

/// How publishers connect: one sub-table per protocol, with the liveness
/// deadlines every protocol shares.
#[derive(Conf)]
#[conf(serde)]
pub struct IngestAppConfig {
    /// How long an established connection may carry nothing before it is
    /// closed, or "off" on a trusted link.
    ///
    /// One value for every protocol. The handshake gets its own, tighter
    /// limit, derived rather than configured: a handshake covers an
    /// unauthenticated peer, which is the cheapest way to hold a socket open,
    /// while an established session covers a publisher that has proved
    /// itself, where a tight limit drops a legitimate stream between
    /// keyframes. Exposing both would invite a generous session limit being
    /// applied to unauthenticated peers. SRT requires this to exceed its
    /// receive latency.
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = parse_optional_duration,
        serde(use_value_parser)
    )]
    idle_timeout: OptionalDuration,
    /// How long a connected publisher may deliver no usable media before it
    /// is dropped, or "off".
    ///
    /// Unlike `idle_timeout`, bytes may still be arriving: keepalives,
    /// metadata, or media that cannot be used. Must cover one segment, or a
    /// publisher between keyframes is dropped.
    #[conf(
        parameter,
        long,
        env,
        default_value = "12s",
        value_parser = parse_optional_duration,
        serde(use_value_parser)
    )]
    stall_timeout: OptionalDuration,
    #[conf(flatten, prefix)]
    rtmp: RtmpAppConfig,
    #[conf(flatten, prefix)]
    rtmps: RtmpsAppConfig,
    #[conf(flatten, prefix)]
    srt: SrtAppConfig,
    #[conf(flatten, prefix)]
    moq: MoqAppConfig,
}

/// The unauthenticated phase is never more patient than a few seconds, however
/// generous an established session is allowed to be.
const MAXIMUM_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

impl IngestAppConfig {
    fn apply(
        &self,
        node: &mut NodeConfig,
        tls: Option<&TlsFiles>,
        warnings: &mut Vec<String>,
    ) -> Result<(), ConfigError> {
        let idle = self.idle_timeout.0;
        // A session read shorter than a keyframe interval drops publishers
        // mid-GOP. Nothing here knows the publisher's cadence, so this only
        // catches values too small to be deliberate.
        if let Some(idle) = idle
            && idle < Duration::from_secs(1)
        {
            return Err(invalid(format!(
                "ingest.idle_timeout ({idle:?}) is below one second, which drops publishers \
                 between ordinary keyframes"
            )));
        }
        // The lesser of the session limit and the constant: a disabled session
        // timeout still leaves the handshake bounded, because an
        // unauthenticated peer should never be trusted to hold a socket.
        let handshake = idle.map_or(MAXIMUM_HANDSHAKE_TIMEOUT, |idle| {
            idle.min(MAXIMUM_HANDSHAKE_TIMEOUT)
        });
        if idle.is_none() {
            // Legitimate on a trusted link and a liability on a public one;
            // nothing here can tell which, so say so.
            warnings.push(
                "ingest.idle_timeout is off: a publisher that stops responding holds its \
                 connection until it is closed from the other end"
                    .to_owned(),
            );
        }

        node.rtmp.timeouts = RtmpTimeouts {
            handshake_read: Some(handshake),
            session_read: idle,
            write: idle,
        };
        self.srt.apply(node, idle)?;
        self.rtmps.apply(node, handshake, tls)?;
        self.moq.apply(node, idle, handshake, tls)?;
        Ok(())
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct RtmpAppConfig {
    /// Address receiving RTMP publishers.
    #[conf(parameter, long, env, default_value = "0.0.0.0:1935")]
    pub listen: SocketAddr,
    /// Require a PROXY protocol header (v1 or v2) naming the client on every
    /// connection. Only for a listener that nothing but the proxy can reach.
    ///
    /// Behind a TLS terminator or load balancer, the socket's peer is the
    /// proxy, so admission, logs, hooks, and `limits.publishers_per_address`
    /// would all see one address for every publisher. The header is
    /// mandatory once enabled: an optional one would let any client that
    /// reaches the port directly claim to be anyone.
    #[conf(parameter, long, env, default_value = "false")]
    pub proxy_protocol: bool,
}

/// RTMP inside TLS, terminated by this node with the `[tls]` certificate.
#[derive(Conf)]
#[conf(serde)]
pub struct RtmpsAppConfig {
    /// Address receiving RTMPS publishers, or "off". Needs `[tls]`, which is
    /// why it is off by default.
    ///
    /// A listener of its own rather than TLS detection on the RTMP port:
    /// encoders pick the transport from the URL scheme, and a separate port
    /// lets a firewall expose only the encrypted one.
    #[conf(
        parameter,
        long,
        env,
        default_value = "off",
        value_parser = parse_optional_address,
        serde(use_value_parser)
    )]
    pub listen: OptionalAddress,
    /// Require a PROXY protocol header (v1 or v2) before the TLS handshake on
    /// every connection, for a TCP load balancer that passes TLS through.
    /// Only for a listener that nothing but the proxy can reach.
    #[conf(parameter, long, env, default_value = "false")]
    pub proxy_protocol: bool,
}

impl RtmpsAppConfig {
    fn apply(
        &self,
        node: &mut NodeConfig,
        handshake: Duration,
        tls: Option<&TlsFiles>,
    ) -> Result<(), ConfigError> {
        node.rtmps_address = self.listen.0;
        node.rtmps_proxy_protocol = self.proxy_protocol;
        if node.rtmps_address.is_some() {
            let tls =
                tls.ok_or_else(|| invalid("ingest.rtmps needs [tls] with a certificate and key"))?;
            node.rtmps_tls = Some(TlsSettings {
                certificate: tls.cert.clone(),
                key: tls.key.clone(),
                // Encoders embed their own TLS stacks and update them slowly,
                // so TLS 1.2 stays accepted here whatever HTTPS requires.
                min_version: TlsVersion::Tls12,
                max_version: TlsVersion::Tls13,
                handshake_timeout: handshake,
                maximum_pending_handshakes: 256,
            });
        }
        Ok(())
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct SrtAppConfig {
    /// Address receiving SRT publishers. SRT currently requires IPv4.
    #[conf(parameter, long, env, default_value = "0.0.0.0:9000")]
    pub listen: SocketAddr,
    /// SRT receive latency; increase for unstable or long-distance networks.
    #[conf(
        parameter,
        long,
        env,
        default_value = "120ms",
        value_parser = rushls_common::config::parse_duration,
        serde(use_value_parser)
    )]
    latency: Duration,
    /// Optional passphrase: inline, `${VAR}`, or `{ file = "/path" }`.
    /// Absent accepts unencrypted SRT.
    #[conf(parameter, env, secret)]
    passphrase: Option<TextSource>,
    /// File holding `passphrase`; the command-line form is always a path.
    #[conf(parameter, long = "passphrase", serde(skip))]
    passphrase_file: Option<PathBuf>,
    /// Encryption strength used when a passphrase is configured.
    #[conf(
        parameter,
        long,
        env,
        default_value = "aes256",
        serde(use_value_parser)
    )]
    encryption: SrtKeyLengthValue,
}

impl SrtAppConfig {
    fn apply(&self, node: &mut NodeConfig, idle: Option<Duration>) -> Result<(), ConfigError> {
        if self.latency.is_zero() {
            return Err(invalid("ingest.srt.latency must be nonzero"));
        }
        // An idle deadline inside the receiver's own latency window would fire
        // on packets the transport is still legitimately waiting to reorder.
        if let Some(idle) = idle
            && idle <= self.latency
        {
            return Err(invalid(format!(
                "ingest.idle_timeout ({idle:?}) must exceed ingest.srt.latency ({:?})",
                self.latency
            )));
        }
        let passphrase = read_credential(
            "the SRT passphrase",
            self.passphrase.as_ref(),
            self.passphrase_file.as_ref(),
        )?;
        node.srt.latency = self.latency;
        // SRT always keeps a liveness deadline; "off" makes it unreachable.
        node.srt.peer_idle_timeout = idle.unwrap_or(Duration::MAX);
        node.srt.encryption = passphrase
            .map(|passphrase| SrtEncryption::new(passphrase, self.encryption.into()))
            .transpose()
            .map_err(ConfigError::SrtEncryption)?;
        Ok(())
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct MoqAppConfig {
    /// Address receiving WebTransport publishers, or "off". Needs `[tls]`:
    /// WebTransport has no cleartext form, which is why it is off by default.
    #[conf(
        parameter,
        long,
        env,
        default_value = "off",
        value_parser = parse_optional_address,
        serde(use_value_parser)
    )]
    pub listen: OptionalAddress,
}

impl MoqAppConfig {
    fn apply(
        &self,
        node: &mut NodeConfig,
        idle: Option<Duration>,
        handshake: Duration,
        tls: Option<&TlsFiles>,
    ) -> Result<(), ConfigError> {
        node.moq_address = self.listen.0;
        node.moq.idle_timeout = idle;
        node.moq.handshake_timeout = handshake;
        if node.moq_address.is_some() {
            let tls =
                tls.ok_or_else(|| invalid("ingest.moq needs [tls] with a certificate and key"))?;
            // handshake_timeout / maximum_pending_handshakes are unused by the
            // QUIC endpoint: idle and pending-publisher budgets live on
            // MoqConfig and IngestListener. They exist because TlsSettings is
            // shared with the HTTPS listener.
            node.moq.tls = Some(TlsSettings {
                certificate: tls.cert.clone(),
                key: tls.key.clone(),
                handshake_timeout: handshake,
                maximum_pending_handshakes: 256,
                ..TlsSettings::default()
            });
        }
        Ok(())
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct HlsAppConfig {
    /// Publish keyframe playlists for fast seeking and scrubbing through CMAF video.
    #[conf(
        parameter,
        long,
        env,
        default_if_missing = "true",
        default_value = "true"
    )]
    scrubbing: bool,
    /// Segment cadence: `"6s"`, or `{ target = "6s", max = "2x", tolerance = "0s" }`.
    ///
    /// `max` is the largest segment admission may accept, including rounding
    /// and both tolerance endpoints. `tolerance` is how far a boundary may
    /// move either way at runtime. Both take durations or multiples of target.
    #[conf(
        parameter,
        long,
        env,
        default_value = "6s",
        value_parser = CadenceValue::from_str,
    )]
    segment: CadenceValue,
    /// Part cadence: `"1s"`, or `{ target = "1s", max = "2x" }`.
    #[conf(
        parameter,
        long,
        env,
        default_value = "1s",
        value_parser = CadenceValue::from_str,
    )]
    part: CadenceValue,
    /// How much completed media each live playlist offers: the DVR window.
    ///
    /// A fixed duration (`"18s"`) or a multiple of the segment duration
    /// (`"6x"`), which adapts to `segment_duration`. The media playlist must
    /// never drop below three times the target duration
    /// (draft-pantos-hls-rfc8216bis-22, section 6.2.1), so a fixed window
    /// shorter than that is refused. `memory.per_stream` and `disk` can
    /// shorten it for high-bitrate streams.
    #[conf(
        parameter,
        long,
        env,
        default_value = "6x",
        value_parser = parse_duration_rule,
        serde(use_value_parser)
    )]
    window: DurationRule,
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
        value_parser = parse_duration_rule,
        serde(use_value_parser)
    )]
    hold_back: DurationRule,
}

/// A segment or part cadence: `"6s"`, or
/// `{ target = "6s", max = "2x", tolerance = "0s" }`.
///
/// The short form is the common case; the table adds admission ceilings.
/// Preferences are distinct from the immutable contract admission selects.
#[derive(Clone, Copy, Debug)]
pub struct CadenceValue {
    /// Preferred duration; omitted in the table form keeps the default.
    target: Option<Duration>,
    /// Largest admitted value; defaults to twice the target.
    max: Option<DurationRule>,
    /// Runtime boundary tolerance either way; segments only.
    tolerance: Option<DurationRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CadenceTable {
    target: Option<String>,
    max: Option<String>,
    tolerance: Option<String>,
}

impl TryFrom<CadenceTable> for CadenceValue {
    type Error = String;

    fn try_from(table: CadenceTable) -> Result<Self, Self::Error> {
        Ok(Self {
            target: table
                .target
                .as_deref()
                .map(|target| {
                    humantime::parse_duration(target).map_err(|error| format!("target: {error}"))
                })
                .transpose()?,
            max: table.max.as_deref().map(parse_duration_rule).transpose()?,
            tolerance: table
                .tolerance
                .as_deref()
                .map(parse_duration_rule)
                .transpose()?,
        })
    }
}

impl FromStr for CadenceValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim_start().starts_with('{') {
            return TomlValue::<CadenceTable>::from_str(value)
                .map_err(|error| error.message().to_owned())?
                .0
                .try_into();
        }
        humantime::parse_duration(value.trim())
            .map(|target| Self {
                target: Some(target),
                max: None,
                tolerance: None,
            })
            .map_err(|error| error.to_string())
    }
}

impl<'de> Deserialize<'de> for CadenceValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            Short(String),
            Full(CadenceTable),
        }
        match Shape::deserialize(deserializer)? {
            Shape::Short(value) => value.parse(),
            Shape::Full(table) => table.try_into(),
        }
        .map_err(serde::de::Error::custom)
    }
}

impl CadenceValue {
    const SEGMENT: Duration = Duration::from_secs(6);
    const PART: Duration = Duration::from_secs(1);

    fn target_or(&self, default: Duration) -> Duration {
        self.target.unwrap_or(default)
    }

    /// The default ceiling: twice the target.
    fn max(&self) -> DurationRule {
        self.max.unwrap_or(DurationRule::MultipleOfTarget(
            TargetDurationMultiple::integer(2),
        ))
    }
}

/// Unlike retention minima, an admission ceiling must never silently saturate.
fn resolve_hls_rule(rule: DurationRule, target: Duration) -> Result<Duration, ConfigError> {
    match rule {
        DurationRule::Fixed(duration) => Ok(duration),
        DurationRule::MultipleOfTarget(multiple) => {
            let nanos = target
                .as_nanos()
                .checked_mul(u128::from(multiple.numerator()))
                .map(|n| n.div_ceil(u128::from(multiple.denominator().get())))
                .filter(|n| *n <= Duration::MAX.as_nanos())
                .ok_or_else(|| invalid("HLS duration rule overflows"))?;
            Ok(Duration::new(
                u64::try_from(nanos / 1_000_000_000)
                    .map_err(|_| invalid("HLS duration rule overflows"))?,
                u32::try_from(nanos % 1_000_000_000).expect("subsecond nanoseconds fit u32"),
            ))
        }
    }
}

impl HlsAppConfig {
    /// The segment duration other sections size their relative values by.
    fn segment_duration(&self) -> Duration {
        self.segment.target_or(CadenceValue::SEGMENT)
    }

    fn apply(&self, node: &mut NodeConfig, warnings: &mut Vec<String>) -> Result<(), ConfigError> {
        if self.part.tolerance.is_some() {
            return Err(invalid(
                "hls.part has no tolerance: parts follow the segment boundaries",
            ));
        }
        let segment = self.segment_duration();
        let part = self.part.target_or(CadenceValue::PART);
        let maximum_segment = resolve_hls_rule(self.segment.max(), segment)?;
        let maximum_part = resolve_hls_rule(self.part.max(), part)?;
        let tolerance = resolve_hls_rule(
            self.segment
                .tolerance
                .unwrap_or(DurationRule::Fixed(Duration::ZERO)),
            segment,
        )?;
        node.session.segmentation = SegmentationPolicy {
            desired_segment_duration: segment,
            desired_part_duration: part,
            maximum_segment_duration: maximum_segment,
            maximum_part_duration: maximum_part,
            early_boundary: tolerance,
            late_boundary: tolerance,
        };
        node.session
            .segmentation
            .validate()
            .map_err(|error| invalid(error.to_string()))?;
        let window = self.window;
        // Validate fixed delivery settings for every contract admission can select.
        let maximum_target = Duration::from_secs(
            u64::try_from(maximum_segment.as_nanos().saturating_add(500_000_000) / 1_000_000_000)
                .unwrap_or(u64::MAX)
                .max(1),
        );
        if window.resolve(maximum_target) < maximum_target.saturating_mul(3) {
            return Err(invalid(
                "hls.window must be at least three times the maximum segment duration",
            ));
        }
        // The advertised window and the retention window are one quantity:
        // media a playlist does not name is media no player can request.
        node.store.retention.retain = window;
        // Two thresholds, because the specification has two. Below three parts
        // is a SHOULD, so it is warned: a deployment on a good network may
        // genuinely want the latency, and refusing would deny a legitimate
        // choice. Below two parts is a MUST, so it is refused — a value the
        // protocol forbids cannot be honoured approximately, and advertising
        // it anyway makes clients stall.
        let hold_back = self.hold_back.resolve(maximum_part);
        if hold_back < maximum_part.saturating_mul(2) {
            return Err(invalid(format!(
                "HLS hold_back ({hold_back:?}) must be at least twice the maximum part duration ({maximum_part:?}): \
                 a client given less than two parts of head start runs out of buffered media \
                 on any loss"
            )));
        }
        if hold_back < maximum_part.saturating_mul(3) {
            warnings.push(format!(
                "hls.hold_back ({hold_back:?}) is below the three part durations HLS \
                 recommends; clients on a lossy link may stall at the live edge"
            ));
        }
        node.hls.timing.part_hold_back = self.hold_back;
        node.hls.playlist.iframe_playlists = self.scrubbing;
        Ok(())
    }
}

/// Parses a duration rule: a multiple of the applicable target (`"6x"`) or
/// a fixed duration (`"18s"`).
///
/// The `x` suffix is the multiple form, so the two can never be confused
/// with each other or with a bare count.
fn parse_duration_rule(value: &str) -> Result<DurationRule, String> {
    if let Some(multiple) = value.strip_suffix('x') {
        let (numerator, denominator) = decimal_fraction(multiple)?;
        if numerator == 0 {
            return Err("a duration multiple must be nonzero".into());
        }
        let denominator = NonZeroU32::new(denominator)
            .ok_or_else(|| "a duration multiple must be nonzero".to_owned())?;
        return Ok(DurationRule::MultipleOfTarget(TargetDurationMultiple::new(
            numerator,
            denominator,
        )));
    }
    humantime::parse_duration(value)
        .map(DurationRule::Fixed)
        .map_err(|error| format!("invalid duration rule: {error}"))
}

/// Parses a decimal into a reduced fraction, so `"1.5"` becomes `3/2`.
///
/// Bounded to nine fractional digits so the denominator always fits a `u32`.
fn decimal_fraction(value: &str) -> Result<(u32, u32), String> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
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
    /// Maximum established HTTP connections across all HTTP listeners.
    #[conf(parameter, long, env, default_value = "4096")]
    max_connections: usize,
    /// Maximum HTTP requests executing or streaming responses across all listeners.
    #[conf(parameter, long, env, default_value = "4096")]
    max_requests: usize,

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
}

impl HttpAppConfig {
    fn resolve(
        &self,
        https: Option<&HttpsAppConfig>,
        tls: Option<&TlsFiles>,
    ) -> Result<HttpConfig, ConfigError> {
        let https = https
            .map(|https| {
                let tls =
                    tls.ok_or_else(|| invalid("[https] needs [tls] with a certificate and key"))?;
                https.resolve(tls)
            })
            .transpose()?;
        let config = HttpConfig {
            limits: crate::server::http::HttpLimits {
                maximum_connections: self.max_connections,
                maximum_requests: self.max_requests,
            },
            cors: self.cors.resolve()?,
            tls_address: https.as_ref().map(|(address, _)| *address),
            tls: https.map(|(_, settings)| settings),
            // Filled in by the caller, which is the only place that knows
            // whether the MOQ listener is on.
            moq_certificate: None,
        };
        config.validate().map_err(invalid)?;
        Ok(config)
    }
}

/// File input is a list; shell input uses a comma-separated value.
/// Keep validation in resolution so all sources report the same invalid-name error.
#[derive(Clone, Debug, Deserialize)]
#[serde(transparent)]
struct HeaderNames(Vec<String>);

impl FromStr for HeaderNames {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(if value.trim().is_empty() {
            Vec::new()
        } else {
            value.split(',').map(str::to_owned).collect()
        }))
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
    /// Response header names visible to cross-origin players.
    /// TOML uses an array; CLI and environment values use comma-separated names.
    /// Replaces the default list; an empty list exposes only browser-safelisted headers.
    #[conf(
        parameter,
        long,
        env,
        default_value = "content-length,content-range,date"
    )]
    expose_headers: HeaderNames,
    /// Permit cookies or browser authorization on cross-origin requests.
    #[conf(parameter, long, env, default_value = "false")]
    credentials: bool,
    /// How long browsers may cache a successful CORS preflight.
    #[conf(
        parameter,
        long,
        env,
        default_value = "10min",
        value_parser = rushls_common::config::parse_duration,
        serde(use_value_parser)
    )]
    max_age: Duration,
}

impl CorsAppConfig {
    fn resolve(&self) -> Result<CorsConfig, ConfigError> {
        Ok(CorsConfig {
            allowed_origins: self.origins.resolve()?,
            expose_headers: self
                .expose_headers
                .0
                .iter()
                .map(|name| {
                    name.trim().parse().map_err(|_| {
                        invalid(format!(
                            "http.cors.expose_headers contains an invalid header name: {name:?}"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            allow_credentials: self.credentials,
            max_age: self.max_age,
        })
    }
}

/// The HTTPS listener. Its certificate and key come from `[tls]`.
#[derive(Conf)]
#[conf(serde)]
pub struct HttpsAppConfig {
    /// Accepted protocol range: `{ min = "1.3", max = "1.3" }`. Set min to
    /// "1.2" for older clients.
    #[conf(flatten, prefix)]
    version: TlsVersionAppConfig,
    /// Address serving HTTPS, bound independently of the cleartext listener.
    #[conf(parameter, long, env, default_value = "[::]:8443")]
    listen: SocketAddr,
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
        value_parser = rushls_common::config::parse_duration,
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
    max_handshakes: usize,
}

#[derive(Conf)]
#[conf(serde)]
pub struct TlsVersionAppConfig {
    /// Lowest accepted HTTPS version: "1.2" or "1.3".
    #[conf(parameter, long, env, default_value = "1.3", serde(use_value_parser))]
    min: TlsVersion,
    /// Highest accepted HTTPS version: "1.2" or "1.3".
    #[conf(parameter, long, env, default_value = "1.3", serde(use_value_parser))]
    max: TlsVersion,
}

impl HttpsAppConfig {
    fn resolve(&self, tls: &TlsFiles) -> Result<(SocketAddr, TlsSettings), ConfigError> {
        if self.version.min > self.version.max {
            return Err(ConfigError::Invalid(
                "https.version.min must not exceed https.version.max".to_owned(),
            ));
        }
        if self.max_handshakes == 0 {
            return Err(ConfigError::Invalid(
                "https.max_handshakes must be at least one, or no \
                 TLS connection can be admitted"
                    .to_owned(),
            ));
        }

        Ok((
            self.listen,
            TlsSettings {
                certificate: tls.cert.clone(),
                key: tls.key.clone(),
                handshake_timeout: self.handshake_timeout,
                maximum_pending_handshakes: self.max_handshakes,
                min_version: self.version.min,
                max_version: self.version.max,
            },
        ))
    }
}

/// One certificate for every TLS listener this node runs.
#[derive(Conf)]
#[conf(serde)]
pub struct TlsAppConfig {
    /// Path to a PEM certificate chain, leaf first. Reloaded on rotation.
    #[conf(parameter, long, env)]
    cert: PathBuf,
    /// Path to the PEM private key for that chain.
    #[conf(parameter, long, env)]
    key: PathBuf,
}

/// Resolved `[tls]` paths, shared by HTTPS, RTMPS, and MoQ ingest.
pub struct TlsFiles {
    cert: PathBuf,
    key: PathBuf,
}

impl TlsAppConfig {
    fn resolve(&self) -> TlsFiles {
        TlsFiles {
            cert: self.cert.clone(),
            key: self.key.clone(),
        }
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
    /// Bearer token required to scrape `/metrics` and `/metrics/streams`:
    /// inline, `${VAR}`, or `{ file = "/path" }`.
    #[conf(parameter, env, secret)]
    token: Option<TextSource>,
    /// File holding `token`; the command-line form is always a path.
    #[conf(parameter, long = "token", serde(skip))]
    token_file: Option<PathBuf>,
}

impl MetricsAppConfig {
    fn resolve(self) -> Result<MetricsConfig, ConfigError> {
        let token = read_credential(
            "the metrics token",
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

#[derive(Conf)]
#[conf(serde)]
pub struct LogAppConfig {
    /// How much Rushls itself logs: error, warn, info, debug, or trace.
    /// Dependencies stay at warn.
    ///
    /// A level rather than a filter directive, because this is the operator's
    /// question — what is Rushls doing — and a bare "debug" applied to every
    /// crate would bury the answer under QUIC and TLS internals. `RUST_LOG`,
    /// when set, replaces the whole filter with its own directives, which is
    /// the escape hatch for debugging a dependency; `--log-level` still
    /// beats it.
    #[conf(parameter, long, env, default_value = "info", serde(use_value_parser))]
    level: LogLevel,
    /// "text" for people, "json" (one object per line) for log collectors.
    #[conf(parameter, long, env, default_value = "text", serde(use_value_parser))]
    format: LogFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
#[display(rename_all = "lowercase")]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl FromStr for LogLevel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        one_of(
            &[
                Self::Error,
                Self::Warn,
                Self::Info,
                Self::Debug,
                Self::Trace,
            ],
            value,
            "log level (directives belong in RUST_LOG)",
        )
    }
}

impl LogAppConfig {
    /// Dependencies' warnings are real problems — a certificate that will not
    /// load, a peer breaking protocol — so they stay visible at every level.
    const DEPENDENCIES: &str = "warn";

    fn resolve(&self, env: &[(OsString, OsString)]) -> Result<LogSettings, ConfigError> {
        let rust_log = env
            .iter()
            .rev()
            .find(|(name, _)| name == "RUST_LOG")
            .map(|(_, value)| value.to_string_lossy().trim().to_owned())
            .filter(|value| !value.is_empty());
        let Some(filter) = rust_log else {
            // `rushls` matches by prefix, so it covers `rushls_common` as well as
            // the binary.
            return Ok(LogSettings {
                filter: format!("{},rushls={}", Self::DEPENDENCIES, self.level),
                format: self.format,
            });
        };
        // Refused at startup: a typo would otherwise silently log at some
        // other level, which is exactly when an operator needs logs.
        tracing_subscriber::EnvFilter::builder()
            .parse(&filter)
            .map_err(|error| {
                invalid(format!("RUST_LOG {filter:?} is not a log filter: {error}"))
            })?;
        Ok(LogSettings {
            filter,
            format: self.format,
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
    Flac,
    Av1,
    H264,
    Hevc,
    Opus,
    SubRip,
    Text,
    WebVtt,
}

impl CodecValue {
    const ALL: [Self; 9] = [
        Self::Aac,
        Self::Flac,
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
            CodecValue::Flac => Self::Flac,
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

// Labels belong to this app; reading inline or mounted text is shared.
fn read_text(label: &str, source: Option<&TextSource>) -> Result<Option<String>, ConfigError> {
    source
        .map(|source| source.read(label))
        .transpose()
        .map_err(ConfigError::from)
}

/// Reads a credential that also has a command-line flag.
///
/// Arguments are visible to every local user through the process list, so
/// the flag only ever names a file; it is never the credential itself. It
/// sits at the CLI's place in the precedence order, above TOML and the
/// environment.
fn read_credential(
    label: &str,
    source: Option<&TextSource>,
    cli_file: Option<&PathBuf>,
) -> Result<Option<String>, ConfigError> {
    rushls_common::config::read_credential(label, source, cli_file.map(PathBuf::as_path))
        .map_err(ConfigError::from)
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
            "{policy}: {label} contains {codec:?}, which is not valid for that media kind"
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

mod paths;

#[cfg(test)]
mod tests;
