//! Operator-facing configuration and its translation into runtime policy.
//!
//! The types in this module are the stable configuration vocabulary. They are
//! deliberately separate from the internal policy types they build: changing
//! how a session or transport is assembled must not silently rename a TOML key,
//! environment variable, or command-line option.

use std::{
    ffi::{OsStr, OsString},
    fmt, fs,
    net::SocketAddr,
    num::{NonZeroU16, NonZeroU32, NonZeroU64, NonZeroUsize},
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use bytesize::ByteSize;
use conf::{Conf, find_parameter};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    admission::{IngestTimingPolicy, Principal, StreamPolicy, TakeoverPolicy},
    delivery::hls::{
        DurationRule, RetentionPolicy, StoreLimits, TargetDurationMultiple,
        cache_control::CacheControlPolicy,
        project::{DeliveryTimingPolicy, PlaylistPolicy, ProgramDateTimePolicy},
        serve::{DeliveryConfig, PlaylistReadiness},
        uri::UriBase,
    },
    domain::{Codec, FrameRate, StreamId},
    mux::{CmafMuxerConfig, SegmentBoundaryPolicy},
    segment::{BoundarySearchPolicy, PrerollLimits, SegmentationPolicy},
    server::{
        NodeConfig,
        http::{AllowedOrigins, CorsConfig, HttpConfig, TlsSettings},
        metrics::ExportPolicy,
    },
    session::{HealthPolicy, SessionConfig, SupervisionPolicy},
    source::{
        DiscoveryLimits, InputLimits,
        avformat::AvformatConfig,
        transport::{
            rtmp::RtmpConfig,
            srt::{SrtConfig, SrtEncryption, SrtKeyLength},
        },
    },
};

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

/// The complete operator-facing configuration.
///
/// Values resolve in the order `defaults < TOML < environment < CLI`.
#[derive(Conf)]
#[conf(serde, name = "rushls", version, env_prefix = "RUSHLS_")]
pub struct AppConfig {
    /// TOML configuration file to load.
    #[conf(parameter, long, env = "CONFIG", serde(skip))]
    pub config: Option<PathBuf>,

    #[conf(flatten, prefix)]
    pub node: NodeAppConfig,
    #[conf(flatten, prefix)]
    pub auth: AuthAppConfig,
    #[conf(flatten, prefix)]
    pub ingest: IngestAppConfig,
    #[conf(flatten, prefix)]
    pub session: SessionAppConfig,
    #[conf(flatten, prefix)]
    pub packaging: PackagingAppConfig,
    #[conf(flatten, prefix)]
    pub delivery: DeliveryAppConfig,
    #[conf(flatten, prefix)]
    pub http: HttpAppConfig,
    #[conf(flatten, prefix)]
    pub observability: ObservabilityAppConfig,
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

    /// Converts the external vocabulary into the internal runtime policies.
    pub fn resolve(self) -> Result<ResolvedAppConfig, ConfigError> {
        if self.auth.publish_key.is_empty() {
            return Err(invalid("publishing key must not be empty"));
        }

        let stream_policy = self.auth.stream_policy.resolve()?;
        let input = self.session.input.resolve()?;
        let rtmp = self.ingest.rtmp.resolve(input)?;
        let srt = self.ingest.srt.resolve(input)?;
        let session = self.session.resolve(input)?;
        let cmaf = self.packaging.cmaf.resolve()?;
        let store = self.delivery.store.resolve()?;
        let delivery = self.delivery.resolve()?;
        let http = self.http.resolve()?;

        Ok(ResolvedAppConfig {
            node: NodeConfig {
                rtmp_address: self.ingest.rtmp.listen,
                srt_address: self.ingest.srt.listen,
                http_address: self.http.listen,
                maintenance_interval: self.node.maintenance_interval,
                maximum_sessions: self.node.maximum_sessions,
                rtmp,
                srt,
                session,
                cmaf,
                store,
                delivery,
                http,
                metrics: ExportPolicy {
                    per_stream: self.observability.metrics_per_stream,
                },
            },
            publish_key: self.auth.publish_key,
            stream_id: StreamId::new(self.auth.stream_id),
            principal: Principal(self.auth.principal),
            stream_policy,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct NodeAppConfig {
    /// Maximum simultaneous publishing sessions.
    #[conf(parameter, long, env, default_value = "256")]
    pub maximum_sessions: usize,
    /// Interval between store and playlist-cache maintenance passes.
    #[conf(
        parameter,
        long,
        env,
        default_value = "1s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    pub maintenance_interval: Duration,
}

#[derive(Conf)]
#[conf(serde)]
pub struct AuthAppConfig {
    /// Shared credential accepted from publishers.
    #[conf(parameter, env, secret)]
    pub publish_key: String,
    /// Logical stream populated by the fixed publisher.
    #[conf(parameter, long, env, default_value = "live/camera")]
    pub stream_id: String,
    /// Principal name recorded for the fixed publisher.
    #[conf(parameter, long, env, default_value = "configured-publisher")]
    pub principal: String,
    #[conf(flatten, prefix)]
    pub stream_policy: StreamPolicyAppConfig,
}

#[derive(Conf)]
#[conf(serde)]
pub struct StreamPolicyAppConfig {
    /// Whether a newly authenticated publisher may replace the incumbent.
    #[conf(parameter, long, env, default_value = "allow", serde(use_value_parser))]
    takeovers: TakeoversValue,
    /// How ahead-of-realtime input is handled: `pace` or `require-realtime`.
    #[conf(parameter, long, env, default_value = "pace", serde(use_value_parser))]
    ingest_timing: IngestTimingValue,
    /// Lead tolerated without pacing after pre-roll.
    #[conf(
        parameter,
        long,
        env,
        default_value = "2s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    initial_lead: Duration,
    /// Largest forward timestamp discontinuity accepted in paced mode.
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    maximum_timestamp_jump: Duration,
    /// Lead accepted in require-realtime mode.
    #[conf(
        parameter,
        long,
        env,
        default_value = "2s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    maximum_lead: Duration,
    /// Accepted video codecs. Repeat on CLI; comma-separate in the environment.
    #[conf(repeat, long, env, serde(use_value_parser))]
    accepted_video_codecs: Vec<CodecValue>,
    /// Accepted audio codecs. Repeat on CLI; comma-separate in the environment.
    #[conf(repeat, long, env, serde(use_value_parser))]
    accepted_audio_codecs: Vec<CodecValue>,
    /// Accepted subtitle codecs. Repeat on CLI; comma-separate in the environment.
    #[conf(repeat, long, env, serde(use_value_parser))]
    accepted_subtitle_codecs: Vec<CodecValue>,
    #[conf(parameter, long, env, default_value = "8")]
    maximum_audio_tracks: usize,
    #[conf(parameter, long, env, default_value = "8")]
    maximum_subtitle_tracks: usize,
    #[conf(parameter, long, env, default_value = "8")]
    maximum_video_tracks: usize,
    #[conf(parameter, long, env, default_value = "7680")]
    maximum_video_width: NonZeroU32,
    #[conf(parameter, long, env, default_value = "4320")]
    maximum_video_height: NonZeroU32,
    /// Maximum video frame rate as an exact `numerator/denominator`.
    #[conf(parameter, long, env, default_value = "240/1", serde(use_value_parser))]
    maximum_video_frame_rate: FrameRateValue,
    #[conf(parameter, long, env, default_value = "192000")]
    maximum_audio_sample_rate: NonZeroU32,
    #[conf(parameter, long, env, default_value = "32")]
    maximum_audio_channels: NonZeroU16,
}

impl StreamPolicyAppConfig {
    fn resolve(self) -> Result<StreamPolicy, ConfigError> {
        let defaults = StreamPolicy::permissive();
        let accepted_video_codecs = codecs_or_default(
            "accepted video codecs",
            self.accepted_video_codecs,
            defaults.accepted_video_codecs,
            &[Codec::H264, Codec::Hevc, Codec::Av1],
        )?;
        let accepted_audio_codecs = codecs_or_default(
            "accepted audio codecs",
            self.accepted_audio_codecs,
            defaults.accepted_audio_codecs,
            &[Codec::Aac, Codec::Opus],
        )?;
        let accepted_subtitle_codecs = codecs_or_default(
            "accepted subtitle codecs",
            self.accepted_subtitle_codecs,
            defaults.accepted_subtitle_codecs,
            &[Codec::WebVtt, Codec::SubRip],
        )?;
        let ingest_timing = match self.ingest_timing {
            IngestTimingValue::Pace => IngestTimingPolicy::PaceToRealtime {
                initial_lead: self.initial_lead,
                maximum_timestamp_jump: self.maximum_timestamp_jump,
            },
            IngestTimingValue::RequireRealtime => IngestTimingPolicy::RequireRealtime {
                maximum_lead: self.maximum_lead,
            },
        };

        Ok(StreamPolicy {
            takeovers: match self.takeovers {
                TakeoversValue::Allow => TakeoverPolicy::Allow,
                TakeoversValue::Deny => TakeoverPolicy::Deny,
            },
            ingest_timing,
            accepted_video_codecs,
            accepted_audio_codecs,
            accepted_subtitle_codecs,
            maximum_audio_tracks: self.maximum_audio_tracks,
            maximum_subtitle_tracks: self.maximum_subtitle_tracks,
            maximum_video_tracks: self.maximum_video_tracks,
            maximum_video_width: self.maximum_video_width,
            maximum_video_height: self.maximum_video_height,
            maximum_video_frame_rate: self.maximum_video_frame_rate.0,
            maximum_audio_sample_rate: self.maximum_audio_sample_rate,
            maximum_audio_channels: self.maximum_audio_channels,
        })
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
    #[conf(parameter, long, env, default_value = "0.0.0.0:1935")]
    pub listen: SocketAddr,
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    maximum_publish_wait: Duration,
    #[conf(parameter, long, env, default_value = "16MiB", serde(use_value_parser))]
    maximum_buffered_flv_bytes: ByteSize,
    #[conf(parameter, long, env, default_value = "8MiB", serde(use_value_parser))]
    maximum_tag_payload_bytes: ByteSize,
    #[conf(flatten, prefix)]
    avformat: AvformatAppConfig,
}

impl RtmpAppConfig {
    fn resolve(&self, input_limits: InputLimits) -> Result<RtmpConfig, ConfigError> {
        Ok(RtmpConfig {
            maximum_publish_wait: self.maximum_publish_wait,
            maximum_buffered_flv_bytes: nonzero_bytes(
                "RTMP maximum buffered FLV bytes",
                self.maximum_buffered_flv_bytes,
            )?,
            maximum_tag_payload_bytes: nonzero_bytes(
                "RTMP maximum tag payload bytes",
                self.maximum_tag_payload_bytes,
            )?,
            avformat: self.avformat.resolve("RTMP")?,
            input_limits,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct SrtAppConfig {
    #[conf(parameter, long, env, default_value = "[::]:9000")]
    pub listen: SocketAddr,
    #[conf(
        parameter,
        long,
        env,
        default_value = "120ms",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    latency: Duration,
    #[conf(
        parameter,
        long,
        env,
        default_value = "5s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    peer_idle_timeout: Duration,
    #[conf(
        parameter,
        long,
        env,
        default_value = "25ms",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    receive_poll_interval: Duration,
    #[conf(parameter, long, env, default_value = "16MiB", serde(use_value_parser))]
    receive_buffer_bytes: ByteSize,
    #[conf(parameter, long, env, default_value = "1456B", serde(use_value_parser))]
    maximum_message_bytes: ByteSize,
    #[conf(parameter, long, env, default_value = "512B", serde(use_value_parser))]
    maximum_stream_id_bytes: ByteSize,
    /// Optional SRT passphrase; absent disables SRT encryption.
    #[conf(parameter, env, secret)]
    passphrase: Option<String>,
    #[conf(
        parameter,
        long,
        env,
        default_value = "aes256",
        serde(use_value_parser)
    )]
    key_length: SrtKeyLengthValue,
    #[conf(flatten, prefix)]
    avformat: AvformatAppConfig,
}

impl SrtAppConfig {
    fn resolve(&self, input_limits: InputLimits) -> Result<SrtConfig, ConfigError> {
        let encryption = self
            .passphrase
            .as_ref()
            .map(|passphrase| SrtEncryption::new(passphrase.clone(), self.key_length.into()))
            .transpose()
            .map_err(ConfigError::SrtEncryption)?;
        Ok(SrtConfig {
            latency: self.latency,
            peer_idle_timeout: self.peer_idle_timeout,
            receive_poll_interval: self.receive_poll_interval,
            receive_buffer_bytes: nonzero_bytes(
                "SRT receive buffer bytes",
                self.receive_buffer_bytes,
            )?,
            maximum_message_bytes: nonzero_bytes(
                "SRT maximum message bytes",
                self.maximum_message_bytes,
            )?,
            maximum_stream_id_bytes: nonzero_bytes(
                "SRT maximum Stream ID bytes",
                self.maximum_stream_id_bytes,
            )?,
            encryption,
            avformat: self.avformat.resolve("SRT")?,
            input_limits,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct AvformatAppConfig {
    #[conf(parameter, long, env, default_value = "32KiB", serde(use_value_parser))]
    io_buffer_size: ByteSize,
    #[conf(parameter, long, env, default_value = "64")]
    packet_channel_capacity: NonZeroUsize,
    #[conf(parameter, long, env, default_value = "16MiB", serde(use_value_parser))]
    maximum_queued_payload_bytes: ByteSize,
}

impl AvformatAppConfig {
    fn resolve(&self, transport: &str) -> Result<AvformatConfig, ConfigError> {
        Ok(AvformatConfig {
            io_buffer_size: nonzero_bytes(
                &format!("{transport} AVFormat I/O buffer size"),
                self.io_buffer_size,
            )?,
            packet_channel_capacity: self.packet_channel_capacity,
            maximum_queued_payload_bytes: nonzero_bytes(
                &format!("{transport} AVFormat maximum queued payload bytes"),
                self.maximum_queued_payload_bytes,
            )?,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct SessionAppConfig {
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    maximum_admission_time: Duration,
    #[conf(flatten, prefix)]
    discovery: DiscoveryAppConfig,
    #[conf(flatten, prefix)]
    input: InputAppConfig,
    #[conf(flatten, prefix)]
    preroll: PrerollAppConfig,
    #[conf(flatten, prefix)]
    segmentation: SegmentationAppConfig,
    #[conf(flatten, prefix)]
    supervision: SupervisionAppConfig,
}

impl SessionAppConfig {
    fn resolve(self, input: InputLimits) -> Result<SessionConfig, ConfigError> {
        Ok(SessionConfig {
            maximum_admission_time: self.maximum_admission_time,
            discovery: self.discovery.resolve()?,
            input,
            preroll: self.preroll.resolve()?,
            segmentation: self.segmentation.resolve()?,
            supervision: self.supervision.resolve(),
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct DiscoveryAppConfig {
    #[conf(parameter, long, env, default_value = "8MiB", serde(use_value_parser))]
    maximum_probe_bytes: ByteSize,
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    maximum_wall_time: Duration,
}

impl DiscoveryAppConfig {
    fn resolve(self) -> Result<DiscoveryLimits, ConfigError> {
        Ok(DiscoveryLimits {
            maximum_probe_bytes: bytes("maximum discovery probe bytes", self.maximum_probe_bytes)?,
            maximum_wall_time: self.maximum_wall_time,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct InputAppConfig {
    #[conf(parameter, long, env, default_value = "4096")]
    maximum_packets_per_batch: usize,
    #[conf(parameter, long, env, default_value = "8MiB", serde(use_value_parser))]
    maximum_payload_bytes_per_packet: ByteSize,
    #[conf(parameter, long, env, default_value = "16MiB", serde(use_value_parser))]
    maximum_payload_bytes_per_batch: ByteSize,
    #[conf(parameter, long, env, default_value = "16384")]
    maximum_samples_per_batch: usize,
    #[conf(
        parameter,
        long,
        env,
        default_value = "12.5MB",
        serde(use_value_parser)
    )]
    maximum_bytes_per_media_second: ByteSize,
    #[conf(parameter, long, env, default_value = "50000")]
    maximum_packets_per_media_second: u64,
    #[conf(parameter, long, env, default_value = "50000")]
    maximum_samples_per_media_second: u64,
    #[conf(
        parameter,
        long,
        env,
        default_value = "1s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    media_density_window: Duration,
}

impl InputAppConfig {
    fn resolve(&self) -> Result<InputLimits, ConfigError> {
        Ok(InputLimits {
            maximum_packets_per_batch: self.maximum_packets_per_batch,
            maximum_payload_bytes_per_packet: bytes(
                "maximum packet payload bytes",
                self.maximum_payload_bytes_per_packet,
            )?,
            maximum_payload_bytes_per_batch: bytes(
                "maximum batch payload bytes",
                self.maximum_payload_bytes_per_batch,
            )?,
            maximum_samples_per_batch: self.maximum_samples_per_batch,
            maximum_bytes_per_media_second: self.maximum_bytes_per_media_second.as_u64(),
            maximum_packets_per_media_second: self.maximum_packets_per_media_second,
            maximum_samples_per_media_second: self.maximum_samples_per_media_second,
            media_density_window: self.media_density_window,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct PrerollAppConfig {
    #[conf(parameter, long, env, default_value = "64MiB", serde(use_value_parser))]
    maximum_buffered_bytes: ByteSize,
    #[conf(parameter, long, env, default_value = "16384")]
    maximum_buffered_samples: usize,
    #[conf(
        parameter,
        long,
        env,
        default_value = "15s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    maximum_wall_time: Duration,
    #[conf(
        parameter,
        long,
        env,
        default_value = "30s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    maximum_media_duration: Duration,
}

impl PrerollAppConfig {
    fn resolve(self) -> Result<PrerollLimits, ConfigError> {
        Ok(PrerollLimits {
            maximum_buffered_bytes: bytes(
                "maximum pre-roll buffered bytes",
                self.maximum_buffered_bytes,
            )?,
            maximum_buffered_samples: self.maximum_buffered_samples,
            maximum_wall_time: self.maximum_wall_time,
            maximum_media_duration: self.maximum_media_duration,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct SegmentationAppConfig {
    #[conf(
        parameter,
        long,
        env,
        default_value = "6s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    desired_segment_duration: Duration,
    #[conf(
        parameter,
        long,
        env,
        default_value = "1s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    desired_part_duration: Duration,
    #[conf(
        parameter,
        long,
        env,
        default_value = "extend-to-next",
        serde(use_value_parser)
    )]
    boundary_search: BoundarySearchValue,
    #[conf(
        parameter,
        long,
        env,
        default_value = "6s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    maximum_search_extension: Duration,
}

impl SegmentationAppConfig {
    fn resolve(self) -> Result<SegmentationPolicy, ConfigError> {
        if self.desired_segment_duration.is_zero() || self.desired_part_duration.is_zero() {
            return Err(invalid(
                "desired segment and part durations must be nonzero",
            ));
        }
        let search = match self.boundary_search {
            BoundarySearchValue::AtOrBeforeDesired => BoundarySearchPolicy::AtOrBeforeDesired,
            BoundarySearchValue::ExtendToNext => BoundarySearchPolicy::ExtendToNext {
                maximum_extension: self.maximum_search_extension,
            },
        };
        Ok(SegmentationPolicy {
            desired_segment_duration: self.desired_segment_duration,
            desired_part_duration: self.desired_part_duration,
            search,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct SupervisionAppConfig {
    #[conf(
        parameter,
        long,
        env,
        default_value = "1s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    health_interval: Duration,
    #[conf(
        parameter,
        long,
        env,
        default_value = "5s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    source_stall_timeout: Duration,
    #[conf(
        parameter,
        long,
        env,
        default_value = "5s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    media_stall_timeout: Duration,
    #[conf(parameter, long, env, default_value = "3")]
    stalled_publication_multiplier: u32,
    #[conf(
        parameter,
        long,
        env,
        default_value = "1s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    minimum_publication_stall_tolerance: Duration,
}

impl SupervisionAppConfig {
    fn resolve(self) -> SupervisionPolicy {
        SupervisionPolicy {
            health: HealthPolicy {
                source_stall_timeout: self.source_stall_timeout,
                media_stall_timeout: self.media_stall_timeout,
                stalled_publication_multiplier: self.stalled_publication_multiplier,
                minimum_publication_stall_tolerance: self.minimum_publication_stall_tolerance,
            },
            health_interval: self.health_interval,
        }
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct PackagingAppConfig {
    #[conf(flatten, prefix)]
    cmaf: CmafAppConfig,
}

#[derive(Conf)]
#[conf(serde)]
pub struct CmafAppConfig {
    #[conf(parameter, long, env, default_value = "32KiB", serde(use_value_parser))]
    io_buffer_size: ByteSize,
    #[conf(
        parameter,
        long,
        env,
        default_value = "strict",
        serde(use_value_parser)
    )]
    segment_boundary: CmafBoundaryValue,
    #[conf(
        parameter,
        long,
        env,
        default_value = "6s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    maximum_boundary_extension: Duration,
}

impl CmafAppConfig {
    fn resolve(self) -> Result<CmafMuxerConfig, ConfigError> {
        let segment_boundary_policy = match self.segment_boundary {
            CmafBoundaryValue::Strict => SegmentBoundaryPolicy::Strict,
            CmafBoundaryValue::ExtendToRandomAccess => {
                SegmentBoundaryPolicy::ExtendToRandomAccess {
                    maximum_extension: self.maximum_boundary_extension,
                }
            }
        };
        Ok(CmafMuxerConfig {
            io_buffer_size: nonzero_bytes("CMAF I/O buffer size", self.io_buffer_size)?,
            segment_boundary_policy,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct DeliveryAppConfig {
    /// Base URI emitted in playlists; empty keeps resource names relative.
    #[conf(parameter, long, env, default_value = "")]
    public_base: String,
    #[conf(
        parameter,
        long,
        env,
        default_value = "completed-segment",
        serde(use_value_parser)
    )]
    playlist_readiness: PlaylistReadinessValue,
    #[conf(flatten, prefix)]
    playlist: PlaylistAppConfig,
    #[conf(flatten, prefix)]
    timing: DeliveryTimingAppConfig,
    #[conf(flatten, prefix)]
    cache: CacheControlAppConfig,
    #[conf(flatten, prefix)]
    store: StoreAppConfig,
}

impl DeliveryAppConfig {
    fn resolve(&self) -> Result<DeliveryConfig, ConfigError> {
        Ok(DeliveryConfig {
            playlist: self.playlist.resolve(),
            timing: self.timing.resolve(),
            readiness: match self.playlist_readiness {
                PlaylistReadinessValue::CompletedSegment => PlaylistReadiness::CompletedSegment,
                PlaylistReadinessValue::AnyMedia => PlaylistReadiness::AnyMedia,
            },
            cache_control: self.cache.resolve(),
            uri_base: UriBase::new(self.public_base.clone()),
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct PlaylistAppConfig {
    #[conf(
        parameter,
        long,
        env,
        default_value = "at-discontinuities",
        serde(use_value_parser)
    )]
    program_date_time: ProgramDateTimeValue,
    #[conf(parameter, long, env, default_value = "6000000")]
    assumed_bandwidth: NonZeroU64,
}

impl PlaylistAppConfig {
    fn resolve(&self) -> PlaylistPolicy {
        PlaylistPolicy {
            program_date_time: match self.program_date_time {
                ProgramDateTimeValue::AtDiscontinuities => ProgramDateTimePolicy::AtDiscontinuities,
                ProgramDateTimeValue::EverySegment => ProgramDateTimePolicy::EverySegment,
            },
            assumed_bandwidth: self.assumed_bandwidth,
        }
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct DeliveryTimingAppConfig {
    #[conf(parameter, long, env, default_value = "3x", serde(use_value_parser))]
    hold_back: TargetMultipleValue,
    #[conf(parameter, long, env, default_value = "3x", serde(use_value_parser))]
    part_hold_back: TargetMultipleValue,
    #[conf(parameter, long, env, default_value = "3x", serde(use_value_parser))]
    blocking_reload: TargetMultipleValue,
    #[conf(parameter, long, env, default_value = "true")]
    can_block_reload: bool,
}

impl DeliveryTimingAppConfig {
    fn resolve(&self) -> DeliveryTimingPolicy {
        DeliveryTimingPolicy {
            hold_back: self.hold_back.0,
            part_hold_back: self.part_hold_back.0,
            blocking_reload: self.blocking_reload.0,
            can_block_reload: self.can_block_reload,
        }
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct CacheControlAppConfig {
    #[conf(parameter, long, env, default_value = "6x", serde(use_value_parser))]
    blocking_playlist: DurationRuleValue,
    #[conf(parameter, long, env, default_value = "1/2x", serde(use_value_parser))]
    playlist: DurationRuleValue,
    #[conf(parameter, long, env, default_value = "6x", serde(use_value_parser))]
    media: DurationRuleValue,
    #[conf(parameter, long, env, default_value = "4x", serde(use_value_parser))]
    blocking_missing: DurationRuleValue,
    #[conf(parameter, long, env, default_value = "1x", serde(use_value_parser))]
    missing: DurationRuleValue,
    #[conf(
        parameter,
        long,
        env,
        default_value = "6s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    assumed_target_duration: Duration,
}

impl CacheControlAppConfig {
    fn resolve(&self) -> CacheControlPolicy {
        CacheControlPolicy {
            blocking_playlist: self.blocking_playlist.0,
            playlist: self.playlist.0,
            media: self.media.0,
            blocking_missing: self.blocking_missing.0,
            missing: self.missing.0,
            assumed_target_duration: self.assumed_target_duration,
        }
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct StoreAppConfig {
    #[conf(parameter, long, env, default_value = "1024")]
    maximum_streams: usize,
    #[conf(
        parameter,
        long,
        env,
        default_value = "30s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    idle_retention: Duration,
    #[conf(flatten, prefix)]
    retention: RetentionAppConfig,
}

impl StoreAppConfig {
    fn resolve(&self) -> Result<StoreLimits, ConfigError> {
        Ok(StoreLimits {
            maximum_streams: self.maximum_streams,
            idle_retention: self.idle_retention,
            retention: self.retention.resolve()?,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct RetentionAppConfig {
    #[conf(parameter, long, env, default_value = "6")]
    minimum_playlist_segments: usize,
    #[conf(parameter, long, env, default_value = "3x", serde(use_value_parser))]
    minimum_playlist_duration: DurationRuleValue,
    #[conf(parameter, long, env, default_value = "3x", serde(use_value_parser))]
    part_tag_retention: DurationRuleValue,
    #[conf(parameter, long, env, default_value = "3x", serde(use_value_parser))]
    part_fetch_grace_period: DurationRuleValue,
    /// Fixed/multiple grace, or `protocol` for the HLS-derived deadline.
    #[conf(
        parameter,
        long,
        env,
        default_value = "protocol",
        serde(use_value_parser)
    )]
    segment_fetch_grace_period: SegmentGraceValue,
    #[conf(
        parameter,
        long,
        env,
        default_value = "512MiB",
        serde(use_value_parser)
    )]
    maximum_payload_bytes: ByteSize,
    #[conf(parameter, long, env, default_value = "16384")]
    maximum_parts: usize,
    #[conf(parameter, long, env, default_value = "4096")]
    maximum_segments: usize,
}

impl RetentionAppConfig {
    fn resolve(&self) -> Result<RetentionPolicy, ConfigError> {
        Ok(RetentionPolicy {
            minimum_playlist_segments: self.minimum_playlist_segments,
            minimum_playlist_duration: self.minimum_playlist_duration.0,
            part_tag_retention: self.part_tag_retention.0,
            part_fetch_grace_period: self.part_fetch_grace_period.0,
            segment_fetch_grace_period: match self.segment_fetch_grace_period {
                SegmentGraceValue::Protocol => None,
                SegmentGraceValue::Rule(rule) => Some(rule),
            },
            maximum_payload_bytes: bytes(
                "maximum retained payload bytes",
                self.maximum_payload_bytes,
            )?,
            maximum_parts: self.maximum_parts,
            maximum_segments: self.maximum_segments,
        })
    }
}

#[derive(Conf)]
#[conf(serde)]
pub struct HttpAppConfig {
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
    /// `*`, `off`, or an explicit list of allowed origins.
    #[conf(
        parameter,
        long,
        env,
        default(OriginsValue::any()),
        default_help_str = "*"
    )]
    origins: OriginsValue,
    #[conf(parameter, long, env, default_value = "false")]
    allow_credentials: bool,
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
        let allowed_origins = self.origins.resolve()?;
        Ok(CorsConfig {
            allowed_origins,
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
    #[conf(
        parameter,
        long,
        env,
        default_value = "10s",
        value_parser = humantime::parse_duration,
        serde(use_value_parser)
    )]
    handshake_timeout: Duration,
    #[conf(parameter, long, env, default_value = "256")]
    maximum_pending_handshakes: usize,
}

impl TlsAppConfig {
    fn resolve(&self) -> Result<TlsSettings, ConfigError> {
        if self.handshake_timeout.is_zero() {
            return Err(invalid("TLS handshake timeout must be nonzero"));
        }
        if self.maximum_pending_handshakes == 0 {
            return Err(invalid("maximum pending TLS handshakes must be nonzero"));
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
pub struct ObservabilityAppConfig {
    /// Include per-stream series in exported metrics.
    #[conf(parameter, long, env, default_value = "false")]
    metrics_per_stream: bool,
}

#[derive(Clone, Copy, Debug)]
struct TargetMultipleValue(TargetDurationMultiple);

impl FromStr for TargetMultipleValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let ratio = value
            .strip_suffix('x')
            .ok_or_else(|| "a target-duration multiple must end in `x`".to_owned())?;
        let (numerator, denominator) = match ratio.split_once('/') {
            Some((numerator, denominator)) => (
                parse_u32("multiple numerator", numerator)?,
                parse_u32("multiple denominator", denominator)?,
            ),
            None => (parse_u32("multiple", ratio)?, 1),
        };
        if numerator == 0 {
            return Err("a target-duration multiple must be positive".into());
        }
        let denominator = NonZeroU32::new(denominator)
            .ok_or_else(|| "a target-duration denominator must be positive".to_owned())?;
        Ok(Self(TargetDurationMultiple::new(numerator, denominator)))
    }
}

impl fmt::Display for TargetMultipleValue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        let numerator = self.0.numerator();
        let denominator = self.0.denominator().get();
        if denominator == 1 {
            write!(output, "{numerator}x")
        } else {
            write!(output, "{numerator}/{denominator}x")
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DurationRuleValue(DurationRule);

impl FromStr for DurationRuleValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.ends_with('x') {
            value
                .parse::<TargetMultipleValue>()
                .map(|multiple| Self(multiple.0.into()))
        } else {
            humantime::parse_duration(value)
                .map(|duration| Self(duration.into()))
                .map_err(|error| error.to_string())
        }
    }
}

impl fmt::Display for DurationRuleValue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            DurationRule::Fixed(duration) => {
                write!(output, "{}", humantime::format_duration(duration))
            }
            DurationRule::MultipleOfTarget(multiple) => TargetMultipleValue(multiple).fmt(output),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum SegmentGraceValue {
    Protocol,
    Rule(DurationRule),
}

impl FromStr for SegmentGraceValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("protocol") {
            Ok(Self::Protocol)
        } else {
            value
                .parse::<DurationRuleValue>()
                .map(|rule| Self::Rule(rule.0))
        }
    }
}

impl fmt::Display for SegmentGraceValue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol => output.write_str("protocol"),
            Self::Rule(rule) => DurationRuleValue(*rule).fmt(output),
        }
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
struct FrameRateValue(FrameRate);

impl FromStr for FrameRateValue {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (numerator, denominator) = value
            .split_once('/')
            .ok_or_else(|| "a frame rate must be `numerator/denominator`".to_owned())?;
        let numerator = NonZeroU32::new(parse_u32("frame-rate numerator", numerator)?)
            .ok_or_else(|| "frame-rate numerator must be positive".to_owned())?;
        let denominator = NonZeroU32::new(parse_u32("frame-rate denominator", denominator)?)
            .ok_or_else(|| "frame-rate denominator must be positive".to_owned())?;
        Ok(Self(FrameRate::new(numerator, denominator)))
    }
}

impl fmt::Display for FrameRateValue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{}/{}", self.0.numerator(), self.0.denominator())
    }
}

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug)]
        enum $name {
            $($variant),+
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok(Self::$variant),)+
                    _ => Err(format!(
                        "expected one of: {}",
                        [$($text),+].join(", ")
                    )),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$variant => output.write_str($text),)+
                }
            }
        }
    };
}

string_enum!(TakeoversValue {
    Allow => "allow",
    Deny => "deny",
});
string_enum!(IngestTimingValue {
    Pace => "pace",
    RequireRealtime => "require-realtime",
});
string_enum!(BoundarySearchValue {
    AtOrBeforeDesired => "at-or-before-desired",
    ExtendToNext => "extend-to-next",
});
string_enum!(CmafBoundaryValue {
    Strict => "strict",
    ExtendToRandomAccess => "extend-to-random-access",
});
string_enum!(PlaylistReadinessValue {
    CompletedSegment => "completed-segment",
    AnyMedia => "any-media",
});
string_enum!(ProgramDateTimeValue {
    AtDiscontinuities => "at-discontinuities",
    EverySegment => "every-segment",
});
string_enum!(SrtKeyLengthValue {
    Aes128 => "aes128",
    Aes192 => "aes192",
    Aes256 => "aes256",
});

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
        ("RUSHLS_PUBLISH_KEY", "AUTH_PUBLISH_KEY"),
        ("RUSHLS_STREAM_ID", "AUTH_STREAM_ID"),
        ("RUSHLS_RTMP_LISTEN", "INGEST_RTMP_LISTEN"),
        ("RUSHLS_SRT_LISTEN", "INGEST_SRT_LISTEN"),
        ("RUSHLS_SRT_PASSPHRASE", "INGEST_SRT_PASSPHRASE"),
        ("RUSHLS_HTTP_LISTEN", "HTTP_LISTEN"),
        ("RUSHLS_TLS_CERT", "HTTP_TLS_CERTIFICATE"),
        ("RUSHLS_TLS_KEY", "HTTP_TLS_KEY"),
        ("RUSHLS_PUBLIC_BASE", "DELIVERY_PUBLIC_BASE"),
        ("RUSHLS_CORS_ORIGINS", "HTTP_CORS_ORIGINS"),
        ("RUSHLS_CORS_CREDENTIALS", "HTTP_CORS_ALLOW_CREDENTIALS"),
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

fn bytes(label: &str, value: ByteSize) -> Result<usize, ConfigError> {
    usize::try_from(value.as_u64()).map_err(|_| {
        invalid(format!(
            "{label} does not fit this platform's address space"
        ))
    })
}

fn nonzero_bytes(label: &str, value: ByteSize) -> Result<NonZeroUsize, ConfigError> {
    NonZeroUsize::new(bytes(label, value)?)
        .ok_or_else(|| invalid(format!("{label} must be nonzero")))
}

fn origins(values: Vec<String>) -> Result<AllowedOrigins, ConfigError> {
    if values.is_empty() {
        Err(invalid("a CORS origin allowlist must not be empty"))
    } else {
        Ok(AllowedOrigins::Only(values))
    }
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

fn parse_u32(label: &str, value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|error| format!("{label} is not an unsigned integer: {error}"))
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(message.into())
}

#[cfg(test)]
mod tests;
