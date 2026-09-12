//! RTMP handshake and native packet source.
//!
//! RTMP media messages are already parsed by `rtmpx`. This adapter maps those
//! tags onto [`crate::source::rtmp::RtmpPacketSource`].

use std::{net::SocketAddr, num::NonZeroUsize, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use rtmpx::{
    EnhancedCapabilities, EnhancedValidationMode, ValidatedMedia, ValidatedMetadata,
    handshake::{Handshake, HandshakeProgress, HandshakeRole},
    sessions::{ServerEvent, ServerOutput, ServerSession, ServerSessionConfig},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    task::JoinHandle,
};

use crate::{
    admission::{
        ClientInfo, IngestProtocol, PresentedCredential, PublishGrant, PublishRequest,
        PublishResource,
    },
    domain::{Appender, BoxFuture},
    observe::SourceMeters,
    source::{
        AcceptedPublish, DiscoveryLimits, DiscoveryReport, InputLimits, InputState, Packet,
        PacketSource, PendingPublish, PublishRejection, SourceError, TransportError,
        rtmp::{IngressEvent, IngressReader, IngressSendError, IngressWriter, RtmpPacketSource},
    },
};

/// Malformed Enhanced FLV framing refuses the publisher, always.
///
/// Not an operator setting. A publisher that cannot describe its own media
/// correctly is refused on the same footing as one that fails the handshake;
/// keeping the bytes as opaque instead only defers the failure to a layer with
/// less context, turning a protocol error into a packaging error. This is
/// separate from `[accept]`, which decides *which* codecs are admitted rather
/// than whether the framing is well-formed at all.
const ENHANCED_VALIDATION: EnhancedValidationMode = EnhancedValidationMode::Strict;

/// Socket deadlines owned by the RTMP transport, independent of protocol state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtmpTimeouts {
    pub handshake_read: Option<Duration>,
    pub session_read: Option<Duration>,
    pub write: Option<Duration>,
}

/// Resource and memory policy for one RTMP connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtmpConfig {
    /// Maximum time from accepting the socket until its first publish command.
    pub maximum_publish_wait: Duration,
    /// Network operation timeouts for the session beneath this connection.
    ///
    /// Distinct from [`Self::maximum_publish_wait`], and both are needed: this
    /// bounds how long a single socket operation may produce nothing, while
    /// `maximum_publish_wait` bounds the whole pre-publish phase. A peer that
    /// dribbles one byte per second defeats the first and is caught by the
    /// second.
    pub timeouts: RtmpTimeouts,
    /// Incomplete RTMP messages the chunk deserializer may reassemble.
    pub maximum_reassembly_bytes: NonZeroUsize,
    /// Encoded access units allowed to wait between the session and the source.
    pub maximum_queued_payload_bytes: NonZeroUsize,
    /// Largest RTMP audio or video message accepted from one publisher.
    pub maximum_message_bytes: NonZeroUsize,
    pub input_limits: InputLimits,
}

impl Default for RtmpConfig {
    fn default() -> Self {
        Self {
            maximum_publish_wait: Duration::from_secs(10),
            // Deliberately looser than the former listener's 2s/2.5s defaults, which
            // are tight enough to drop a legitimate publisher on a poor
            // network between keyframes. The configuration layer resolves
            // these from `peer_timeout`; this value is what a caller
            // constructing the transport directly gets.
            timeouts: RtmpTimeouts {
                handshake_read: Some(Duration::from_secs(10)),
                session_read: Some(Duration::from_secs(10)),
                write: Some(Duration::from_secs(10)),
            },
            maximum_reassembly_bytes: NonZeroUsize::new(crate::source::PipelineMemory::TRANSPORT)
                .expect("the transport budget is nonzero"),
            maximum_queued_payload_bytes: NonZeroUsize::new(
                crate::source::PipelineMemory::DEMUX_QUEUE,
            )
            .expect("the demux queue budget is nonzero"),
            maximum_message_bytes: nz::usize!(8 * 1024 * 1024),
            input_limits: InputLimits::permissive(),
        }
    }
}

impl RtmpConfig {
    fn validate(self) -> Result<Self, TransportError> {
        if self.maximum_publish_wait.is_zero() {
            return Err(invalid_request("maximum RTMP publish wait must be nonzero"));
        }
        if self.maximum_reassembly_bytes.get() < self.maximum_message_bytes.get() {
            return Err(invalid_request(
                "the RTMP reassembly budget must fit one maximum-sized message",
            ));
        }
        if self.maximum_queued_payload_bytes.get() < self.maximum_message_bytes.get() {
            return Err(invalid_request(
                "the RTMP packet queue must fit one maximum-sized message",
            ));
        }
        Ok(self)
    }
}

/// An RTMP connection paused at its first publish command.
///
/// One RTMP connection maps to one publication. Although RTMP permits multiple
/// stream IDs on a connection, the session layer deliberately admits and owns
/// publications independently.
pub struct RtmpPendingPublish {
    request: PublishRequest,
    decision: oneshot::Sender<PublishDecision>,
    ingress: IngressReader,
    config: RtmpConfig,
    session: JoinHandle<SessionResult>,
}

impl RtmpPendingPublish {
    /// Runs the RTMP handshake through the first publish command.
    ///
    /// The publish name is retained exactly as the resource name and presented
    /// credential. Keeping that policy here makes a future query/token mapper a
    /// transport-only change.
    pub async fn handshake<S>(
        io: S,
        remote_address: SocketAddr,
        config: RtmpConfig,
    ) -> Result<Self, TransportError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = config.validate()?;
        let (ingress, writer) = crate::source::rtmp::channel(config.maximum_queued_payload_bytes);
        let supervisor = writer.clone();
        let (publish_tx, publish_rx) = oneshot::channel();
        let handler = MediaHandler {
            remote_address,
            publish: Some(publish_tx),
            writer,
            active_stream_id: None,
            maximum_message_bytes: config.maximum_message_bytes.get(),
        };
        let mut session = tokio::spawn(async move {
            let result = run_server_session(io, handler, config).await;
            match &result {
                Ok(true) => supervisor.finish(InputState::Closed),
                Ok(false) => supervisor.finish(InputState::Interrupted),
                Err(error) => supervisor.fail(error.to_string()),
            }
            result
        });

        let attempt = if let Ok(result) = tokio::time::timeout(config.maximum_publish_wait, async {
            tokio::select! {
                attempt = publish_rx => attempt.map_err(|_| {
                    TransportError::Handshake("RTMP connection ended before publishing".into())
                }),
                result = &mut session => Err(session_handshake_error(result)),
            }
        })
        .await
        {
            result?
        } else {
            session.abort();
            return Err(TransportError::Handshake(
                "RTMP publisher did not publish before the deadline".into(),
            ));
        };

        Ok(Self {
            request: attempt.request,
            decision: attempt.decision,
            ingress,
            config,
            session,
        })
    }

    pub async fn handshake_tcp(
        stream: TcpStream,
        config: RtmpConfig,
    ) -> Result<Self, TransportError> {
        let remote_address = stream.peer_addr().map_err(|error| {
            TransportError::Handshake(format!("could not identify the RTMP peer: {error}").into())
        })?;
        Self::handshake(stream, remote_address, config).await
    }
}

impl PendingPublish for RtmpPendingPublish {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        Ok(self.request.clone())
    }

    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(async move {
            let Self {
                decision,
                ingress,
                config,
                session,
                ..
            } = *self;
            let source = RtmpPacketSource::new(ingress, config.input_limits, meters)
                .map_err(|error| TransportError::Accept(error.to_string().into()))?;
            let (completion_tx, completion_rx) = oneshot::channel();
            decision
                .send(PublishDecision::Accept(completion_tx))
                .map_err(|_| {
                    TransportError::Accept(
                        "RTMP connection ended before acceptance completed".into(),
                    )
                })?;
            completion_rx
                .await
                .map_err(|_| {
                    TransportError::Accept(
                        "RTMP connection ended before acceptance completed".into(),
                    )
                })?
                .map_err(TransportError::Accept)?;
            Ok(AcceptedPublish {
                source: Box::new(RtmpSessionSource {
                    source,
                    session: Some(session),
                }),
                grant,
            })
        })
    }

    fn reject(
        self: Box<Self>,
        rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        Box::pin(async move {
            let (completion_tx, completion_rx) = oneshot::channel();
            let Self {
                decision, session, ..
            } = *self;
            decision
                .send(PublishDecision::Reject(rejection, completion_tx))
                .map_err(|_| {
                    TransportError::Reject(
                        "RTMP connection ended before rejection completed".into(),
                    )
                })?;
            completion_rx.await.map_err(|_| {
                TransportError::Reject("RTMP connection ended before rejection completed".into())
            })?;
            session.abort();
            Ok(())
        })
    }
}

struct RtmpSessionSource {
    source: RtmpPacketSource,
    session: Option<JoinHandle<SessionResult>>,
}

impl PacketSource for RtmpSessionSource {
    fn discover(
        &mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'_, Result<DiscoveryReport, SourceError>> {
        self.source.discover(limits)
    }

    fn fill<'a>(
        &'a mut self,
        out: &'a mut dyn Appender<Packet>,
    ) -> BoxFuture<'a, Result<InputState, SourceError>> {
        self.source.fill(out)
    }
}

impl Drop for RtmpSessionSource {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            session.abort();
        }
    }
}

struct PublishAttempt {
    request: PublishRequest,
    decision: oneshot::Sender<PublishDecision>,
}

#[derive(Debug)]
enum PublishDecision {
    Accept(oneshot::Sender<Result<(), Box<str>>>),
    Reject(PublishRejection, oneshot::Sender<()>),
}

enum PublishOutcome {
    Accepted,
    Rejected {
        rejection: PublishRejection,
        completion: oneshot::Sender<()>,
    },
}

struct MediaHandler {
    remote_address: SocketAddr,
    publish: Option<oneshot::Sender<PublishAttempt>>,
    writer: IngressWriter,
    active_stream_id: Option<u32>,
    maximum_message_bytes: usize,
}

impl MediaHandler {
    async fn send(&mut self, event: IngressEvent) -> Result<(), Box<str>> {
        let required = event.queued_bytes();
        if required > self.maximum_message_bytes {
            self.writer.fail(
                format!(
                    "RTMP media message was {required} bytes, above the permitted {}",
                    self.maximum_message_bytes
                )
                .into_boxed_str(),
            );
            return Err("RTMP media payload exceeds configured message limit".into());
        }
        self.writer.send(event).await.map_err(|error| {
            self.writer.fail(error.to_string());
            match error {
                IngressSendError::TooLarge { .. } => {
                    "RTMP media payload exceeds the packet queue".into()
                }
                other => other.to_string().into_boxed_str(),
            }
        })
    }

    async fn on_publish(
        &mut self,
        stream_id: u32,
        app_name: &str,
        stream_name: &str,
    ) -> Result<PublishOutcome, Box<str>> {
        if self.active_stream_id.is_some() || app_name.is_empty() || stream_name.is_empty() {
            return Err("invalid or duplicate RTMP publish request".into());
        }
        let publish = self
            .publish
            .take()
            .ok_or_else(|| Box::<str>::from("RTMP connection attempted a second publication"))?;
        let (decision_tx, decision_rx) = oneshot::channel();
        let request = PublishRequest {
            protocol: IngestProtocol::Rtmp,
            resource: PublishResource {
                namespace: Some(app_name.to_owned()),
                name: stream_name.to_owned(),
            },
            credential: PresentedCredential::new(stream_name.as_bytes()),
            client: ClientInfo {
                remote_address: self.remote_address,
                encoder: None,
                protocol_version: None,
            },
        };
        publish
            .send(PublishAttempt {
                request,
                decision: decision_tx,
            })
            .map_err(|_| Box::<str>::from("publication admission receiver closed"))?;

        match decision_rx
            .await
            .map_err(|_| Box::<str>::from("publication admission decision was dropped"))?
        {
            PublishDecision::Accept(completion) => {
                self.active_stream_id = Some(stream_id);
                let _ = completion.send(Ok(()));
                Ok(PublishOutcome::Accepted)
            }
            PublishDecision::Reject(rejection, completion) => Ok(PublishOutcome::Rejected {
                rejection,
                completion,
            }),
        }
    }

    fn on_unpublish(&mut self, stream_id: u32) -> Result<(), Box<str>> {
        if self.active_stream_id != Some(stream_id) {
            return Err("RTMP unpublish did not match the active stream".into());
        }
        self.active_stream_id = None;
        self.writer.finish(InputState::Closed);
        Ok(())
    }

    async fn on_audio(
        &mut self,
        stream_id: u32,
        timestamp: u32,
        media: ValidatedMedia<rtmpx::ParsedAudio>,
    ) -> Result<(), Box<str>> {
        self.require_active(stream_id)?;
        self.send(IngressEvent::Audio { timestamp, media }).await
    }

    async fn on_video(
        &mut self,
        stream_id: u32,
        timestamp: u32,
        media: ValidatedMedia<rtmpx::ParsedVideo>,
    ) -> Result<(), Box<str>> {
        self.require_active(stream_id)?;
        self.send(IngressEvent::Video { timestamp, media }).await
    }

    async fn on_metadata(
        &mut self,
        stream_id: u32,
        metadata: ValidatedMetadata,
    ) -> Result<(), Box<str>> {
        self.require_active(stream_id)?;
        self.send(IngressEvent::Metadata(metadata)).await
    }

    async fn on_script(
        &mut self,
        stream_id: u32,
        timestamp: u32,
        payload: Bytes,
    ) -> Result<(), Box<str>> {
        self.require_active(stream_id)?;
        self.send(IngressEvent::Script { timestamp, payload }).await
    }

    fn require_active(&self, stream_id: u32) -> Result<(), Box<str>> {
        if self.active_stream_id != Some(stream_id) {
            return Err("RTMP media arrived outside the active publication".into());
        }
        Ok(())
    }
}

type SessionResult = Result<bool, Box<str>>;

async fn run_server_session<S>(
    mut io: S,
    mut handler: MediaHandler,
    config: RtmpConfig,
) -> SessionResult
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut read_buffer = vec![0_u8; 16 * 1024];
    let mut handshake = Handshake::new(HandshakeRole::Server);
    let carry = loop {
        let read = timed_read(
            &mut io,
            &mut read_buffer,
            config.timeouts.handshake_read,
            "RTMP handshake read",
        )
        .await?;
        if read == 0 {
            return Err("RTMP peer closed during handshake".into());
        }
        match handshake
            .process_bytes(&read_buffer[..read])
            .map_err(|error| format!("RTMP handshake failed: {error:?}").into_boxed_str())?
        {
            HandshakeProgress::InProgress { response_bytes } => {
                timed_write(&mut io, &response_bytes, config.timeouts.write).await?;
            }
            HandshakeProgress::Completed {
                response_bytes,
                remaining_bytes,
            } => {
                timed_write(&mut io, &response_bytes, config.timeouts.write).await?;
                break remaining_bytes;
            }
        }
    };

    let mut session_config = ServerSessionConfig::new();
    session_config.window_ack_size = 2_500_000;
    session_config.decoder_limits.maximum_message_size = config.maximum_message_bytes.get();
    session_config.decoder_limits.maximum_buffered_bytes = config.maximum_reassembly_bytes.get();
    session_config.session_limits.max_streams = 1;
    session_config.payload_pool = Some(rtmpx::PayloadPool::new(rtmpx::PayloadPoolConfig {
        max_cached_payloads: 2,
        max_descriptors_per_payload: 4096,
    }));
    let mut session = ServerSession::new(session_config)
        .map_err(|error| format!("could not create RTMP session: {error}").into_boxed_str())?;
    let mut input = Bytes::from(carry);
    let mut buffer = BytesMut::with_capacity(16 * 1024);
    loop {
        if !process_session_input(&mut io, &mut session, &mut handler, config, &mut input).await? {
            return Ok(false);
        }
        // Retain the read allocation while the decoder holds incomplete messages.
        buffer.reserve(16 * 1024);
        let read = async { (&mut io).take(16 * 1024).read_buf(&mut buffer).await };
        let count = match config.timeouts.session_read {
            Some(duration) => tokio::time::timeout(duration, read)
                .await
                .map_err(|_| Box::<str>::from("RTMP session read timed out"))?,
            None => read.await,
        }
        .map_err(|error| format!("RTMP session read failed: {error}").into_boxed_str())?;
        if count == 0 {
            return Ok(true);
        }
        input = buffer.split().freeze();
    }
}

#[allow(clippy::too_many_lines)] // Keep the protocol event taxonomy visible in one dispatcher.
async fn process_session_input<S>(
    io: &mut S,
    session: &mut ServerSession,
    handler: &mut MediaHandler,
    config: RtmpConfig,
    input: &mut Bytes,
) -> Result<bool, Box<str>>
where
    S: AsyncWrite + Unpin,
{
    while let Some(output) = session
        .receive(input)
        .map_err(|error| format!("invalid RTMP input: {error}").into_boxed_str())?
    {
        match output {
            ServerOutput::Packet(packet) => timed_packet(io, packet, config.timeouts.write).await?,
            ServerOutput::Event(event) => match event {
                ServerEvent::ConnectionRequested {
                    request_id,
                    additional_properties,
                    ..
                } => {
                    EnhancedCapabilities::parse(&additional_properties, ENHANCED_VALIDATION)
                        .map_err(String::into_boxed_str)?;
                    session
                        .accept_request_with_properties(request_id, enhanced_server_capabilities())
                        .map_err(|error| error.to_string().into_boxed_str())?;
                }
                ServerEvent::PublishStreamRequested {
                    request_id,
                    app_name,
                    stream_key,
                    stream_id,
                    ..
                } => {
                    match handler
                        .on_publish(stream_id.get(), &app_name, &stream_key)
                        .await?
                    {
                        PublishOutcome::Accepted => session
                            .accept_request(request_id)
                            .map_err(|error| error.to_string().into_boxed_str())?,
                        PublishOutcome::Rejected {
                            rejection,
                            completion,
                        } => {
                            let (code, description) = publish_rejection_status(rejection);
                            session
                                .reject_request(request_id, code, description)
                                .map_err(|error| error.to_string().into_boxed_str())?;
                            drain_control(io, session, config.timeouts.write).await?;
                            let _ = completion.send(());
                            return Ok(false);
                        }
                    }
                }
                ServerEvent::AudioDataReceived {
                    data,
                    timestamp,
                    stream_id,
                    ..
                } => {
                    // The codec pipeline needs contiguous samples. Reuse a single segment,
                    // otherwise coalesce once here; the RTMP decoder itself keeps slices.
                    let media = ValidatedMedia::parse_audio(data.into_bytes(), ENHANCED_VALIDATION)
                        .map_err(|error| error.to_string().into_boxed_str())?;
                    handler
                        .on_audio(stream_id.get(), timestamp.value, media)
                        .await?;
                }
                ServerEvent::VideoDataReceived {
                    data,
                    timestamp,
                    stream_id,
                    ..
                } => {
                    let media = ValidatedMedia::parse_video(data.into_bytes(), ENHANCED_VALIDATION)
                        .map_err(|error| error.to_string().into_boxed_str())?;
                    handler
                        .on_video(stream_id.get(), timestamp.value, media)
                        .await?;
                }
                ServerEvent::StreamDataReceived {
                    message, stream_id, ..
                } => {
                    if message
                        .metadata()
                        .map_err(|error| error.to_string().into_boxed_str())?
                        .is_some()
                    {
                        let metadata = ValidatedMetadata::parse(message, ENHANCED_VALIDATION)
                            .map_err(|error| error.to_string().into_boxed_str())?;
                        handler.on_metadata(stream_id.get(), metadata).await?;
                    } else {
                        let timestamp = message.timestamp().value;
                        handler
                            .on_script(
                                stream_id.get(),
                                timestamp,
                                message.into_payload().into_bytes(),
                            )
                            .await?;
                    }
                }
                ServerEvent::PublishStreamFinished { stream_id, .. } => {
                    handler.on_unpublish(stream_id.get())?;
                    return Ok(false);
                }
                ServerEvent::PlayStreamRequested { request_id, .. } => {
                    session
                        .reject_request(
                            request_id,
                            "NetStream.Play.Failed",
                            "this endpoint only accepts publishers",
                        )
                        .map_err(|error| error.to_string().into_boxed_str())?;
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(true)
}

fn publish_rejection_status(rejection: PublishRejection) -> (&'static str, &'static str) {
    match rejection {
        PublishRejection::Unauthorized => (
            "NetStream.Publish.Denied",
            "publisher authentication failed",
        ),
        PublishRejection::Forbidden => (
            "NetStream.Publish.Denied",
            "publisher is not allowed to publish this stream",
        ),
        PublishRejection::AlreadyPublished => (
            "NetStream.Publish.BadName",
            "stream is already being published",
        ),
        PublishRejection::ServiceUnavailable => (
            "NetStream.Publish.Failed",
            "publication service is unavailable",
        ),
    }
}

fn enhanced_server_capabilities() -> rtmpx::amf0::Amf0Object {
    use rtmpx::amf0::Amf0Value;
    let video = ["avc1", "hvc1", "av01"];
    let audio = ["mp4a", "Opus"];
    let info_map = |values: &[&str]| {
        Amf0Value::Object(
            values
                .iter()
                .map(|value| ((*value).to_owned(), Amf0Value::Number(1.0)))
                .collect(),
        )
    };
    rtmpx::amf0::Amf0Object::from([
        (
            "fourCcList".to_owned(),
            Amf0Value::StrictArray(
                video
                    .into_iter()
                    .chain(audio)
                    .map(|value| Amf0Value::Utf8String(value.to_owned()))
                    .collect(),
            ),
        ),
        ("videoFourCcInfoMap".to_owned(), info_map(&video)),
        ("audioFourCcInfoMap".to_owned(), info_map(&audio)),
        ("capsEx".to_owned(), Amf0Value::Number(14.0)),
    ])
}

async fn drain_control<S: AsyncWrite + Unpin>(
    io: &mut S,
    session: &mut ServerSession,
    timeout: Option<Duration>,
) -> Result<(), Box<str>> {
    while let Some(output) = session
        .receive(&mut Bytes::new())
        .map_err(|error| error.to_string().into_boxed_str())?
    {
        if let ServerOutput::Packet(packet) = output {
            timed_packet(io, packet, timeout).await?;
        }
    }
    Ok(())
}

async fn timed_packet<S: AsyncWrite + Unpin>(
    io: &mut S,
    mut packet: rtmpx::Packet,
    timeout: Option<Duration>,
) -> Result<(), Box<str>> {
    let write = async {
        while !packet.is_complete() {
            let mut slices = [std::io::IoSlice::new(&[]); 32];
            let count = packet.io_slices(&mut slices);
            match io.write_vectored(&slices[..count]).await {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(written) => packet.advance(written),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        io.flush().await
    };
    match timeout {
        Some(duration) => tokio::time::timeout(duration, write)
            .await
            .map_err(|_| Box::<str>::from("RTMP write timed out"))?,
        None => write.await,
    }
    .map_err(|error| format!("RTMP write failed: {error}").into_boxed_str())
}

async fn timed_read<S>(
    io: &mut S,
    buffer: &mut [u8],
    timeout: Option<Duration>,
    operation: &'static str,
) -> Result<usize, Box<str>>
where
    S: AsyncRead + Unpin,
{
    match timeout {
        Some(duration) => tokio::time::timeout(duration, io.read(buffer))
            .await
            .map_err(|_| format!("{operation} timed out").into_boxed_str())?
            .map_err(|error| format!("{operation} failed: {error}").into_boxed_str()),
        None => io
            .read(buffer)
            .await
            .map_err(|error| format!("{operation} failed: {error}").into_boxed_str()),
    }
}

async fn timed_write<S>(io: &mut S, bytes: &[u8], timeout: Option<Duration>) -> Result<(), Box<str>>
where
    S: AsyncWrite + Unpin,
{
    if bytes.is_empty() {
        return Ok(());
    }
    let write = async {
        io.write_all(bytes).await?;
        io.flush().await
    };
    match timeout {
        Some(duration) => tokio::time::timeout(duration, write)
            .await
            .map_err(|_| Box::<str>::from("RTMP write timed out"))?
            .map_err(|error| format!("RTMP write failed: {error}").into_boxed_str()),
        None => write
            .await
            .map_err(|error| format!("RTMP write failed: {error}").into_boxed_str()),
    }
}

fn invalid_request(message: &'static str) -> TransportError {
    TransportError::InvalidPublishRequest(message.into())
}

fn session_handshake_error(
    result: Result<SessionResult, tokio::task::JoinError>,
) -> TransportError {
    let message = match result {
        Ok(Ok(_)) => "RTMP connection ended before publishing".into(),
        Ok(Err(error)) => format!("RTMP session failed before publishing: {error}"),
        Err(error) => format!("RTMP session task failed before publishing: {error}"),
    };
    TransportError::Handshake(message.into())
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};
    use rtmpx::{
        EnhancedValidationMode,
        amf0::{Amf0Object, Amf0Value},
        chunk_io::{ChunkEncoder, ContiguousDecoder},
        messages::{RawMessage, RtmpMessage},
        time::RtmpTimestamp,
    };

    use crate::{
        domain::Codec,
        observe::{ProcessMeters, SessionMeters},
        source::{
            DiscoveryLimits, PacketSource,
            rtmp::{IngressEvent, IngressReader, RtmpPacketSource},
        },
    };

    use super::*;

    #[test]
    fn shared_core_advances_a_new_type_three_message_by_the_previous_delta() {
        let mut wire = BytesMut::new();
        // Type 0: absolute timestamp 100, three-byte audio message.
        wire.extend_from_slice(&[
            0x04, 0x00, 0x00, 0x64, 0x00, 0x00, 0x03, 0x08, 0x01, 0x00, 0x00, 0x00,
        ]);
        wire.extend_from_slice(b"aaa");
        // Type 1: delta 21, same stream with a new three-byte payload.
        wire.extend_from_slice(&[0x44, 0x00, 0x00, 0x15, 0x00, 0x00, 0x03, 0x08]);
        wire.extend_from_slice(b"bbb");
        // Type 3: a complete new message reusing the delta, length, and type.
        wire.extend_from_slice(&[0xc4]);
        wire.extend_from_slice(b"ccc");
        let mut reader = ContiguousDecoder::new();

        let first = reader
            .get_next_message(&wire)
            .expect("first chunk is valid")
            .expect("first message is complete");
        let second = reader
            .get_next_message(&[])
            .expect("second chunk is valid")
            .expect("second message is complete");
        let third = reader
            .get_next_message(&[])
            .expect("third chunk is valid")
            .expect("third message is complete");

        assert_eq!(first.timestamp.value, 100);
        assert_eq!(second.timestamp.value, 121);
        assert_eq!(third.timestamp.value, 142);
        assert_eq!(third.data.as_ref(), b"ccc");
    }

    #[test]
    fn shared_core_keeps_a_type_three_continuation_on_the_same_timestamp() {
        let mut wire = BytesMut::new();
        // The 130-byte message exceeds RTMP's initial 128-byte chunk size.
        wire.extend_from_slice(&[
            0x04, 0x00, 0x00, 0x64, 0x00, 0x00, 0x82, 0x08, 0x01, 0x00, 0x00, 0x00,
        ]);
        wire.extend_from_slice(&[b'a'; 128]);
        wire.extend_from_slice(&[0xc4, b'b', b'b']);
        let mut reader = ContiguousDecoder::new();

        let message = reader
            .get_next_message(&wire)
            .expect("continuation chunk is valid")
            .expect("partial chunks form one complete message");

        assert_eq!(message.timestamp.value, 100);
        assert_eq!(message.data.len(), 130);
    }

    fn handler(
        capacity: NonZeroUsize,
        maximum_message_bytes: usize,
    ) -> (
        MediaHandler,
        oneshot::Receiver<PublishAttempt>,
        IngressReader,
    ) {
        let (reader, writer) = crate::source::rtmp::channel(capacity);
        let (publish, attempt) = oneshot::channel();
        (
            MediaHandler {
                remote_address: "127.0.0.1:1935".parse().expect("constant is valid"),
                publish: Some(publish),
                writer,
                active_stream_id: None,
                maximum_message_bytes,
            },
            attempt,
            reader,
        )
    }

    async fn accept(
        mut handler: MediaHandler,
        attempt: oneshot::Receiver<PublishAttempt>,
        stream_id: u32,
        app: &str,
        name: &str,
    ) -> MediaHandler {
        let app = app.to_owned();
        let name = name.to_owned();
        let publish = tokio::spawn(async move {
            let result = handler.on_publish(stream_id, &app, &name).await;
            (handler, result)
        });
        let attempt = attempt.await.expect("publish attempt was delivered");
        let (completion_tx, completion_rx) = oneshot::channel();
        attempt
            .decision
            .send(PublishDecision::Accept(completion_tx))
            .expect("handler still awaits decision");
        completion_rx
            .await
            .expect("completion was sent")
            .expect("acceptance completed");
        let (handler, result) = publish.await.expect("handler task did not panic");
        result.expect("publication was accepted");
        handler
    }

    #[tokio::test]
    async fn admission_gates_media_until_the_publish_is_accepted() {
        let (handler, attempt, reader) = handler(nz::usize!(1024), 512);
        let handler = accept(handler, attempt, 7, "live", "camera-key").await;
        assert_eq!(handler.active_stream_id, Some(7));
        drop(reader);
    }

    #[tokio::test]
    async fn publish_request_keeps_the_stream_key_as_the_credential() {
        let (mut handler, attempt, _reader) = handler(nz::usize!(1024), 512);
        let publish = tokio::spawn(async move {
            let result = handler.on_publish(7, "live", "camera-key").await;
            (handler, result)
        });
        let attempt = attempt.await.expect("publish attempt was delivered");
        assert_eq!(attempt.request.protocol, IngestProtocol::Rtmp);
        assert_eq!(attempt.request.resource.namespace.as_deref(), Some("live"));
        assert_eq!(attempt.request.resource.name, "camera-key");
        assert_eq!(attempt.request.credential.expose(), b"camera-key");
        let (completion_tx, _completion_rx) = oneshot::channel();
        attempt
            .decision
            .send(PublishDecision::Accept(completion_tx))
            .expect("handler still awaits decision");
        let (_handler, result) = publish.await.expect("handler task did not panic");
        assert!(result.is_ok());
    }

    /// Feeds one client message into the session, pumping server replies back
    /// through the deserializer so later responses decode against the same
    /// chunk state.
    fn exchange(
        session: &mut ServerSession,
        serializer: &mut ChunkEncoder,
        deserializer: &mut ContiguousDecoder,
        message: &RawMessage,
    ) -> (Vec<RtmpMessage>, Vec<ServerEvent>) {
        let packet = serializer
            .encode(message.as_ref(), rtmpx::EncodeOptions::default())
            .expect("test message serializes");
        drain(deserializer, session, &mut Bytes::from(packet.to_vec()))
    }

    /// Splits session results into decoded server messages and raised events,
    /// keeping the deserializer in step with the server chunk stream.
    fn drain(
        deserializer: &mut ContiguousDecoder,
        session: &mut ServerSession,
        input: &mut Bytes,
    ) -> (Vec<RtmpMessage>, Vec<ServerEvent>) {
        let mut messages = Vec::new();
        let mut events = Vec::new();
        while let Some(result) = session.receive(input).expect("session accepts input") {
            match result {
                ServerOutput::Packet(packet) => {
                    let mut next = deserializer
                        .get_next_message(&packet.to_vec())
                        .expect("server bytes decode");
                    loop {
                        match next {
                            Some(payload) => {
                                let message =
                                    payload.to_rtmp_message().expect("server message decodes");
                                if let RtmpMessage::SetChunkSize { size } = &message {
                                    deserializer
                                        .set_chunk_size(*size as usize)
                                        .expect("chunk size is valid");
                                }
                                messages.push(message);
                            }
                            None => break,
                        }
                        next = deserializer
                            .get_next_message(&[])
                            .expect("buffered bytes decode");
                    }
                }
                ServerOutput::Event(event) => events.push(event),
                _ => {}
            }
        }
        (messages, events)
    }

    // The session drive below is the test: splitting the connect/publish
    // choreography across more helpers would hide the sequence under review.
    // The `_result` stream id is a small server-assigned integer, so the
    // float-to-int conversion cannot lose anything in practice.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[tokio::test]
    async fn script_data_events_are_queued_for_the_packet_source() {
        let (handler, attempt, reader) = handler(nz::usize!(1024), 512);
        let mut session =
            ServerSession::new(ServerSessionConfig::new()).expect("session config is valid");
        let mut serializer = ChunkEncoder::new();
        let mut deserializer = ContiguousDecoder::new();

        // The session only emits script events for an accepted publication,
        // so drive connect, createStream, and publish for real.
        let connect = RtmpMessage::Amf0Command {
            command_name: "connect".into(),
            transaction_id: 1.0,
            command_object: Amf0Value::Object(Amf0Object::from([
                ("app".into(), Amf0Value::Utf8String("live".into())),
                ("objectEncoding".into(), Amf0Value::Number(0.0)),
            ])),
            additional_arguments: Vec::new(),
        }
        .into_raw_message(RtmpTimestamp::new(0), 0)
        .expect("connect encodes");
        let (_, events) = exchange(&mut session, &mut serializer, &mut deserializer, &connect);
        let request_id = match events.as_slice() {
            [ServerEvent::ConnectionRequested { request_id, .. }] => *request_id,
            other => panic!("expected a connection request, got {other:?}"),
        };
        session
            .accept_request(request_id)
            .expect("connection is accepted");
        drain(&mut deserializer, &mut session, &mut Bytes::new());

        let create = RtmpMessage::Amf0Command {
            command_name: "createStream".into(),
            transaction_id: 2.0,
            command_object: Amf0Value::Null,
            additional_arguments: Vec::new(),
        }
        .into_raw_message(RtmpTimestamp::new(0), 0)
        .expect("createStream encodes");
        let (responses, _) = exchange(&mut session, &mut serializer, &mut deserializer, &create);
        let stream_id = responses
            .iter()
            .find_map(|response| match response {
                RtmpMessage::Amf0Command {
                    command_name,
                    additional_arguments,
                    ..
                } if command_name == "_result" => match additional_arguments.as_slice() {
                    [Amf0Value::Number(id)] => Some(*id as u32),
                    _ => None,
                },
                _ => None,
            })
            .expect("server assigns a stream id");

        let publish = RtmpMessage::Amf0Command {
            command_name: "publish".into(),
            transaction_id: 3.0,
            command_object: Amf0Value::Null,
            additional_arguments: vec![
                Amf0Value::Utf8String("camera-key".into()),
                Amf0Value::Utf8String("live".into()),
            ],
        }
        .into_raw_message(RtmpTimestamp::new(0), stream_id)
        .expect("publish encodes");
        let (_, events) = exchange(&mut session, &mut serializer, &mut deserializer, &publish);
        let request_id = match events.as_slice() {
            [ServerEvent::PublishStreamRequested { request_id, .. }] => *request_id,
            other => panic!("expected a publish request, got {other:?}"),
        };
        session
            .accept_request(request_id)
            .expect("publish is accepted");
        drain(&mut deserializer, &mut session, &mut Bytes::new());
        let mut handler = accept(handler, attempt, stream_id, "live", "camera-key").await;

        // The cue bytes travel untouched: the session raises the event and the
        // transport queues the raw payload for the packet source.
        let payload = crate::source::encode_cue(b"onCaption", b"hello");
        let script = RawMessage {
            timestamp: RtmpTimestamp::new(1_234),
            type_id: 18, // AMF0 data, matching RtmpMessage::Amf0Data
            message_stream_id: stream_id,
            data: payload.clone(),
        };
        let packet = serializer
            .encode(script, rtmpx::EncodeOptions::default())
            .expect("script data serializes");
        let mut input = Bytes::from(packet.to_vec());
        let mut sink = tokio::io::sink();

        process_session_input(
            &mut sink,
            &mut session,
            &mut handler,
            RtmpConfig::default(),
            &mut input,
        )
        .await
        .expect("script data is queued");
        let queued = reader.try_recv().expect("script data reached the queue");
        match queued {
            crate::source::IngressEvent::Script {
                timestamp,
                payload: body,
            } => {
                assert_eq!(timestamp, 1_234);
                assert_eq!(body, payload);
            }
            other => panic!("expected script event, got {other:?}"),
        }
        handler.on_unpublish(stream_id).expect("clean unpublish");
    }

    #[tokio::test]
    async fn oversized_messages_fail_before_a_packet_is_queued() {
        let (mut handler, attempt, mut reader) = handler(nz::usize!(64), 4);
        handler = accept(handler, attempt, 1, "live", "camera").await;

        let media = ValidatedMedia::parse_audio(
            Bytes::from_static(&[0xaf, 0x01, b'1', b'2', b'3', b'4', b'5']),
            EnhancedValidationMode::Strict,
        )
        .expect("legacy AAC is valid");
        assert!(handler.on_audio(1, 0, media).await.is_err());
        match reader.recv().await {
            IngressEvent::Failed(_) => {}
            other => panic!("oversize ends as a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn framed_aac_is_discovered_by_the_native_packet_source() {
        let (mut handler, attempt, reader) = handler(nz::usize!(64 * 1024), 32 * 1024);
        handler = accept(handler, attempt, 3, "live", "aac").await;

        let mut sequence = vec![0xaf, 0];
        sequence.extend_from_slice(crate::mux::fixtures::AAC_EXTRADATA);
        let media =
            ValidatedMedia::parse_audio(Bytes::from(sequence), EnhancedValidationMode::Strict)
                .expect("AAC sequence header is valid");
        handler
            .on_audio(3, 0, media)
            .await
            .expect("AAC sequence header is queued");

        let mut frame = vec![0xaf, 1];
        frame.extend_from_slice(crate::mux::fixtures::AAC_FRAME);
        let media = ValidatedMedia::parse_audio(Bytes::from(frame), EnhancedValidationMode::Strict)
            .expect("AAC frame is valid");
        handler
            .on_audio(3, 21, media)
            .await
            .expect("AAC frame is queued");
        handler.on_unpublish(3).expect("clean unpublish");

        let meters = SessionMeters::new(ProcessMeters::default());
        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())
                .expect("RTMP source opens");
        let discovery = source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 64 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("framed AAC is discovered");
        assert_eq!(discovery.tracks.tracks()[0].codec, Codec::Aac);
        assert_eq!(
            discovery.tracks.tracks()[0].codec_extradata.as_bytes(),
            crate::mux::fixtures::AAC_EXTRADATA
        );
        let mut recovered = Vec::new();
        while source
            .fill(&mut recovered)
            .await
            .expect("framed AAC demuxes")
            .is_open()
        {}
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].payload.as_bytes(),
            crate::mux::fixtures::AAC_FRAME
        );
        assert!(!matches!(
            recovered[0].payload.as_bytes(),
            [0xff, second, ..] if second & 0xf6 == 0xf0
        ));
    }

    #[test]
    fn configuration_requires_room_for_one_whole_message() {
        let config = RtmpConfig {
            maximum_queued_payload_bytes: nz::usize!(8),
            maximum_message_bytes: nz::usize!(16),
            ..RtmpConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(TransportError::InvalidPublishRequest(_))
        ));
    }

    #[test]
    fn configuration_requires_reassembly_to_fit_one_message() {
        let config = RtmpConfig {
            maximum_reassembly_bytes: nz::usize!(8),
            maximum_message_bytes: nz::usize!(16),
            ..RtmpConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(TransportError::InvalidPublishRequest(_))
        ));
    }
}
