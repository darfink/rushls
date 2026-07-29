//! Operator-facing configuration and its translation into runtime policy.
//!
//! This module describes administrator intent, not the shape of the internal
//! pipeline. Low-level configuration starts from its library defaults and only
//! the deliberately supported operator choices are applied here.

use std::{
    ffi::{OsStr, OsString},
    fmt, fs,
    net::SocketAddr,
    num::NonZeroU32,
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use bytesize::ByteSize;
use conf::{Conf, find_parameter};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    admission::{Principal, StreamPolicy, TakeoverPolicy},
    delivery::hls::uri::UriBase,
    domain::{Codec, FrameRate, StreamId},
    segment::SegmentationPolicy,
    server::{
        NodeConfig,
        http::{AllowedOrigins, CorsConfig, HttpConfig, OriginPattern, TlsSettings},
        metrics::{ExportPolicy, MetricsConfig, MetricsToken},
    },
    source::transport::srt::{SrtEncryption, SrtKeyLength},
};

const CONFIGURED_PUBLISHER: &str = "configured-publisher";

/// Configuration after all external values have been validated and translated.
pub struct ResolvedAppConfig {
    pub node: NodeConfig,
    pub publish_key: String,
    pub stream_id: StreamId,
    pub principal: Principal,
    pub stream_policy: StreamPolicy,
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
#[conf(serde, name = "rushls", version, env_prefix = "RUSHLS_")]
pub struct AppConfig {
    /// TOML configuration file to load.
    #[conf(parameter, long, env = "CONFIG", serde(skip))]
    pub config: Option<PathBuf>,

    #[conf(flatten, prefix)]
    pub server: ServerAppConfig,
    #[conf(flatten, prefix)]
    pub publishing: PublishingAppConfig,
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
        let env = normalize_legacy_env(env.into_iter().collect());
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
        let publishing = self.publishing.resolve()?;
        let mut node = NodeConfig {
            maximum_sessions: self.server.maximum_concurrent_publishers,
            rtmp_address: self.ingest.rtmp.listen,
            srt_address: self.ingest.srt.listen,
            http_address: self.http.listen,
            ..NodeConfig::default()
        };
        self.ingest.srt.apply(&mut node)?;
        self.hls.apply(&mut node)?;
        self.storage.apply(&mut node)?;
        node.http = self.http.resolve()?;
        node.metrics = self.metrics.resolve()?;

        Ok(ResolvedAppConfig {
            node,
            publish_key: publishing.key,
            stream_id: StreamId::new(publishing.stream_id),
            principal: Principal(CONFIGURED_PUBLISHER.into()),
            stream_policy: publishing.policy,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct ServerAppConfig {
    /// Maximum publishers that may be active at the same time.
    #[conf(parameter, long, env, default_value = "256")]
    pub maximum_concurrent_publishers: usize,
}

#[derive(Conf)]
#[conf(serde)]
pub struct PublishingAppConfig {
    /// Shared credential accepted from publishers.
    #[conf(parameter, env, secret)]
    key: String,
    /// HLS stream name populated by the configured publisher.
    #[conf(parameter, long, env, default_value = "live/camera")]
    stream_id: String,
    /// Let a new publisher replace the current publisher of this stream.
    #[conf(parameter, long, env, default_value = "true")]
    allow_takeover: bool,
    #[conf(flatten, prefix)]
    media: MediaPolicyAppConfig,
}

struct ResolvedPublishing {
    key: String,
    stream_id: String,
    policy: StreamPolicy,
}

impl PublishingAppConfig {
    fn resolve(self) -> Result<ResolvedPublishing, ConfigError> {
        if self.key.is_empty() {
            return Err(invalid("publishing key must not be empty"));
        }
        let mut policy = self.media.resolve()?;
        policy.takeovers = if self.allow_takeover {
            TakeoverPolicy::Allow
        } else {
            TakeoverPolicy::Deny
        };
        Ok(ResolvedPublishing {
            key: self.key,
            stream_id: self.stream_id,
            policy,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct MediaPolicyAppConfig {
    /// Video codecs publishers may send. Containers are detected automatically.
    #[conf(repeat, long, env, serde(use_value_parser))]
    video_codecs: Vec<CodecValue>,
    /// Audio codecs publishers may send. Containers are detected automatically.
    #[conf(repeat, long, env, serde(use_value_parser))]
    audio_codecs: Vec<CodecValue>,
    /// Subtitle codecs publishers may send.
    #[conf(repeat, long, env, serde(use_value_parser))]
    subtitle_codecs: Vec<CodecValue>,
    /// Maximum number of simultaneous tracks of each media kind.
    #[conf(parameter, long, env, default_value = "8")]
    maximum_video_tracks: usize,
    /// Maximum simultaneous audio tracks, including alternate languages.
    #[conf(parameter, long, env, default_value = "8")]
    maximum_audio_tracks: usize,
    /// Maximum simultaneous subtitle tracks, including alternate languages.
    #[conf(parameter, long, env, default_value = "8")]
    maximum_subtitle_tracks: usize,
    /// Largest accepted video dimensions, written as `WIDTHxHEIGHT`.
    #[conf(
        parameter,
        long,
        env,
        default_value = "7680x4320",
        serde(use_value_parser)
    )]
    maximum_video_resolution: ResolutionValue,
    /// Largest frame rate, written as an integer or exact fraction.
    #[conf(parameter, long, env, default_value = "240", serde(use_value_parser))]
    maximum_video_frame_rate: FrameRateValue,
}

impl MediaPolicyAppConfig {
    fn resolve(self) -> Result<StreamPolicy, ConfigError> {
        let mut policy = StreamPolicy::permissive();
        policy.accepted_video_codecs = codecs_or_default(
            "video codecs",
            self.video_codecs,
            policy.accepted_video_codecs,
            &[Codec::H264, Codec::Hevc, Codec::Av1],
        )?;
        policy.accepted_audio_codecs = codecs_or_default(
            "audio codecs",
            self.audio_codecs,
            policy.accepted_audio_codecs,
            &[Codec::Aac, Codec::Opus],
        )?;
        policy.accepted_subtitle_codecs = codecs_or_default(
            "subtitle codecs",
            self.subtitle_codecs,
            policy.accepted_subtitle_codecs,
            &[Codec::WebVtt, Codec::SubRip],
        )?;
        policy.maximum_video_tracks = self.maximum_video_tracks;
        policy.maximum_audio_tracks = self.maximum_audio_tracks;
        policy.maximum_subtitle_tracks = self.maximum_subtitle_tracks;
        policy.maximum_video_width = self.maximum_video_resolution.width;
        policy.maximum_video_height = self.maximum_video_resolution.height;
        policy.maximum_video_frame_rate = self.maximum_video_frame_rate.0;
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
        node.srt.latency = self.latency;
        node.srt.encryption = self
            .passphrase
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
    /// Minimum completed segments shown in a live playlist.
    #[conf(parameter, long, env, default_value = "6")]
    playlist_segments: usize,
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
        if self.playlist_segments == 0 {
            return Err(invalid("HLS playlist must retain at least one segment"));
        }
        node.session.segmentation =
            SegmentationPolicy::latency_first(self.segment_duration, self.part_duration);
        node.store.retention.minimum_playlist_segments = self.playlist_segments;
        node.delivery.uri_base = UriBase::new(self.public_base_url.clone());
        Ok(())
    }
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
    /// Address serving HLS and optional metrics.
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
            tls: self.tls.as_ref().map(TlsAppConfig::resolve).transpose()?,
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
    fn resolve(&self) -> Result<TlsSettings, ConfigError> {
        Ok(TlsSettings {
            certificate: self.certificate.clone(),
            key: self.key.clone(),
            ..TlsSettings::default()
        })
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
    /// Include per-stream series. Avoid this with unbounded stream names.
    #[conf(parameter, long, env, default_value = "false")]
    per_stream: bool,
}

impl MetricsAppConfig {
    fn resolve(self) -> Result<MetricsConfig, ConfigError> {
        if self.token.as_ref().is_some_and(String::is_empty) {
            return Err(invalid("metrics token must not be empty"));
        }
        Ok(MetricsConfig {
            enabled: self.enabled,
            token: self.token.map(MetricsToken::new),
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
            Self::Text(value) => origins(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|origin| !origin.is_empty())
                    .map(str::to_owned)
                    .collect(),
            ),
            Self::List(values) => origins(values.clone()),
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

#[derive(Clone, Copy, Debug)]
enum SrtKeyLengthValue {
    Aes128,
    Aes192,
    Aes256,
}

impl FromStr for SrtKeyLengthValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "aes128" => Ok(Self::Aes128),
            "aes192" => Ok(Self::Aes192),
            "aes256" => Ok(Self::Aes256),
            _ => Err("expected one of: aes128, aes192, aes256".into()),
        }
    }
}

impl fmt::Display for SrtKeyLengthValue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(match self {
            Self::Aes128 => "aes128",
            Self::Aes192 => "aes192",
            Self::Aes256 => "aes256",
        })
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

#[derive(Clone, Copy, Debug)]
enum CodecValue {
    Aac,
    Av1,
    H264,
    Hevc,
    Opus,
    SubRip,
    WebVtt,
}

impl FromStr for CodecValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "aac" => Ok(Self::Aac),
            "av1" => Ok(Self::Av1),
            "h264" | "avc" => Ok(Self::H264),
            "hevc" | "h265" => Ok(Self::Hevc),
            "opus" => Ok(Self::Opus),
            "subrip" | "srt" => Ok(Self::SubRip),
            "webvtt" | "vtt" => Ok(Self::WebVtt),
            _ => Err("unsupported codec name".into()),
        }
    }
}

impl fmt::Display for CodecValue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(match self {
            Self::Aac => "aac",
            Self::Av1 => "av1",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Opus => "opus",
            Self::SubRip => "subrip",
            Self::WebVtt => "webvtt",
        })
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
            CodecValue::WebVtt => Self::WebVtt,
        }
    }
}

fn normalize_legacy_env(mut env: Vec<(OsString, OsString)>) -> Vec<(OsString, OsString)> {
    for (legacy, canonical) in [
        ("RUSHLS_PUBLISH_KEY", "PUBLISHING_KEY"),
        ("RUSHLS_AUTH_PUBLISH_KEY", "PUBLISHING_KEY"),
        ("RUSHLS_STREAM_ID", "PUBLISHING_STREAM_ID"),
        ("RUSHLS_AUTH_STREAM_ID", "PUBLISHING_STREAM_ID"),
        ("RUSHLS_RTMP_LISTEN", "INGEST_RTMP_LISTEN"),
        ("RUSHLS_SRT_LISTEN", "INGEST_SRT_LISTEN"),
        ("RUSHLS_SRT_PASSPHRASE", "INGEST_SRT_PASSPHRASE"),
        ("RUSHLS_TLS_CERT", "HTTP_TLS_CERTIFICATE"),
        ("RUSHLS_TLS_KEY", "HTTP_TLS_KEY"),
        ("RUSHLS_PUBLIC_BASE", "HLS_PUBLIC_BASE_URL"),
        ("RUSHLS_DELIVERY_PUBLIC_BASE", "HLS_PUBLIC_BASE_URL"),
        ("RUSHLS_CORS_ORIGINS", "HTTP_CORS_ORIGINS"),
        ("RUSHLS_CORS_CREDENTIALS", "HTTP_CORS_ALLOW_CREDENTIALS"),
        ("RUSHLS_OBSERVABILITY_METRICS_ENABLED", "METRICS_ENABLED"),
        ("RUSHLS_OBSERVABILITY_METRICS_TOKEN", "METRICS_TOKEN"),
        (
            "RUSHLS_OBSERVABILITY_METRICS_PER_STREAM",
            "METRICS_PER_STREAM",
        ),
    ] {
        let canonical = format!("RUSHLS_{canonical}");
        if env_value(&env, &canonical).is_none()
            && let Some(value) = env_value(&env, legacy).map(OsStr::to_owned)
        {
            env.push((canonical.into(), value));
        }
    }
    env
}

fn env_value<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_os_str())
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

fn origins(values: Vec<String>) -> Result<AllowedOrigins, ConfigError> {
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

fn codecs_or_default(
    label: &str,
    configured: Vec<CodecValue>,
    defaults: Vec<Codec>,
    permitted: &[Codec],
) -> Result<Vec<Codec>, ConfigError> {
    if configured.is_empty() {
        return Ok(defaults);
    }
    let configured: Vec<Codec> = configured.into_iter().map(Into::into).collect();
    if let Some(codec) = configured.iter().find(|codec| !permitted.contains(codec)) {
        return Err(invalid(format!(
            "{label} contains {codec:?}, which is not valid for that media kind"
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
