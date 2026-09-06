use std::{num::NonZeroUsize, sync::Arc};

use tokio::sync::{mpsc, oneshot};

use crate::{
    domain::{Appender, BoxFuture},
    observe::SourceMeters,
    source::{
        ByteInput, DiscoveryLimits, DiscoveryProblem, DiscoveryReport, InputLimits, InputState,
        Packet, PacketSource, SourceError,
    },
};

use super::{
    control::Control,
    worker::{self, WorkerEvent},
};

/// Memory and worker configuration for the streaming MPEG-TS demuxer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MpegTsConfig {
    /// Encoded bytes requested from the transport on each demux turn.
    pub read_buffer_size: NonZeroUsize,
    /// Encoded packets allowed to wait between the blocking worker and Tokio.
    pub packet_channel_capacity: NonZeroUsize,
    /// Aggregate encoded payload allowed to wait in the worker channel.
    pub maximum_queued_payload_bytes: NonZeroUsize,
}

impl Default for MpegTsConfig {
    fn default() -> Self {
        Self {
            read_buffer_size: nz::usize!(32 * 1024),
            packet_channel_capacity: nz::usize!(64),
            maximum_queued_payload_bytes: NonZeroUsize::new(
                crate::source::PipelineMemory::DEMUX_QUEUE,
            )
            .expect("the demux queue budget is nonzero"),
        }
    }
}

pub struct MpegTsPacketSource {
    input: Option<Box<dyn ByteInput>>,
    config: MpegTsConfig,
    limits: InputLimits,
    meters: Arc<dyn SourceMeters>,
    control: Arc<Control>,
    receiver: Option<mpsc::Receiver<WorkerEvent>>,
    pending: Option<worker::QueuedPacket>,
    discovery: Option<DiscoveryReport>,
    terminal: Option<InputState>,
}

impl MpegTsPacketSource {
    pub fn new(
        input: Box<dyn ByteInput>,
        config: MpegTsConfig,
        limits: InputLimits,
        meters: Arc<dyn SourceMeters>,
    ) -> Result<Self, SourceError> {
        if limits.maximum_packets_per_batch == 0 {
            return Err(SourceError::Open(
                "maximum packets per batch must be nonzero".into(),
            ));
        }
        if limits.maximum_payload_bytes_per_packet == 0 {
            return Err(SourceError::Open(
                "maximum packet payload must be nonzero".into(),
            ));
        }
        if limits.maximum_payload_bytes_per_batch < limits.maximum_payload_bytes_per_packet {
            return Err(SourceError::Open(
                "maximum batch payload must fit one maximum-sized packet".into(),
            ));
        }
        if config.maximum_queued_payload_bytes.get() < limits.maximum_payload_bytes_per_packet {
            return Err(SourceError::Open(
                "queued payload budget must fit one maximum-sized packet".into(),
            ));
        }
        Ok(Self {
            input: Some(input),
            config,
            limits,
            meters,
            control: Arc::new(Control::new()),
            receiver: None,
            pending: None,
            discovery: None,
            terminal: None,
        })
    }

    async fn receive(&mut self, wait: bool) -> Option<WorkerEvent> {
        let receiver = self.receiver.as_mut()?;
        if wait {
            receiver.recv().await
        } else {
            receiver.try_recv().ok()
        }
    }
}

impl PacketSource for MpegTsPacketSource {
    fn discover(
        &mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'_, Result<DiscoveryReport, SourceError>> {
        Box::pin(async move {
            if let Some(discovery) = &self.discovery {
                return Ok(discovery.clone());
            }
            let input = self.input.take().ok_or(DiscoveryProblem::AlreadyStarted)?;
            let (discovery_tx, discovery_rx) = oneshot::channel();
            let (output_tx, output_rx) = mpsc::channel(self.config.packet_channel_capacity.get());
            worker::spawn(
                input,
                self.config,
                self.limits,
                limits,
                Arc::clone(&self.control),
                discovery_tx,
                output_tx,
            )?;
            self.receiver = Some(output_rx);
            let discovery = discovery_rx
                .await
                .map_err(|_| SourceError::from(DiscoveryProblem::Abandoned))??;
            self.discovery = Some(discovery.clone());
            Ok(discovery)
        })
    }

    fn fill<'a>(
        &'a mut self,
        out: &'a mut dyn Appender<Packet>,
    ) -> BoxFuture<'a, Result<InputState, SourceError>> {
        Box::pin(async move {
            if self.discovery.is_none() {
                return Err(SourceError::Input(
                    "MPEG-TS source must be discovered before reading packets".into(),
                ));
            }
            if let Some(terminal) = self.terminal {
                return Ok(terminal);
            }

            let mut packets = 0_usize;
            let mut payload_bytes = 0_usize;
            loop {
                let event = if let Some(packet) = self.pending.take() {
                    Some(WorkerEvent::Packet(packet))
                } else {
                    self.receive(packets == 0).await
                };
                let Some(event) = event else {
                    if packets == 0 {
                        self.terminal = Some(InputState::Interrupted);
                    }
                    break;
                };

                match event {
                    WorkerEvent::Packet(packet) => {
                        let next_bytes = payload_bytes
                            .checked_add(packet.payload_len())
                            .ok_or_else(|| {
                                SourceError::Input("batch payload accounting overflowed".into())
                            })?;
                        if packets == self.limits.maximum_packets_per_batch
                            || next_bytes > self.limits.maximum_payload_bytes_per_batch
                        {
                            self.pending = Some(packet);
                            break;
                        }
                        payload_bytes = next_bytes;
                        packets += 1;
                        out.push(packet.into_packet());
                    }
                    WorkerEvent::End(state) => {
                        self.terminal = Some(state);
                        break;
                    }
                    WorkerEvent::Error(error) => return Err(error),
                }
            }

            self.meters
                .source_progress(payload_bytes as u64, packets as u64, 0);
            Ok(self.terminal.unwrap_or(InputState::Open))
        })
    }
}

impl Drop for MpegTsPacketSource {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, time::Duration};

    use crate::{
        domain::{Codec, MediaKind, TrackCounts},
        observe::{ProcessMeters, SessionMeters},
        source::{
            DiscoveryLimits, H264CaptionDetector, InputLimits, InputState, PacketSource, ReadInput,
            SourceError,
        },
    };

    use super::*;

    fn discovery_limits() -> DiscoveryLimits {
        DiscoveryLimits {
            maximum_probe_bytes: 64 * 1024,
            maximum_wall_time: Duration::from_secs(2),
        }
    }

    fn source(bytes: Vec<u8>) -> MpegTsPacketSource {
        let meters = SessionMeters::new(ProcessMeters::default());
        MpegTsPacketSource::new(
            Box::new(ReadInput::closed(Cursor::new(bytes))),
            MpegTsConfig::default(),
            InputLimits::permissive(),
            meters.source_view(),
        )
        .expect("fixture source opens")
    }

    async fn drain(source: &mut MpegTsPacketSource) -> (Vec<crate::source::Packet>, InputState) {
        let mut packets = Vec::new();
        loop {
            let state = source.fill(&mut packets).await.expect("fixture demuxes");
            if !state.is_open() {
                return (packets, state);
            }
        }
    }

    #[tokio::test]
    async fn discovers_h264_and_raw_aac_from_mpeg_ts() {
        let mut source = source(crate::source::fixtures::h264_adts_aac_mpeg_ts());
        let discovery = source
            .discover(discovery_limits())
            .await
            .expect("H.264 and ADTS AAC in MPEG-TS are discovered");
        let tracks = discovery.tracks.tracks();
        assert_eq!(tracks.len(), 2);
        let video = tracks
            .iter()
            .find(|track| track.kind() == MediaKind::Video)
            .expect("video track");
        let audio = tracks
            .iter()
            .find(|track| track.kind() == MediaKind::Audio)
            .expect("audio track");
        assert_eq!(video.codec, Codec::H264);
        assert_eq!(audio.codec, Codec::Aac);
        assert_eq!(video.timebase, crate::domain::Timebase::hz90k());
        let crate::domain::MediaParameters::Video { frame_rate, .. } = video.parameters else {
            panic!("video track parameters");
        };
        assert_eq!(
            frame_rate,
            Some(crate::domain::FrameRate::new(nz::u32!(10), nz::u32!(1))),
            "SPS VUI timing times a one-frame stream that has no successor DTS"
        );
        let crate::domain::MediaParameters::Audio { sample_rate, .. } = audio.parameters else {
            panic!("audio track parameters");
        };
        assert_eq!(
            audio.timebase,
            crate::domain::Timebase::new(nz::u32!(1), sample_rate)
        );
        assert!(
            tracks.iter().all(|track| !track.codec_extradata.is_empty()),
            "length-prefixed video and filtered AAC both expose decoder configuration"
        );
        assert!(
            tracks.iter().all(|track| track.first_pts.is_some()),
            "discovery waits for a first PTS so timeline calibration can start"
        );
        assert_eq!(video.rfc6381_codec().as_deref(), Some("avc1.42c01e"));
        assert_eq!(audio.rfc6381_codec().as_deref(), Some("mp4a.40.2"));

        let (packets, state) = drain(&mut source).await;
        assert_eq!(state, InputState::Closed);
        assert!(!packets.is_empty());
        let audio_packets = packets
            .iter()
            .filter(|packet| packet.track_id == audio.id)
            .collect::<Vec<_>>();
        assert!(!audio_packets.is_empty());
        assert!(audio_packets.iter().all(|packet| {
            !matches!(
                packet.payload.as_bytes(),
                [0xff, second, ..] if second & 0xf6 == 0xf0
            )
        }));
        assert!(
            packets
                .iter()
                .any(|packet| packet.track_id == video.id && packet.random_access)
        );
    }

    #[tokio::test]
    async fn discovers_iso_639_language_on_each_audio_pid() -> Result<(), SourceError> {
        let mut source =
            source(include_bytes!("../../../tests/apple_hls/fixtures/h264_dual_aac.ts").to_vec());
        let discovery = source.discover(discovery_limits()).await?;
        assert_eq!(
            discovery.tracks.counts(),
            TrackCounts {
                audio: 2,
                subtitle: 0,
                video: 1,
            }
        );
        let mut languages: Vec<_> = discovery
            .tracks
            .tracks()
            .iter()
            .filter(|track| track.kind() == MediaKind::Audio)
            .filter_map(|track| track.language.as_deref())
            .collect();
        languages.sort_unstable();
        assert_eq!(
            languages,
            ["en", "es"],
            "PMT ISO_639_language_descriptor becomes catalog language"
        );
        Ok(())
    }

    #[tokio::test]
    async fn caption_scanner_sees_avcc_framing_from_mpeg_ts() {
        let mut source = source(crate::source::fixtures::h264_adts_aac_mpeg_ts());
        let discovery = source
            .discover(discovery_limits())
            .await
            .expect("fixture is discovered");
        let video = discovery
            .tracks
            .tracks()
            .iter()
            .find(|track| track.codec == Codec::H264)
            .expect("H.264 track");
        let mut detector =
            H264CaptionDetector::new(video).expect("avcC extradata selects AVCC framing");

        let (packets, _) = drain(&mut source).await;
        for packet in packets.iter().filter(|packet| packet.track_id == video.id) {
            let _ = detector.inspect(packet.payload.as_bytes());
        }
        assert_eq!(detector.malformed_sei(), 0);
        assert!(detector.access_units_seen() > 0);
    }

    #[tokio::test]
    async fn probe_budget_stops_discovery_before_tracks_resolve() {
        let mut source = source(crate::source::fixtures::h264_adts_aac_mpeg_ts());
        let error = source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 188,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect_err("one transport packet cannot finish discovery");
        assert_eq!(
            error,
            SourceError::Discovery(crate::source::DiscoveryProblem::ProbeLimitExceeded)
        );
    }

    #[tokio::test]
    async fn matroska_is_refused_as_not_mpeg_ts() {
        let mut source = source(crate::source::fixtures::ebml_header());
        let error = source
            .discover(discovery_limits())
            .await
            .expect_err("Matroska is not MPEG-TS");
        assert!(
            error.to_string().contains("MPEG-TS"),
            "refusal names the required container: {error}"
        );
    }

    #[tokio::test]
    async fn truncated_tables_without_access_units_do_not_invent_tracks() {
        let mut bytes = crate::source::fixtures::h264_adts_aac_mpeg_ts();
        bytes.truncate(3 * 188);
        let mut source = source(bytes);
        let error = source
            .discover(discovery_limits())
            .await
            .expect_err("PAT/PMT without PES cannot describe media");
        assert!(
            matches!(error, SourceError::Discovery(_) | SourceError::Demux(_)),
            "truncated tables fail discovery: {error}"
        );
    }

    #[tokio::test]
    async fn fill_before_discover_is_refused() {
        let mut source = source(crate::source::fixtures::h264_adts_aac_mpeg_ts());
        let error = source
            .fill(&mut Vec::new())
            .await
            .expect_err("fill requires discovery");
        assert!(matches!(error, SourceError::Input(_)));
    }
}
