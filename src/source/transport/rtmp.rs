//! RTMP handshake and FLV packet source.
//!
//! RTMP media messages already carry FLV tag payloads, including Enhanced RTMP
//! payloads. This adapter adds only the FLV file header and tag framing, then
//! lets the existing AVFormat source discover and demultiplex the result.

use std::{net::SocketAddr, num::NonZeroUsize, sync::Arc, time::Duration};

use bytes::Bytes;
use scuffle_rtmp::{
    ServerSession,
    session::server::{ServerSessionError, SessionData, SessionHandler},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
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
    session: JoinHandle<Result<bool, scuffle_rtmp::error::RtmpError>>,
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
        };
        let mut session = tokio::spawn(async move {
            let result = ServerSession::new(io, handler).run().await;
            match &result {
                Ok(true) => supervisor.finish(InputState::Closed),
                Ok(false) => supervisor.finish(InputState::Interrupted),
                Err(error) => supervisor.fail(error.to_string()),
            }
            result
        });

        let attempt = match tokio::time::timeout(config.maximum_publish_wait, async {
            tokio::select! {
                attempt = publish_rx => attempt.map_err(|_| {
                    TransportError::Handshake("RTMP connection ended before publishing".into())
                }),
                result = &mut session => Err(session_handshake_error(result)),
            }
        })
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                session.abort();
                return Err(TransportError::Handshake(
                    "RTMP publisher did not publish before the deadline".into(),
                ));
            }
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

            // scuffle-rtmp currently exposes no handler API for a typed
            // NetStream.Publish rejection. Returning a handler error closes the
            // connection, which is safe but less informative to the encoder.
            Ok(())
        })
    }
}

struct RtmpPacketSource {
    source: AvformatPacketSource,
    session: Option<JoinHandle<Result<bool, scuffle_rtmp::error::RtmpError>>>,
}

impl PacketSource for RtmpPacketSource {
    fn discover<'a>(
        &'a mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'a, Result<DiscoveryReport, SourceError>> {
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

struct FlvHandler {
    remote_address: SocketAddr,
    publish: Option<oneshot::Sender<PublishAttempt>>,
    writer: AvformatByteChannelWriter,
    active_stream_id: Option<u32>,
    maximum_tag_payload_bytes: usize,
}

impl FlvHandler {
    async fn write_tag(
        &self,
        tag_type: u8,
        timestamp: u32,
        payload: Bytes,
    ) -> Result<(), ServerSessionError> {
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
            // SessionHandler has no user-defined error variant. The detailed
            // cause is retained on the byte input before the RTMP session ends.
            return Err(ServerSessionError::PlayNotSupported);
        }

        let payload_len = payload.len() as u32;
        let mut header = [0_u8; FLV_TAG_HEADER_BYTES];
        header[0] = tag_type;
        write_u24_be(&mut header[1..4], payload_len);
        write_u24_be(&mut header[4..7], timestamp & 0x00ff_ffff);
        header[7] = (timestamp >> 24) as u8;
        // header[8..11] is FLV's always-zero StreamID.
        let previous_tag_size = (FLV_TAG_HEADER_BYTES as u32 + payload_len).to_be_bytes();

        self.writer
            .send_group([
                Bytes::copy_from_slice(&header),
                payload,
                Bytes::copy_from_slice(&previous_tag_size),
            ])
            .await
            .map_err(|error| {
                self.writer.fail(error.to_string());
                ServerSessionError::PlayNotSupported
            })
    }
}

impl SessionHandler for FlvHandler {
    async fn on_publish(
        &mut self,
        stream_id: u32,
        app_name: &str,
        stream_name: &str,
    ) -> Result<(), ServerSessionError> {
        if self.active_stream_id.is_some() || app_name.is_empty() || stream_name.is_empty() {
            return Err(ServerSessionError::PlayNotSupported);
        }
        let publish = self
            .publish
            .take()
            .ok_or(ServerSessionError::PlayNotSupported)?;
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
                // Scuffle's handler callback does not expose connect metadata
                // or the negotiated Enhanced RTMP capability set.
                protocol_version: None,
            },
        };
        publish
            .send(PublishAttempt {
                request,
                decision: decision_tx,
            })
            .map_err(|_| ServerSessionError::PlayNotSupported)?;

        match decision_rx
            .await
            .map_err(|_| ServerSessionError::PlayNotSupported)?
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
                        Ok(())
                    }
                    Err(error) => {
                        self.writer.fail(error.clone());
                        let _ = completion.send(Err(error));
                        Err(ServerSessionError::PlayNotSupported)
                    }
                }
            }
            PublishDecision::Reject(_rejection, completion) => {
                let _ = completion.send(());
                Err(ServerSessionError::PlayNotSupported)
            }
        }
    }

    async fn on_unpublish(&mut self, stream_id: u32) -> Result<(), ServerSessionError> {
        if self.active_stream_id != Some(stream_id) {
            return Err(ServerSessionError::PlayNotSupported);
        }
        self.active_stream_id = None;
        self.writer.finish(InputState::Closed);
        Ok(())
    }

    async fn on_data(
        &mut self,
        stream_id: u32,
        data: SessionData,
    ) -> Result<(), ServerSessionError> {
        if self.active_stream_id != Some(stream_id) {
            return Err(ServerSessionError::PlayNotSupported);
        }
        match data {
            SessionData::Audio { timestamp, data } => self.write_tag(8, timestamp, data).await,
            SessionData::Video { timestamp, data } => self.write_tag(9, timestamp, data).await,
            SessionData::Amf0 { timestamp, data } => self.write_tag(18, timestamp, data).await,
        }
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
    result: Result<Result<bool, scuffle_rtmp::error::RtmpError>, tokio::task::JoinError>,
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
    use scuffle_rtmp::chunk::reader::ChunkReader;

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
    fn scuffle_advances_a_new_type_three_message_by_the_previous_delta() {
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
        let mut reader = ChunkReader::default();

        let first = reader
            .read_chunk(&mut wire)
            .expect("first chunk is valid")
            .expect("first message is complete");
        let second = reader
            .read_chunk(&mut wire)
            .expect("second chunk is valid")
            .expect("second message is complete");
        let third = reader
            .read_chunk(&mut wire)
            .expect("third chunk is valid")
            .expect("third message is complete");

        assert_eq!(first.message_header.timestamp, 100);
        assert_eq!(second.message_header.timestamp, 121);
        assert_eq!(third.message_header.timestamp, 142);
        assert_eq!(third.payload.as_ref(), b"ccc");
        assert!(wire.is_empty());
    }

    #[test]
    fn scuffle_keeps_a_type_three_continuation_on_the_same_timestamp() {
        let mut wire = BytesMut::new();
        // The 130-byte message exceeds RTMP's initial 128-byte chunk size.
        wire.extend_from_slice(&[
            0x04, 0x00, 0x00, 0x64, 0x00, 0x00, 0x82, 0x08, 0x01, 0x00, 0x00, 0x00,
        ]);
        wire.extend_from_slice(&[b'a'; 128]);
        wire.extend_from_slice(&[0xc4, b'b', b'b']);
        let mut reader = ChunkReader::default();

        let message = reader
            .read_chunk(&mut wire)
            .expect("continuation chunk is valid")
            .expect("partial chunks form one complete message");

        assert_eq!(message.message_header.timestamp, 100);
        assert_eq!(message.payload.len(), 130);
        assert!(wire.is_empty());
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
        handler.on_unpublish(7).await.expect("clean unpublish");

        let bytes = drain(&mut input).expect("channel ended cleanly");
        assert_eq!(&bytes[..FLV_HEADER.len()], FLV_HEADER);
        let tag = &bytes[FLV_HEADER.len()..];
        assert_eq!(tag[0], 9);
        assert_eq!(&tag[1..4], &[0, 0, payload.len() as u8]);
        assert_eq!(&tag[4..8], &[0x34, 0x56, 0x78, 0x12]);
        assert_eq!(&tag[8..11], &[0, 0, 0]);
        assert_eq!(&tag[11..11 + payload.len()], payload.as_ref());
        assert_eq!(
            &tag[11 + payload.len()..],
            &(FLV_TAG_HEADER_BYTES as u32 + payload.len() as u32).to_be_bytes()
        );
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
        handler.on_unpublish(3).await.expect("clean unpublish");

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
