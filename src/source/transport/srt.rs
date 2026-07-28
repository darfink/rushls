//! Native SRT listener and container-agnostic AVFormat source.
//!
//! SRT message boundaries are transport details. The accepted socket is read
//! as one ordered byte stream by the existing blocking AVFormat worker, so
//! MPEG-TS, Matroska, and any other probed container follow the same source
//! path. SRT delivery timestamps are intentionally ignored; embedded container
//! timestamps remain the media clock.
//!
//! Callers normally use `publish:<namespace/name>:<credential>`, or
//! `publish:<credential>` when one key identifies both the presented resource
//! and credential. Hardware requiring the SRT access-control convention can
//! instead use `#!::u=<credential>,r=<namespace/name>,m=publish,t=stream`.
//! Transport encryption is configured separately and is never admission.

use std::{
    net::SocketAddr,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use tokio::sync::mpsc;

use crate::{
    admission::{ClientInfo, IngestProtocol, PublishGrant, PublishRequest},
    domain::BoxFuture,
    observe::SourceMeters,
    source::{
        AcceptedPublish, InputLimits, InputState, PendingPublish, PublishRejection, TransportError,
        avformat::{
            AvformatConfig, AvformatInput, AvformatInputError, AvformatInterrupt,
            AvformatPacketSource,
        },
    },
};

mod native;
mod stream_id;

const LIBSRT_MINIMUM_VERSION: u32 = 0x01_05_05;
const PEER_MINIMUM_VERSION: i32 = 0x01_03_00;
const SRT_MAXIMUM_LIVE_PAYLOAD_BYTES: usize = 1_456;
const SRT_MAXIMUM_STREAM_ID_BYTES: usize = 512;
const LOSS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SrtKeyLength {
    Aes128,
    Aes192,
    Aes256,
}

impl SrtKeyLength {
    const fn bytes(self) -> i32 {
        match self {
            Self::Aes128 => 16,
            Self::Aes192 => 24,
            Self::Aes256 => 32,
        }
    }
}

#[derive(Clone, derive_more::Debug, Eq, PartialEq)]
#[debug("SrtEncryption {{ key_length: {key_length:?}, passphrase: [REDACTED] }}")]
pub struct SrtEncryption {
    #[debug(skip)]
    passphrase: Box<str>,
    key_length: SrtKeyLength,
}

impl SrtEncryption {
    pub fn new(
        passphrase: impl Into<Box<str>>,
        key_length: SrtKeyLength,
    ) -> Result<Self, TransportError> {
        let passphrase = passphrase.into();
        if !(10..=79).contains(&passphrase.len()) {
            return Err(invalid_request(
                "the SRT passphrase must contain 10 to 79 UTF-8 bytes",
            ));
        }
        Ok(Self {
            passphrase,
            key_length,
        })
    }
}

/// Transport and demux policy for one SRT listener.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SrtConfig {
    /// Receiver latency used by SRT's timestamp-based packet delivery.
    pub latency: Duration,
    pub peer_idle_timeout: Duration,
    /// Polling interval used by native I/O to observe AVFormat cancellation.
    pub receive_poll_interval: Duration,
    pub receive_buffer_bytes: NonZeroUsize,
    /// Largest complete SRT live message accepted from one caller.
    pub maximum_message_bytes: NonZeroUsize,
    pub maximum_stream_id_bytes: NonZeroUsize,
    pub encryption: Option<SrtEncryption>,
    pub avformat: AvformatConfig,
    pub input_limits: InputLimits,
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            latency: Duration::from_millis(120),
            peer_idle_timeout: Duration::from_secs(5),
            receive_poll_interval: Duration::from_millis(25),
            receive_buffer_bytes: nz::usize!(16 * 1024 * 1024),
            maximum_message_bytes: nz::usize!(SRT_MAXIMUM_LIVE_PAYLOAD_BYTES),
            maximum_stream_id_bytes: nz::usize!(SRT_MAXIMUM_STREAM_ID_BYTES),
            encryption: None,
            avformat: AvformatConfig::default(),
            input_limits: InputLimits::permissive(),
        }
    }
}

impl SrtConfig {
    fn validate(&self) -> Result<(), TransportError> {
        if native::version() < LIBSRT_MINIMUM_VERSION {
            return Err(invalid_request("libSRT 1.5.5 or newer is required"));
        }
        if self.latency.is_zero()
            || self.peer_idle_timeout.is_zero()
            || self.receive_poll_interval.is_zero()
        {
            return Err(invalid_request("SRT timing values must be nonzero"));
        }
        if self.maximum_message_bytes.get() > SRT_MAXIMUM_LIVE_PAYLOAD_BYTES {
            return Err(invalid_request(
                "maximum SRT message size exceeds the live-mode payload ceiling",
            ));
        }
        if self.maximum_stream_id_bytes.get() > SRT_MAXIMUM_STREAM_ID_BYTES {
            return Err(invalid_request(
                "maximum SRT Stream ID size exceeds the protocol ceiling",
            ));
        }
        if self.receive_buffer_bytes.get() < self.maximum_message_bytes.get() {
            return Err(invalid_request(
                "the SRT receive buffer must fit one maximum-sized message",
            ));
        }
        self.native_options().map(|_| ())
    }

    fn native_options(&self) -> Result<native::NativeOptions<'_>, TransportError> {
        let (passphrase, key_length) =
            self.encryption
                .as_ref()
                .map_or((None, SrtKeyLength::Aes128.bytes()), |encryption| {
                    (
                        Some(encryption.passphrase.as_bytes()),
                        encryption.key_length.bytes(),
                    )
                });
        Ok(native::NativeOptions {
            latency_ms: duration_millis(self.latency, "SRT latency")?,
            peer_idle_timeout_ms: duration_millis(self.peer_idle_timeout, "SRT peer idle timeout")?,
            receive_buffer_bytes: i32::try_from(self.receive_buffer_bytes.get())
                .map_err(|_| invalid_request("the SRT receive buffer is too large"))?,
            payload_size: i32::try_from(self.maximum_message_bytes.get())
                .expect("validated SRT payload fits i32"),
            minimum_peer_version: PEER_MINIMUM_VERSION,
            passphrase,
            key_length,
        })
    }
}

/// A process listener whose blocking native accept runs outside Tokio.
pub struct SrtListener {
    local_address: SocketAddr,
    incoming: mpsc::Receiver<Result<SrtPendingPublish, TransportError>>,
    socket: Arc<native::Socket>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SrtListener {
    pub fn bind(
        address: SocketAddr,
        config: SrtConfig,
        backlog: usize,
    ) -> Result<Self, TransportError> {
        config.validate()?;
        if backlog == 0 {
            return Err(invalid_request("the SRT listener backlog must be nonzero"));
        }

        let listener = native::open_listener(address, &config.native_options()?, backlog)
            .map_err(|error| TransportError::Handshake(error.to_string().into()))?;
        let local_address = listener.local_address;
        let socket = listener.socket;
        let poll = listener.poll;
        let stopping = Arc::new(AtomicBool::new(false));
        let (sender, incoming) = mpsc::channel(backlog);
        let thread_socket = Arc::clone(&socket);
        let thread_poll = Arc::clone(&poll);
        let thread_stopping = Arc::clone(&stopping);
        let thread = std::thread::Builder::new()
            .name("rushls-srt-listener".into())
            .spawn(move || {
                while !thread_stopping.load(Ordering::Acquire) {
                    match native::wait(&thread_poll, config.receive_poll_interval) {
                        Ok(native::Wait::Timeout) => {}
                        Ok(native::Wait::Ready) => {
                            let connection = match native::accept(&thread_socket) {
                                Ok(connection) => connection,
                                Err(error) => {
                                    if sender
                                        .blocking_send(Err(TransportError::Handshake(
                                            error.to_string().into(),
                                        )))
                                        .is_err()
                                    {
                                        break;
                                    }
                                    continue;
                                }
                            };
                            let pending = SrtPendingPublish::new(connection, config.clone());
                            if sender.blocking_send(pending).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            if sender
                                .blocking_send(Err(TransportError::Handshake(
                                    error.to_string().into(),
                                )))
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|error| {
                socket.close();
                TransportError::Handshake(
                    format!("could not start the SRT listener thread: {error}").into(),
                )
            })?;

        Ok(Self {
            local_address,
            incoming,
            socket,
            stopping,
            thread: Some(thread),
        })
    }

    pub fn local_address(&self) -> SocketAddr {
        self.local_address
    }

    pub async fn accept(&mut self) -> Option<Result<SrtPendingPublish, TransportError>> {
        self.incoming.recv().await
    }
}

impl Drop for SrtListener {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.socket.close();
    }
}

/// An established SRT connection awaiting application admission.
pub struct SrtPendingPublish {
    request: PublishRequest,
    socket: Arc<native::Socket>,
    config: SrtConfig,
}

impl SrtPendingPublish {
    fn new(connection: native::Connection, config: SrtConfig) -> Result<Self, TransportError> {
        let parsed = stream_id::parse(&connection.stream_id, config.maximum_stream_id_bytes.get())
            .map_err(TransportError::InvalidPublishRequest)?;
        Ok(Self {
            request: PublishRequest {
                protocol: IngestProtocol::Srt,
                resource: parsed.resource,
                credential: parsed.credential,
                client: ClientInfo {
                    remote_address: connection.remote_address,
                    encoder: None,
                    protocol_version: Some(format_version(connection.protocol_version)),
                },
            },
            socket: connection.socket,
            config,
        })
    }
}

impl PendingPublish for SrtPendingPublish {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        Ok(self.request.clone())
    }

    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(async move {
            let Self { socket, config, .. } = *self;
            let input = SrtInput::new(
                socket,
                config.maximum_message_bytes.get(),
                config.receive_poll_interval,
                Arc::clone(&meters),
            );
            let source = AvformatPacketSource::new(
                Box::new(input),
                config.avformat,
                config.input_limits,
                meters,
            )
            .map_err(|error| TransportError::Accept(error.to_string().into()))?;
            Ok(AcceptedPublish {
                source: Box::new(source),
                grant,
            })
        })
    }

    fn reject(
        self: Box<Self>,
        _rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        Box::pin(async move {
            self.socket.close();
            Ok(())
        })
    }
}

struct SrtInput {
    socket: Arc<native::Socket>,
    scratch: Vec<u8>,
    poll_interval: Duration,
    pending_start: usize,
    pending_end: usize,
    meters: Arc<dyn SourceMeters>,
    last_loss_total: u64,
    next_loss_sample: Instant,
}

impl SrtInput {
    fn new(
        socket: Arc<native::Socket>,
        maximum_message_bytes: usize,
        poll_interval: Duration,
        meters: Arc<dyn SourceMeters>,
    ) -> Self {
        let last_loss_total = native::receive_loss_total(&socket).unwrap_or(0);
        Self {
            socket,
            scratch: vec![0; maximum_message_bytes],
            poll_interval,
            pending_start: 0,
            pending_end: 0,
            meters,
            last_loss_total,
            next_loss_sample: Instant::now() + LOSS_SAMPLE_INTERVAL,
        }
    }

    fn copy_pending(&mut self, output: &mut [u8]) -> usize {
        let available = self.pending_end - self.pending_start;
        let copied = available.min(output.len());
        output[..copied]
            .copy_from_slice(&self.scratch[self.pending_start..self.pending_start + copied]);
        self.pending_start += copied;
        if self.pending_start == self.pending_end {
            self.pending_start = 0;
            self.pending_end = 0;
        }
        copied
    }

    fn record_transport_loss(&mut self, force: bool) {
        if !force && Instant::now() < self.next_loss_sample {
            return;
        }
        if let Some(total) = native::receive_loss_total(&self.socket) {
            let lost = total.saturating_sub(self.last_loss_total);
            self.last_loss_total = total;
            if lost > 0 {
                self.meters.source_progress(0, 0, lost);
            }
        }
        self.next_loss_sample = Instant::now() + LOSS_SAMPLE_INTERVAL;
    }
}

impl AvformatInput for SrtInput {
    fn read(
        &mut self,
        output: &mut [u8],
        interrupt: &dyn AvformatInterrupt,
    ) -> Result<usize, AvformatInputError> {
        if output.is_empty() {
            return Ok(0);
        }
        if interrupt.interrupted() {
            return Err(AvformatInputError::End(InputState::Interrupted));
        }
        if self.pending_start != self.pending_end {
            return Ok(self.copy_pending(output));
        }

        loop {
            if interrupt.interrupted() {
                return Err(AvformatInputError::End(InputState::Interrupted));
            }

            let receive_into_output = output.len() >= self.scratch.len();
            let result = if receive_into_output {
                let maximum = self.scratch.len();
                native::receive(&self.socket, &mut output[..maximum])
            } else {
                native::receive(&self.socket, &mut self.scratch)
            };
            match result {
                Ok(native::Receive::Data(received)) => {
                    self.record_transport_loss(false);
                    if receive_into_output {
                        return Ok(received);
                    }
                    self.pending_end = received;
                    return Ok(self.copy_pending(output));
                }
                Ok(native::Receive::Retry) => {
                    self.record_transport_loss(false);
                    std::thread::sleep(self.poll_interval);
                }
                Ok(native::Receive::End) => {
                    self.record_transport_loss(true);
                    return Err(AvformatInputError::End(InputState::Closed));
                }
                Err(error) if native::ended_cleanly(&self.socket) => {
                    self.record_transport_loss(true);
                    return Err(AvformatInputError::End(InputState::Closed));
                }
                Err(_error) if native::is_broken(&self.socket) => {
                    self.record_transport_loss(true);
                    return Err(AvformatInputError::End(InputState::Interrupted));
                }
                Err(error) => {
                    self.record_transport_loss(true);
                    return Err(AvformatInputError::Failed(error.to_string().into()));
                }
            }
        }
    }
}

impl Drop for SrtInput {
    fn drop(&mut self) {
        self.socket.close();
    }
}

fn duration_millis(duration: Duration, field: &'static str) -> Result<i32, TransportError> {
    let milliseconds = duration.as_nanos().div_ceil(1_000_000);
    i32::try_from(milliseconds)
        .ok()
        .filter(|milliseconds| *milliseconds > 0)
        .ok_or_else(|| {
            TransportError::InvalidPublishRequest(format!("{field} is out of range").into())
        })
}

fn format_version(version: u32) -> String {
    format!(
        "{}.{}.{}",
        (version >> 16) & 0xff,
        (version >> 8) & 0xff,
        version & 0xff
    )
}

fn invalid_request(message: &'static str) -> TransportError {
    TransportError::InvalidPublishRequest(message.into())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use crate::{
        admission::{Principal, PublishGrant, StreamPolicy},
        domain::{StreamId, TrackCounts},
        observe::{ProcessMeters, SessionMeters},
        source::DiscoveryLimits,
    };

    use super::*;

    struct NeverInterrupt(AtomicBool);

    impl AvformatInterrupt for NeverInterrupt {
        fn interrupted(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn grant() -> PublishGrant {
        PublishGrant {
            stream_id: StreamId::new("live/camera"),
            principal: Principal("publisher".into()),
            policy: StreamPolicy::permissive(),
        }
    }

    #[test]
    fn invalid_transport_limits_are_rejected_before_binding() {
        let config = SrtConfig {
            maximum_message_bytes: nz::usize!(1_457),
            ..SrtConfig::default()
        };
        assert!(config.validate().is_err());

        let config = SrtConfig {
            receive_buffer_bytes: nz::usize!(1_000),
            ..SrtConfig::default()
        };
        assert!(config.validate().is_err());

        assert!(SrtEncryption::new("short", SrtKeyLength::Aes256).is_err());
    }

    #[tokio::test]
    async fn a_live_message_survives_partial_avformat_reads() {
        let config = SrtConfig {
            encryption: Some(
                SrtEncryption::new("test-secret", SrtKeyLength::Aes256)
                    .expect("test passphrase is valid"),
            ),
            ..SrtConfig::default()
        };
        let mut listener = SrtListener::bind(
            "127.0.0.1:0".parse().expect("constant is valid"),
            config.clone(),
            4,
        )
        .expect("listener binds");
        let address = listener.local_address();
        let caller_config = config.clone();
        let caller = std::thread::spawn(move || {
            let options = caller_config.native_options().expect("options are valid");
            let socket =
                native::test_connect(address, &options, "secret").expect("test caller connects");
            native::test_send(&socket, b"abcdefg").expect("message sends");
            socket
        });
        let pending = listener
            .accept()
            .await
            .expect("listener remains open")
            .expect("connection is accepted");
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut input = SrtInput::new(
            pending.socket,
            1_456,
            config.receive_poll_interval,
            meters.source_view(),
        );
        let interrupt = NeverInterrupt(AtomicBool::new(false));
        let mut first = [0; 3];
        let mut second = [0; 4];

        assert_eq!(input.read(&mut first, &interrupt), Ok(3));
        assert_eq!(input.read(&mut second, &interrupt), Ok(4));
        assert_eq!(&first, b"abc");
        assert_eq!(&second, b"defg");

        caller.join().expect("caller did not panic").close();
    }

    #[tokio::test]
    async fn matroska_over_srt_reuses_the_real_avformat_source() {
        let config = SrtConfig::default();
        let mut listener = SrtListener::bind(
            "127.0.0.1:0".parse().expect("constant is valid"),
            config.clone(),
            4,
        )
        .expect("listener binds");
        let address = listener.local_address();
        let caller = std::thread::spawn(move || {
            let options = config.native_options().expect("options are valid");
            let socket = native::test_connect(address, &options, "publish:live/camera:secret")
                .expect("test caller connects");
            let fixture = crate::source::avformat::fixtures::primed_aac_mkv();
            for message in fixture.chunks(1_316) {
                native::test_send(&socket, message).expect("fixture message sends");
            }
            socket
        });

        let pending = listener
            .accept()
            .await
            .expect("listener remains open")
            .expect("connection is accepted");
        let request = pending.publish_request().expect("request is valid");
        assert_eq!(request.protocol, IngestProtocol::Srt);
        assert_eq!(request.resource.namespace.as_deref(), Some("live"));
        assert_eq!(request.resource.name, "camera");
        assert_eq!(request.credential.expose(), b"secret");

        let meters = SessionMeters::new(ProcessMeters::default());
        let mut accepted = Box::new(pending)
            .accept(grant(), meters.source_view())
            .await
            .expect("publication is accepted");
        let caller = caller.join().expect("caller did not panic");
        let discovery = accepted
            .source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 64 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("Matroska is discovered");
        assert_eq!(
            discovery.tracks.counts(),
            TrackCounts {
                audio: 1,
                subtitle: 0,
                video: 0,
            }
        );
        caller.close();

        let mut packets = Vec::new();
        while accepted
            .source
            .fill(&mut packets)
            .await
            .expect("Matroska packets demux")
            .is_open()
        {}
        assert!(!packets.is_empty());
    }
}
