//! SRT listener and MPEG-TS packet source.
//!
//! SRT message boundaries are transport details. The accepted socket is read
//! as one ordered byte stream and demultiplexed as MPEG-TS. Other containers
//! (Matroska, FLV, …) are refused at discovery. SRT delivery timestamps are
//! intentionally ignored; embedded PES timestamps remain the media clock.
//!
//! The transport is [`rsrt`]: live mode, TSBPD, and HaiCrypt. That crate is
//! IPv4-only, so the listener refuses IPv6 bind addresses rather than silently
//! narrowing `[::]` to IPv4. Peer `SHUTDOWN` becomes [`InputState::Closed`]
//! (`#EXT-X-ENDLIST`); idle and sequence breaks become
//! [`InputState::Interrupted`] so a reconnect can continue the live playlist.
//!
//! Callers normally use `publish:<namespace/name>:<credential>`, or
//! `publish:<credential>` when one key identifies both the presented resource
//! and credential. Hardware requiring the SRT access-control convention can
//! instead use `#!::u=<credential>,r=<namespace/name>,m=publish,t=stream`.
//! Transport encryption is configured separately and is never admission.

use std::{
    net::{SocketAddr, SocketAddrV4},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use rsrt::{CloseReason, SrtOptions};

use crate::{
    admission::{ClientInfo, IngestProtocol, PublishGrant, PublishRequest},
    domain::BoxFuture,
    observe::SourceMeters,
    source::{
        AcceptedPublish, ByteInput, ByteInputError, InputLimits, InputState, MpegTsConfig,
        MpegTsPacketSource, PendingPublish, PublishRejection, TransportError,
    },
};

mod stream_id;

const SRT_MAXIMUM_LIVE_PAYLOAD_BYTES: usize = 1_456;
const SRT_MAXIMUM_STREAM_ID_BYTES: usize = 512;
/// IPv4 header + UDP header + SRT header subtracted from MSS to get payload.
const SRT_IPV4_UDP_SRT_HEADER_BYTES: usize = 44;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SrtKeyLength {
    Aes128,
    Aes192,
    Aes256,
}

impl From<SrtKeyLength> for rsrt::KeyLength {
    fn from(value: SrtKeyLength) -> Self {
        match value {
            SrtKeyLength::Aes128 => Self::Aes128,
            SrtKeyLength::Aes192 => Self::Aes192,
            SrtKeyLength::Aes256 => Self::Aes256,
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
    pub receive_buffer_bytes: NonZeroUsize,
    /// Largest complete SRT live message accepted from one caller.
    pub maximum_message_bytes: NonZeroUsize,
    pub maximum_stream_id_bytes: NonZeroUsize,
    pub encryption: Option<SrtEncryption>,
    pub mpegts: MpegTsConfig,
    pub input_limits: InputLimits,
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            latency: Duration::from_millis(120),
            peer_idle_timeout: Duration::from_secs(5),
            receive_buffer_bytes: nz::usize!(16 * 1024 * 1024),
            maximum_message_bytes: nz::usize!(SRT_MAXIMUM_LIVE_PAYLOAD_BYTES),
            maximum_stream_id_bytes: nz::usize!(SRT_MAXIMUM_STREAM_ID_BYTES),
            encryption: None,
            mpegts: MpegTsConfig::default(),
            input_limits: InputLimits::permissive(),
        }
    }
}

impl SrtConfig {
    fn validate(&self) -> Result<(), TransportError> {
        if self.latency.is_zero() || self.peer_idle_timeout.is_zero() {
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
        Ok(())
    }

    fn rsrt_options(&self, stream_id: Option<&str>) -> Result<SrtOptions, TransportError> {
        self.validate()?;
        let mut options = SrtOptions {
            latency: self.latency,
            peer_idle_timeout: self.peer_idle_timeout,
            mss: u32::try_from(
                self.maximum_message_bytes
                    .get()
                    .saturating_add(SRT_IPV4_UDP_SRT_HEADER_BYTES),
            )
            .map_err(|_| invalid_request("the SRT message size is too large"))?,
            recv_buffer_pkts: self
                .receive_buffer_bytes
                .get()
                .div_ceil(self.maximum_message_bytes.get())
                .max(1),
            udp_recv_buffer: Some(self.receive_buffer_bytes.get()),
            ..SrtOptions::default()
        };
        if let Some(encryption) = &self.encryption {
            options = options
                .passphrase(encryption.passphrase.to_string())
                .pbkeylen(encryption.key_length.into());
        }
        if let Some(stream_id) = stream_id {
            options = options.streamid(stream_id);
        }
        Ok(options)
    }
}

/// A process listener whose accept loop runs on the Tokio runtime.
pub struct SrtListener {
    inner: rsrt::SrtListener,
    config: SrtConfig,
}

impl SrtListener {
    pub async fn bind(
        address: SocketAddr,
        config: SrtConfig,
        backlog: usize,
    ) -> Result<Self, TransportError> {
        config.validate()?;
        if backlog == 0 {
            return Err(invalid_request("the SRT listener backlog must be nonzero"));
        }
        // rsrt's accept queue is a compiled constant (64). A zero from the
        // runtime is still a programming error and is refused above.

        let inner = rsrt::SrtListener::bind(ipv4_address(address)?, config.rsrt_options(None)?)
            .await
            .map_err(|error| TransportError::Handshake(error.to_string().into()))?;
        Ok(Self { inner, config })
    }

    pub fn local_address(&self) -> SocketAddr {
        SocketAddr::V4(self.inner.local_addr())
    }

    pub async fn accept(&mut self) -> Option<Result<SrtPendingPublish, TransportError>> {
        match self.inner.accept().await {
            Ok((socket, peer)) => Some(SrtPendingPublish::new(
                socket,
                SocketAddr::V4(peer),
                self.config.clone(),
            )),
            Err(rsrt::SrtError::Closed(CloseReason::Local)) => None,
            Err(error) => Some(Err(TransportError::Handshake(error.to_string().into()))),
        }
    }
}

/// An established SRT connection awaiting application admission.
pub struct SrtPendingPublish {
    request: PublishRequest,
    socket: rsrt::SrtSocket,
    config: SrtConfig,
}

impl SrtPendingPublish {
    fn new(
        socket: rsrt::SrtSocket,
        remote_address: SocketAddr,
        config: SrtConfig,
    ) -> Result<Self, TransportError> {
        let stream_id = socket.streamid().unwrap_or_default();
        let parsed = stream_id::parse(&stream_id, config.maximum_stream_id_bytes.get())
            .map_err(TransportError::InvalidPublishRequest)?;
        Ok(Self {
            request: PublishRequest {
                protocol: IngestProtocol::Srt,
                resource: parsed.resource,
                credential: parsed.credential,
                client: ClientInfo {
                    remote_address,
                    encoder: None,
                    protocol_version: None,
                },
            },
            socket,
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
            let input = SrtInput::new(socket);
            let source = MpegTsPacketSource::new(
                Box::new(input),
                config.mpegts,
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
            drop(self.socket);
            Ok(())
        })
    }
}

struct SrtInput {
    socket: rsrt::SrtSocket,
    pending: Bytes,
}

impl SrtInput {
    fn new(socket: rsrt::SrtSocket) -> Self {
        Self {
            socket,
            pending: Bytes::new(),
        }
    }

    fn copy_pending(&mut self, output: &mut [u8]) -> usize {
        let copied = self.pending.len().min(output.len());
        output[..copied].copy_from_slice(&self.pending[..copied]);
        let _ = self.pending.split_to(copied);
        copied
    }
}

impl ByteInput for SrtInput {
    fn read<'a>(
        &'a mut self,
        output: &'a mut [u8],
    ) -> BoxFuture<'a, Result<usize, ByteInputError>> {
        Box::pin(async move {
            if output.is_empty() {
                return Ok(0);
            }
            if !self.pending.is_empty() {
                return Ok(self.copy_pending(output));
            }

            loop {
                match self.socket.recv().await {
                    Ok(Some(payload)) if payload.is_empty() => {}
                    Ok(Some(payload)) => {
                        self.pending = payload;
                        return Ok(self.copy_pending(output));
                    }
                    // Peer SHUTDOWN or a local close: the publisher meant this.
                    Ok(None) => return Err(ByteInputError::End(InputState::Closed)),
                    Err(error) => return Err(byte_input_error(error)),
                }
            }
        })
    }
}

fn byte_input_error(error: rsrt::SrtError) -> ByteInputError {
    match error {
        // recv() already maps Shutdown/Local to Ok(None). Keep the same
        // distinction if a later call surfaces the closed socket as an error.
        rsrt::SrtError::Closed(CloseReason::Shutdown | CloseReason::Local) => {
            ByteInputError::End(InputState::Closed)
        }
        // PeerIdle, DataIdle, SequenceDiscrepancy: the link broke. Omit
        // EXT-X-ENDLIST so a reconnect can continue the live playlist.
        rsrt::SrtError::Closed(_) => ByteInputError::End(InputState::Interrupted),
        other => ByteInputError::Failed(other.to_string().into()),
    }
}

fn ipv4_address(address: SocketAddr) -> Result<SocketAddrV4, TransportError> {
    match address {
        SocketAddr::V4(address) => Ok(address),
        SocketAddr::V6(_) => Err(invalid_request(
            "SRT ingest is IPv4-only; bind an IPv4 address such as 0.0.0.0:9000",
        )),
    }
}

fn invalid_request(message: &'static str) -> TransportError {
    TransportError::InvalidPublishRequest(message.into())
}

/// An SRT caller that injects MPEG-TS into a local listener.
///
/// Production publishers are remote encoders. Tests need a same-process peer
/// that speaks the same stack, so this wrapper is part of the crate rather
/// than `cfg(test)`-only: integration tests compile the library without test
/// cfg and still have to drive a real `SrtListener`.
pub struct SrtCaller {
    socket: rsrt::SrtSocket,
}

impl SrtCaller {
    pub async fn connect(
        address: SocketAddr,
        config: &SrtConfig,
        stream_id: &str,
    ) -> Result<Self, TransportError> {
        let socket = rsrt::SrtSocket::connect(
            ipv4_address(address)?,
            config.rsrt_options(Some(stream_id))?,
        )
        .await
        .map_err(|error| TransportError::Handshake(error.to_string().into()))?;
        Ok(Self { socket })
    }

    pub async fn send(&self, bytes: &[u8]) -> Result<(), TransportError> {
        self.socket
            .send(bytes)
            .await
            .map_err(|error| TransportError::Accept(error.to_string().into()))
    }

    /// Sends MPEG-TS in live-message chunks of 1_316 bytes.
    pub async fn send_mpegts(&self, bytes: &[u8]) -> Result<(), TransportError> {
        for chunk in bytes.chunks(1_316) {
            self.send(chunk).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        admission::{Principal, PublishGrant, StreamPolicy},
        domain::{Codec, MediaKind, StreamId, TrackCounts},
        observe::{ProcessMeters, SessionMeters},
        source::DiscoveryLimits,
    };

    use super::*;

    fn grant() -> PublishGrant {
        PublishGrant {
            stream_id: StreamId::new("live/camera"),
            principal: Principal("publisher".into()),
            policy: StreamPolicy::permissive(),
        }
    }

    async fn bind_loopback(config: SrtConfig) -> Result<SrtListener, TransportError> {
        SrtListener::bind("127.0.0.1:0".parse().expect("constant is valid"), config, 4).await
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
    async fn an_ipv6_listen_address_is_refused() {
        let error = match SrtListener::bind(
            "[::]:0".parse().expect("constant is valid"),
            SrtConfig::default(),
            1,
        )
        .await
        {
            Ok(_listener) => panic!("IPv6 is unsupported"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("IPv4-only"),
            "refusal names the IPv4 restriction: {error}"
        );
    }

    #[tokio::test]
    async fn a_live_message_survives_partial_demux_reads() {
        let config = SrtConfig {
            encryption: Some(
                SrtEncryption::new("test-secret", SrtKeyLength::Aes256)
                    .expect("test passphrase is valid"),
            ),
            ..SrtConfig::default()
        };
        let mut listener = bind_loopback(config.clone()).await.expect("listener binds");
        let address = listener.local_address();
        let caller = tokio::spawn(async move {
            let caller = SrtCaller::connect(address, &config, "secret")
                .await
                .expect("test caller connects");
            caller.send(b"abcdefg").await.expect("message sends");
            caller
        });
        let pending = listener
            .accept()
            .await
            .expect("listener remains open")
            .expect("connection is accepted");
        let caller = caller.await.expect("caller did not panic");
        let mut input = SrtInput::new(pending.socket);
        let mut first = [0; 3];
        let mut second = [0; 4];
        assert_eq!(input.read(&mut first).await, Ok(3));
        assert_eq!(input.read(&mut second).await, Ok(4));
        assert_eq!(&first, b"abc");
        assert_eq!(&second, b"defg");
        drop(caller);
    }

    #[tokio::test]
    async fn mpeg_ts_over_srt_is_discovered_by_the_streaming_demuxer() {
        let config = SrtConfig::default();
        let mut listener = bind_loopback(config.clone()).await.expect("listener binds");
        let address = listener.local_address();
        let caller = tokio::spawn(async move {
            let caller = SrtCaller::connect(address, &config, "publish:live/camera:secret")
                .await
                .expect("test caller connects");
            caller
                .send_mpegts(&crate::source::fixtures::h264_adts_aac_mpeg_ts())
                .await
                .expect("fixture message sends");
            caller
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
        let caller = caller.await.expect("caller did not panic");
        // A burst this short does not freeze tracks until the PES assembler
        // is finished. Dropping the caller sends SHUTDOWN, which is also the
        // orderly end that writes EXT-X-ENDLIST.
        drop(caller);
        let discovery = accepted
            .source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 64 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("MPEG-TS is discovered");
        assert_eq!(
            discovery.tracks.counts(),
            TrackCounts {
                audio: 1,
                subtitle: 0,
                video: 1,
            }
        );
        assert!(
            discovery
                .tracks
                .tracks()
                .iter()
                .any(|track| track.codec == Codec::H264 && track.kind() == MediaKind::Video)
        );

        let mut packets = Vec::new();
        let mut state = InputState::Open;
        while state.is_open() {
            state = accepted
                .source
                .fill(&mut packets)
                .await
                .expect("MPEG-TS packets demux");
        }
        assert!(!packets.is_empty());
        assert_eq!(
            state,
            InputState::Closed,
            "dropping the caller sends SHUTDOWN, which ends the playlist"
        );
    }

    #[tokio::test]
    async fn matroska_over_srt_is_refused() {
        let config = SrtConfig::default();
        let mut listener = bind_loopback(config.clone()).await.expect("listener binds");
        let address = listener.local_address();
        let caller = tokio::spawn(async move {
            let caller = SrtCaller::connect(address, &config, "publish:live/camera:secret")
                .await
                .expect("test caller connects");
            caller
                .send_mpegts(&crate::source::fixtures::ebml_header())
                .await
                .expect("fixture message sends");
            caller
        });

        let pending = listener
            .accept()
            .await
            .expect("listener remains open")
            .expect("connection is accepted");
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut accepted = Box::new(pending)
            .accept(grant(), meters.source_view())
            .await
            .expect("publication is accepted");
        let caller = caller.await.expect("caller did not panic");
        let error = accepted
            .source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 64 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect_err("Matroska is not MPEG-TS");
        assert!(
            error.to_string().contains("MPEG-TS"),
            "refusal names the required container: {error}"
        );
        drop(caller);
    }

    #[test]
    fn peer_shutdown_closes_the_source_and_idle_breaks_interrupt_it() {
        assert_eq!(
            byte_input_error(rsrt::SrtError::Closed(CloseReason::Shutdown)),
            ByteInputError::End(InputState::Closed)
        );
        assert_eq!(
            byte_input_error(rsrt::SrtError::Closed(CloseReason::Local)),
            ByteInputError::End(InputState::Closed)
        );
        assert_eq!(
            byte_input_error(rsrt::SrtError::Closed(CloseReason::PeerIdle)),
            ByteInputError::End(InputState::Interrupted)
        );
        assert_eq!(
            byte_input_error(rsrt::SrtError::Closed(CloseReason::DataIdle)),
            ByteInputError::End(InputState::Interrupted)
        );
        assert_eq!(
            byte_input_error(rsrt::SrtError::Closed(CloseReason::SequenceDiscrepancy)),
            ByteInputError::End(InputState::Interrupted)
        );
    }
}
