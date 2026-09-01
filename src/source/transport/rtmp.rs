//! RTMP handshake and FLV packet source.
//!
//! RTMP media messages already carry FLV tag payloads, including Enhanced RTMP
//! payloads. This adapter adds only the FLV file header and tag framing, then
//! lets the existing AVFormat source discover and demultiplex the result.

use std::{net::SocketAddr, num::NonZeroUsize, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use cc_rtmp::{
    EnhancedCapabilities, EnhancedValidationMode, ServerSessionTimeouts, ValidatedMedia,
    ValidatedMetadata,
    handshake::{Handshake, HandshakeProcessResult, PeerType},
    sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult},
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
        avformat::{
            AvformatByteChannel, AvformatByteChannelWriter, AvformatConfig, AvformatPacketSource,
        },
    },
};

const FLV_HEADER: &[u8] = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00";
const FLV_TAG_HEADER_BYTES: usize = 11;
const FLV_PREVIOUS_TAG_SIZE_BYTES: usize = 4;
const FLV_TAG_OVERHEAD: usize = FLV_TAG_HEADER_BYTES + FLV_PREVIOUS_TAG_SIZE_BYTES;
const FLV_MAXIMUM_PAYLOAD_BYTES: usize = 0x00ff_ffff;

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
    pub timeouts: ServerSessionTimeouts,
    /// Enhanced FLV structural validation policy.
    pub enhanced_validation: EnhancedValidationMode,
    /// Encoded FLV bytes allowed to wait for AVFormat.
    pub maximum_buffered_flv_bytes: NonZeroUsize,
    /// Application limit below the FLV format's fixed 24-bit tag-size ceiling.
    pub maximum_tag_payload_bytes: NonZeroUsize,
    pub avformat: AvformatConfig,
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
            timeouts: ServerSessionTimeouts {
                handshake_read: Some(Duration::from_secs(10)),
                session_read: Some(Duration::from_secs(10)),
                write: Some(Duration::from_secs(10)),
            },
            enhanced_validation: EnhancedValidationMode::Strict,
            maximum_buffered_flv_bytes: nz::usize!(16 * 1024 * 1024),
            maximum_tag_payload_bytes: nz::usize!(8 * 1024 * 1024),
            avformat: AvformatConfig::default(),
            input_limits: InputLimits::permissive(),
        }
    }
}

impl RtmpConfig {
    fn validate(self) -> Result<Self, TransportError> {
        if self.maximum_publish_wait.is_zero() {
            return Err(invalid_request("maximum RTMP publish wait must be nonzero"));
        }
        if self.maximum_tag_payload_bytes.get() > FLV_MAXIMUM_PAYLOAD_BYTES {
            return Err(invalid_request(
                "maximum RTMP tag payload exceeds FLV's 24-bit size field",
            ));
        }
        let required = self
            .maximum_tag_payload_bytes
            .get()
            .checked_add(FLV_TAG_OVERHEAD)
            .ok_or_else(|| invalid_request("maximum RTMP tag size overflowed"))?;
        if self.maximum_buffered_flv_bytes.get() < required {
            return Err(invalid_request(
                "the FLV byte budget must fit one maximum-sized tag",
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
    input: AvformatByteChannel,
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
        let (input, writer) = AvformatByteChannel::new(config.maximum_buffered_flv_bytes);
        let supervisor = writer.clone();
        let (publish_tx, publish_rx) = oneshot::channel();
        let handler = FlvHandler {
            remote_address,
            publish: Some(publish_tx),
            writer,
            active_stream_id: None,
            maximum_tag_payload_bytes: config.maximum_tag_payload_bytes.get(),
            framing: BytesMut::new(),
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
            input,
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
                input,
                config,
                session,
                ..
            } = *self;
            let source = AvformatPacketSource::new(
                Box::new(input),
                config.avformat,
                config.input_limits,
                meters,
            )
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
                source: Box::new(RtmpPacketSource {
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

struct RtmpPacketSource {
    source: AvformatPacketSource,
    session: Option<JoinHandle<SessionResult>>,
}

impl PacketSource for RtmpPacketSource {
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

impl Drop for RtmpPacketSource {
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

struct FlvHandler {
    remote_address: SocketAddr,
    publish: Option<oneshot::Sender<PublishAttempt>>,
    writer: AvformatByteChannelWriter,
    active_stream_id: Option<u32>,
    maximum_tag_payload_bytes: usize,
    /// Scratch space for the FLV tag header and previous-tag-size footer.
    ///
    /// The frozen framing `Bytes` handed to the channel shares this buffer's
    /// allocation, and the next message reuses whatever capacity remains, so
    /// steady state costs no per-message allocation for framing.
    framing: BytesMut,
}

impl FlvHandler {
    async fn write_tag(
        &mut self,
        tag_type: u8,
        timestamp: u32,
        payload: Bytes,
    ) -> Result<(), Box<str>> {
        if payload.len() > self.maximum_tag_payload_bytes
            || payload.len() > FLV_MAXIMUM_PAYLOAD_BYTES
        {
            self.writer.fail(
                format!(
                    "RTMP media message was {} bytes, above the permitted {}",
                    payload.len(),
                    self.maximum_tag_payload_bytes
                )
                .into_boxed_str(),
            );
            return Err("RTMP media payload exceeds configured FLV tag limit".into());
        }

        // Bounded above by `FLV_MAXIMUM_PAYLOAD_BYTES` (24-bit).
        let payload_len = u32::try_from(payload.len()).expect("FLV payload fits u32");
        let header_len = u32::try_from(FLV_TAG_HEADER_BYTES).expect("FLV header fits u32");
        let mut header = [0_u8; FLV_TAG_HEADER_BYTES];
        header[0] = tag_type;
        write_u24_be(&mut header[1..4], payload_len);
        write_u24_be(&mut header[4..7], timestamp & 0x00ff_ffff);
        header[7] = u8::try_from(timestamp >> 24).unwrap_or(u8::MAX);
        // header[8..11] is FLV's always-zero StreamID.
        let previous_tag_size = (header_len + payload_len).to_be_bytes();

        // Both framing pieces are carved from one per-connection scratch
        // buffer: each frozen `Bytes` shares its allocation with the next
        // message's reuse, so steady state costs no allocation for framing.
        self.framing.clear();
        self.framing.extend_from_slice(&header);
        let header = self.framing.split_to(self.framing.len()).freeze();
        self.framing.clear();
        self.framing.extend_from_slice(&previous_tag_size);
        let previous_tag_size = self.framing.split_to(self.framing.len()).freeze();

        self.writer
            .send_group([header, payload, previous_tag_size])
            .await
            .map_err(|error| {
                self.writer.fail(error.to_string());
                error.to_string().into_boxed_str()
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
                let result = self
                    .writer
                    .send(Bytes::from_static(FLV_HEADER))
                    .await
                    .map_err(|error| error.to_string().into_boxed_str());
                match result {
                    Ok(()) => {
                        self.active_stream_id = Some(stream_id);
                        let _ = completion.send(Ok(()));
                        Ok(PublishOutcome::Accepted)
                    }
                    Err(error) => {
                        self.writer.fail(error.clone());
                        let _ = completion.send(Err(error));
                        Err("could not enqueue FLV header".into())
                    }
                }
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

    async fn on_data(&mut self, stream_id: u32, data: SessionData) -> Result<(), Box<str>> {
        if self.active_stream_id != Some(stream_id) {
            return Err("RTMP media arrived outside the active publication".into());
        }
        match data {
            SessionData::Audio { timestamp, data } => self.write_tag(8, timestamp, data).await,
            SessionData::Video { timestamp, data } => self.write_tag(9, timestamp, data).await,
            SessionData::Amf0 { timestamp, data } => self.write_tag(18, timestamp, data).await,
        }
    }
}

enum SessionData {
    Audio { timestamp: u32, data: Bytes },
    Video { timestamp: u32, data: Bytes },
    Amf0 { timestamp: u32, data: Bytes },
}

type SessionResult = Result<bool, Box<str>>;

async fn run_server_session<S>(
    mut io: S,
    mut handler: FlvHandler,
    config: RtmpConfig,
) -> SessionResult
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut read_buffer = vec![0_u8; 16 * 1024];
    let mut handshake = Handshake::new(PeerType::Server);
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
            HandshakeProcessResult::InProgress { response_bytes } => {
                timed_write(&mut io, &response_bytes, config.timeouts.write).await?;
            }
            HandshakeProcessResult::Completed {
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
    session_config.chunk_deserializer.maximum_message_size = config.maximum_tag_payload_bytes.get();
    session_config.chunk_deserializer.maximum_buffered_bytes =
        config.maximum_buffered_flv_bytes.get();
    let (mut session, initial) = ServerSession::new(session_config)
        .map_err(|error| format!("could not create RTMP session: {error}").into_boxed_str())?;
    debug_assert!(initial.is_empty(), "server must not write before connect");

    if !carry.is_empty() {
        let results = session
            .handle_input(&carry)
            .map_err(|error| format!("invalid RTMP input: {error}").into_boxed_str())?;
        if !process_session_results(&mut io, &mut session, &mut handler, config, results).await? {
            return Ok(false);
        }
    }

    loop {
        let read = timed_read(
            &mut io,
            &mut read_buffer,
            config.timeouts.session_read,
            "RTMP session read",
        )
        .await?;
        if read == 0 {
            return Ok(true);
        }
        let results = session
            .handle_input(&read_buffer[..read])
            .map_err(|error| format!("invalid RTMP input: {error}").into_boxed_str())?;
        if !process_session_results(&mut io, &mut session, &mut handler, config, results).await? {
            return Ok(false);
        }
    }
}

#[allow(clippy::too_many_lines)] // Keep the protocol event taxonomy visible in one dispatcher.
async fn process_session_results<S>(
    io: &mut S,
    session: &mut ServerSession,
    handler: &mut FlvHandler,
    config: RtmpConfig,
    results: Vec<ServerSessionResult>,
) -> Result<bool, Box<str>>
where
    S: AsyncWrite + Unpin,
{
    let mut pending = results;
    loop {
        let mut follow_up = Vec::new();
        for result in pending {
            match result {
                ServerSessionResult::OutboundResponse(packet) => {
                    timed_write(io, &packet.bytes, config.timeouts.write).await?;
                }
                ServerSessionResult::UnhandleableMessageReceived(_) => {}
                ServerSessionResult::RaisedEvent(event) => match event {
                    ServerSessionEvent::ConnectionRequested {
                        request_id,
                        additional_properties,
                        ..
                    } => {
                        EnhancedCapabilities::parse(
                            &additional_properties,
                            config.enhanced_validation,
                        )
                        .map_err(String::into_boxed_str)?;
                        follow_up.extend(
                            session
                                .accept_request_with_properties(
                                    request_id,
                                    enhanced_server_capabilities(),
                                )
                                .map_err(|error| error.to_string().into_boxed_str())?,
                        );
                    }
                    ServerSessionEvent::PublishStreamRequested {
                        request_id,
                        app_name,
                        stream_key,
                        stream_id,
                        ..
                    } => {
                        match handler
                            .on_publish(stream_id, &app_name, &stream_key)
                            .await?
                        {
                            PublishOutcome::Accepted => {
                                follow_up.extend(
                                    session
                                        .accept_request(request_id)
                                        .map_err(|error| error.to_string().into_boxed_str())?,
                                );
                            }
                            PublishOutcome::Rejected {
                                rejection,
                                completion,
                            } => {
                                let (code, description) = publish_rejection_status(rejection);
                                follow_up.extend(
                                    session
                                        .reject_request(request_id, code, description)
                                        .map_err(|error| error.to_string().into_boxed_str())?,
                                );
                                write_server_results(io, config.timeouts.write, follow_up).await?;
                                let _ = completion.send(());
                                return Ok(false);
                            }
                        }
                    }
                    ServerSessionEvent::AudioDataReceived {
                        data, timestamp, ..
                    } => {
                        let media = ValidatedMedia::parse_audio(data, config.enhanced_validation)
                            .map_err(|error| error.to_string().into_boxed_str())?;
                        let stream_id = handler.active_stream_id.ok_or_else(|| {
                            Box::<str>::from("audio arrived before publish acceptance")
                        })?;
                        handler
                            .on_data(
                                stream_id,
                                SessionData::Audio {
                                    timestamp: timestamp.value,
                                    data: media.raw,
                                },
                            )
                            .await?;
                    }
                    ServerSessionEvent::VideoDataReceived {
                        data, timestamp, ..
                    } => {
                        let media = ValidatedMedia::parse_video(data, config.enhanced_validation)
                            .map_err(|error| error.to_string().into_boxed_str())?;
                        let stream_id = handler.active_stream_id.ok_or_else(|| {
                            Box::<str>::from("video arrived before publish acceptance")
                        })?;
                        handler
                            .on_data(
                                stream_id,
                                SessionData::Video {
                                    timestamp: timestamp.value,
                                    data: media.raw,
                                },
                            )
                            .await?;
                    }
                    ServerSessionEvent::StreamMetadataChanged {
                        raw_metadata,
                        raw_payload,
                        timestamp,
                        ..
                    } => {
                        let metadata = ValidatedMetadata::parse(
                            raw_payload,
                            raw_metadata,
                            config.enhanced_validation,
                        )
                        .map_err(|error| error.to_string().into_boxed_str())?;
                        let stream_id = handler.active_stream_id.ok_or_else(|| {
                            Box::<str>::from("metadata arrived before publish acceptance")
                        })?;
                        handler
                            .on_data(
                                stream_id,
                                SessionData::Amf0 {
                                    timestamp: timestamp.value,
                                    data: metadata.raw,
                                },
                            )
                            .await?;
                    }
                    ServerSessionEvent::StreamDataReceived {
                        raw_payload,
                        timestamp,
                        ..
                    } => {
                        let stream_id = handler.active_stream_id.ok_or_else(|| {
                            Box::<str>::from("script data arrived before publish acceptance")
                        })?;
                        handler
                            .on_data(
                                stream_id,
                                SessionData::Amf0 {
                                    timestamp: timestamp.value,
                                    data: raw_payload,
                                },
                            )
                            .await?;
                    }
                    ServerSessionEvent::PublishStreamFinished { .. } => {
                        if let Some(stream_id) = handler.active_stream_id {
                            handler.on_unpublish(stream_id)?;
                        }
                        return Ok(false);
                    }
                    ServerSessionEvent::PlayStreamRequested { request_id, .. } => {
                        follow_up.extend(
                            session
                                .reject_request(
                                    request_id,
                                    "NetStream.Play.Failed",
                                    "this endpoint only accepts publishers",
                                )
                                .map_err(|error| error.to_string().into_boxed_str())?,
                        );
                    }
                    _ => {}
                },
            }
        }
        if follow_up.is_empty() {
            return Ok(true);
        }
        pending = follow_up;
    }
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

fn enhanced_server_capabilities() -> std::collections::HashMap<String, cc_rtmp::rml_amf0::Amf0Value>
{
    use cc_rtmp::rml_amf0::Amf0Value;
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
    std::collections::HashMap::from([
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

async fn write_server_results<S>(
    io: &mut S,
    timeout: Option<Duration>,
    results: Vec<ServerSessionResult>,
) -> Result<(), Box<str>>
where
    S: AsyncWrite + Unpin,
{
    for result in results {
        if let ServerSessionResult::OutboundResponse(packet) = result {
            timed_write(io, &packet.bytes, timeout).await?;
        }
    }
    Ok(())
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

fn write_u24_be(output: &mut [u8], value: u32) {
    output.copy_from_slice(&[
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    ]);
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
    use std::io::Cursor;

    use bytes::BytesMut;
    use cc_rtmp::chunk_io::ChunkDeserializer;

    use crate::{
        domain::Codec,
        observe::{ProcessMeters, SessionMeters},
        source::{
            DiscoveryLimits, PacketSource,
            avformat::{
                AvformatConfig, AvformatInput, AvformatInputError, AvformatInterrupt,
                AvformatPacketSource, ReadInput,
            },
        },
    };

    use super::*;

    struct NeverInterrupt;

    impl AvformatInterrupt for NeverInterrupt {
        fn interrupted(&self) -> bool {
            false
        }
    }

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
        let mut reader = ChunkDeserializer::new();

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
        let mut reader = ChunkDeserializer::new();

        let message = reader
            .get_next_message(&wire)
            .expect("continuation chunk is valid")
            .expect("partial chunks form one complete message");

        assert_eq!(message.timestamp.value, 100);
        assert_eq!(message.data.len(), 130);
    }

    fn handler(
        capacity: NonZeroUsize,
        maximum_tag_payload_bytes: usize,
    ) -> (
        FlvHandler,
        oneshot::Receiver<PublishAttempt>,
        AvformatByteChannel,
    ) {
        let (input, writer) = AvformatByteChannel::new(capacity);
        let (publish, attempt) = oneshot::channel();
        (
            FlvHandler {
                remote_address: "127.0.0.1:1935".parse().expect("constant is valid"),
                publish: Some(publish),
                writer,
                active_stream_id: None,
                maximum_tag_payload_bytes,
                framing: BytesMut::new(),
            },
            attempt,
            input,
        )
    }

    fn drain(input: &mut AvformatByteChannel) -> Result<Vec<u8>, AvformatInputError> {
        let mut output = Vec::new();
        let mut buffer = [0; 64];
        loop {
            match input.read(&mut buffer, &NeverInterrupt) {
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(AvformatInputError::End(_)) => return Ok(output),
                Err(error) => return Err(error),
            }
        }
    }

    #[tokio::test]
    async fn admission_gates_flv_header_and_preserves_tag_payloads() {
        let (mut handler, attempt, mut input) = handler(nz::usize!(1024), 512);
        let publish = tokio::spawn(async move {
            let result = handler.on_publish(7, "live", "camera-key").await;
            (handler, result)
        });
        let attempt = attempt.await.expect("publish attempt was delivered");
        assert_eq!(attempt.request.protocol, IngestProtocol::Rtmp);
        assert_eq!(attempt.request.resource.namespace.as_deref(), Some("live"));
        assert_eq!(attempt.request.resource.name, "camera-key");
        assert_eq!(attempt.request.credential.expose(), b"camera-key");

        let (completion_tx, completion_rx) = oneshot::channel();
        attempt
            .decision
            .send(PublishDecision::Accept(completion_tx))
            .expect("handler still awaits decision");
        completion_rx
            .await
            .expect("completion was sent")
            .expect("header was queued");
        let (mut handler, result) = publish.await.expect("handler task did not panic");
        assert!(result.is_ok());

        let payload = Bytes::from_static(b"\x90av01enhanced");
        handler
            .on_data(
                7,
                SessionData::Video {
                    timestamp: 0x1234_5678,
                    data: payload.clone(),
                },
            )
            .await
            .expect("tag was queued");
        handler.on_unpublish(7).expect("clean unpublish");

        let bytes = drain(&mut input).expect("channel ended cleanly");
        assert_eq!(&bytes[..FLV_HEADER.len()], FLV_HEADER);
        let tag = &bytes[FLV_HEADER.len()..];
        assert_eq!(tag[0], 9);
        assert_eq!(
            &tag[1..4],
            &[
                0,
                0,
                u8::try_from(payload.len()).expect("fixture payload fits u8")
            ]
        );
        assert_eq!(&tag[4..8], &[0x34, 0x56, 0x78, 0x12]);
        assert_eq!(&tag[8..11], &[0, 0, 0]);
        assert_eq!(&tag[11..11 + payload.len()], payload.as_ref());
        let previous_tag_size = u32::try_from(FLV_TAG_HEADER_BYTES).expect("header fits u32")
            + u32::try_from(payload.len()).expect("fixture payload fits u32");
        assert_eq!(&tag[11 + payload.len()..], &previous_tag_size.to_be_bytes());
    }

    #[tokio::test]
    async fn stream_data_events_preserve_script_payloads_as_flv_tags() {
        let (mut handler, _attempt, mut input) = handler(nz::usize!(1024), 512);
        handler.active_stream_id = Some(7);
        let payload = Bytes::from_static(
            b"\x02\x00\x09onCaption\x08\x00\x00\x00\x01\x00\x04text\x02\x00\x05hello\x00\x00\x09",
        );
        let event = ServerSessionEvent::StreamDataReceived {
            app_name: "live".into(),
            stream_key: "camera-key".into(),
            raw_payload: payload.clone(),
            timestamp: cc_rtmp::time::RtmpTimestamp::new(1_234),
        };
        let (mut session, _initial) =
            ServerSession::new(ServerSessionConfig::new()).expect("session config is valid");
        let mut sink = tokio::io::sink();

        process_session_results(
            &mut sink,
            &mut session,
            &mut handler,
            RtmpConfig::default(),
            vec![ServerSessionResult::RaisedEvent(event)],
        )
        .await
        .expect("script data event is relayed");
        handler.on_unpublish(7).expect("clean unpublish");

        let bytes = drain(&mut input).expect("channel ended cleanly");
        assert_eq!(bytes[0], 18, "script data uses an FLV data tag");
        assert_eq!(&bytes[4..8], &[0, 4, 210, 0]);
        assert_eq!(&bytes[11..11 + payload.len()], payload.as_ref());
    }

    #[tokio::test]
    async fn oversized_messages_fail_before_exposing_a_partial_tag() {
        let (mut handler, attempt, mut input) = handler(nz::usize!(64), 4);
        let publish = tokio::spawn(async move {
            let result = handler.on_publish(1, "live", "camera").await;
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
            .expect("header was queued");
        let (mut handler, _) = publish.await.expect("handler task did not panic");

        assert!(
            handler
                .on_data(
                    1,
                    SessionData::Audio {
                        timestamp: 0,
                        data: Bytes::from_static(b"12345"),
                    },
                )
                .await
                .is_err()
        );
        let error = drain(&mut input).expect_err("oversize ends as a failure");
        assert!(matches!(error, AvformatInputError::Failed(_)));
    }

    #[tokio::test]
    async fn framed_aac_is_discovered_by_the_real_avformat_source() {
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut fixture = AvformatPacketSource::new(
            Box::new(ReadInput::closed(Cursor::new(
                crate::source::avformat::fixtures::primed_aac_mkv(),
            ))),
            AvformatConfig::default(),
            InputLimits::permissive(),
            meters.source_view(),
        )
        .expect("fixture source opens");
        let discovery = fixture
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 64 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("AAC fixture is discovered");
        let extradata = discovery.tracks.tracks()[0].codec_extradata.clone();
        let mut packets = Vec::new();
        while fixture
            .fill(&mut packets)
            .await
            .expect("AAC fixture demuxes")
            .is_open()
        {}

        let (mut handler, attempt, input) = handler(nz::usize!(64 * 1024), 32 * 1024);
        let publish = tokio::spawn(async move {
            let result = handler.on_publish(3, "live", "aac").await;
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
            .expect("header was queued");
        let (mut handler, result) = publish.await.expect("handler task did not panic");
        result.expect("publication was accepted");

        let mut sequence = Vec::with_capacity(2 + extradata.len());
        sequence.extend_from_slice(&[0xaf, 0]);
        sequence.extend_from_slice(extradata.as_bytes());
        handler
            .on_data(
                3,
                SessionData::Audio {
                    timestamp: 0,
                    data: Bytes::from(sequence),
                },
            )
            .await
            .expect("AAC sequence header is framed");
        for (index, packet) in packets.iter().enumerate() {
            let mut payload = Vec::with_capacity(2 + packet.payload.len());
            payload.extend_from_slice(&[0xaf, 1]);
            payload.extend_from_slice(packet.payload.as_bytes());
            handler
                .on_data(
                    3,
                    SessionData::Audio {
                        timestamp: u32::try_from(index * 21).expect("fixture is short"),
                        data: Bytes::from(payload),
                    },
                )
                .await
                .expect("AAC frame is framed");
        }
        handler.on_unpublish(3).expect("clean unpublish");

        let meters = SessionMeters::new(ProcessMeters::default());
        let mut source = AvformatPacketSource::new(
            Box::new(input),
            AvformatConfig::default(),
            InputLimits::permissive(),
            meters.source_view(),
        )
        .expect("FLV source opens");
        let discovery = source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 64 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("framed FLV is discovered");
        assert_eq!(discovery.tracks.tracks()[0].codec, Codec::Aac);
        let mut recovered = Vec::new();
        while source
            .fill(&mut recovered)
            .await
            .expect("framed FLV demuxes")
            .is_open()
        {}
        assert_eq!(recovered.len(), packets.len());
    }

    #[test]
    fn configuration_requires_room_for_one_whole_flv_tag() {
        let config = RtmpConfig {
            maximum_buffered_flv_bytes: nz::usize!(20),
            maximum_tag_payload_bytes: nz::usize!(10),
            ..RtmpConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(TransportError::InvalidPublishRequest(_))
        ));
    }

    #[test]
    fn configuration_rejects_a_payload_limit_that_flv_cannot_encode() {
        let config = RtmpConfig {
            maximum_buffered_flv_bytes: nz::usize!(0x0100_0100),
            maximum_tag_payload_bytes: nz::usize!(0x0100_0000),
            ..RtmpConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(TransportError::InvalidPublishRequest(_))
        ));
    }
}
