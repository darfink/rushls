use std::{num::NonZeroUsize, sync::Arc};

use tokio::sync::{mpsc, oneshot};

use crate::{
    domain::{Appender, BoxFuture},
    observe::SourceMeters,
    source::{
        DiscoveryLimits, DiscoveryReport, InputLimits, InputState, Packet, PacketSource,
        SourceError,
    },
};

use super::{
    control::Control,
    input::AvformatInput,
    worker::{self, WorkerEvent},
};

/// Memory and worker configuration that is specific to AVFormat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AvformatConfig {
    /// Buffer FFmpeg uses when calling the custom byte reader.
    pub io_buffer_size: NonZeroUsize,
    /// Encoded packets allowed to wait between the blocking worker and Tokio.
    ///
    /// Total queued payload is additionally bounded by the per-packet input
    /// limit. Keeping this modest lets AVFormat read ahead without letting it
    /// run away from the live pipeline.
    pub packet_channel_capacity: NonZeroUsize,
    /// Aggregate encoded payload allowed to wait in the worker channel.
    pub maximum_queued_payload_bytes: NonZeroUsize,
}

impl Default for AvformatConfig {
    fn default() -> Self {
        Self {
            io_buffer_size: NonZeroUsize::new(32 * 1024).expect("constant is nonzero"),
            packet_channel_capacity: NonZeroUsize::new(64).expect("constant is nonzero"),
            maximum_queued_payload_bytes: NonZeroUsize::new(16 * 1024 * 1024)
                .expect("constant is nonzero"),
        }
    }
}

pub struct AvformatPacketSource {
    input: Option<Box<dyn AvformatInput>>,
    config: AvformatConfig,
    limits: InputLimits,
    meters: Arc<dyn SourceMeters>,
    control: Arc<Control>,
    receiver: Option<mpsc::Receiver<WorkerEvent>>,
    pending: Option<worker::QueuedPacket>,
    discovery: Option<DiscoveryReport>,
    terminal: Option<InputState>,
}

impl AvformatPacketSource {
    pub fn new(
        input: Box<dyn AvformatInput>,
        config: AvformatConfig,
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

impl PacketSource for AvformatPacketSource {
    fn discover<'a>(
        &'a mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'a, Result<DiscoveryReport, SourceError>> {
        Box::pin(async move {
            if let Some(discovery) = &self.discovery {
                return Ok(discovery.clone());
            }
            let input = self
                .input
                .take()
                .ok_or_else(|| SourceError::Discovery("discovery already started".into()))?;
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
            let discovery = discovery_rx.await.map_err(|_| {
                SourceError::Discovery("AVFormat worker stopped during discovery".into())
            })??;
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
                    "AVFormat source must be discovered before reading packets".into(),
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

impl Drop for AvformatPacketSource {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Cursor, Read},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use crate::{
        domain::MediaParameters,
        observe::{ProcessMeters, SessionMeters},
    };

    use crate::source::avformat::{AvformatInputError, AvformatInterrupt, ReadInput};

    use super::*;

    fn wav() -> Vec<u8> {
        const SAMPLE_RATE: u32 = 8_000;
        const SAMPLES: usize = 800;
        let data_bytes = u32::try_from(SAMPLES * 2).expect("fixture size fits");
        let mut wav = Vec::with_capacity(44 + data_bytes as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        wav.resize(44 + data_bytes as usize, 0);
        wav
    }

    fn source(end: InputState, limits: InputLimits) -> AvformatPacketSource {
        let meters = SessionMeters::new(ProcessMeters::default());
        let input: Box<dyn AvformatInput> = match end {
            InputState::Closed => Box::new(ReadInput::closed(Cursor::new(wav()))),
            InputState::Interrupted | InputState::Open => {
                Box::new(ReadInput::interrupted(Cursor::new(wav())))
            }
        };
        AvformatPacketSource::new(
            input,
            AvformatConfig::default(),
            limits,
            meters.source_view(),
        )
        .expect("configuration is valid")
    }

    fn discovery_limits() -> DiscoveryLimits {
        DiscoveryLimits {
            maximum_probe_bytes: 64 * 1024,
            maximum_wall_time: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn discovers_and_demuxes_a_nonseekable_byte_stream() {
        let mut source = source(InputState::Closed, InputLimits::permissive());

        let discovery = source
            .discover(discovery_limits())
            .await
            .expect("WAV is discovered");
        assert_eq!(discovery.tracks.tracks().len(), 1);
        assert_eq!(
            discovery.tracks.tracks()[0].parameters,
            MediaParameters::Audio {
                sample_rate: nz::u32!(8_000),
                channels: nz::u16!(1),
                frame_size: None,
                bit_depth: None,
            }
        );
        assert_eq!(discovery.tracks.tracks()[0].title, None);
        assert_eq!(discovery.tracks.tracks()[0].language, None);
        assert!(discovery.tracks.tracks()[0].codec_extradata.is_empty());
        assert_eq!(
            source
                .discover(discovery_limits())
                .await
                .expect("discovery is cached"),
            discovery
        );

        let mut packets = Vec::new();
        let state = source.fill(&mut packets).await.expect("packets are read");

        assert_eq!(state, InputState::Closed);
        assert!(!packets.is_empty());
        assert!(packets.iter().all(|packet| !packet.payload.is_empty()));
        let retained = packets[0].payload.clone();
        drop(source);
        assert!(
            !retained.is_empty(),
            "the AVBuffer reference outlives its format context"
        );
    }

    #[tokio::test]
    async fn preserves_an_interrupted_byte_input_terminal_state() {
        let mut source = source(InputState::Interrupted, InputLimits::permissive());
        source
            .discover(discovery_limits())
            .await
            .expect("WAV is discovered");

        let mut packets = Vec::new();
        let state = source.fill(&mut packets).await.expect("packets are read");

        assert_eq!(state, InputState::Interrupted);
        assert!(!packets.is_empty());
    }

    #[tokio::test]
    async fn rejects_a_payload_before_copying_it_into_the_pipeline() {
        let mut source = source(
            InputState::Closed,
            InputLimits {
                maximum_payload_bytes_per_packet: 8,
                maximum_payload_bytes_per_batch: 8,
                ..InputLimits::permissive()
            },
        );
        source
            .discover(discovery_limits())
            .await
            .expect("WAV is discovered");

        let error = source
            .fill(&mut Vec::new())
            .await
            .expect_err("the demuxed packet exceeds the configured cap");

        assert!(matches!(
            error,
            SourceError::PacketPayloadTooLarge { limit: 8, .. }
        ));
    }

    #[tokio::test]
    async fn enforces_the_probe_byte_limit_inside_the_ffmpeg_reader() {
        let mut source = source(InputState::Closed, InputLimits::permissive());

        let error = source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 12,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect_err("the WAV header does not fit the probe budget");

        assert!(matches!(error, SourceError::Discovery(_)));
        assert!(error.to_string().contains("probe byte limit"));
    }

    #[tokio::test]
    async fn reports_unrecognized_input_as_a_discovery_failure() {
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut source = AvformatPacketSource::new(
            Box::new(ReadInput::closed(Cursor::new(vec![0_u8; 1_024]))),
            AvformatConfig::default(),
            InputLimits::permissive(),
            meters.source_view(),
        )
        .expect("configuration is valid");

        let error = source
            .discover(discovery_limits())
            .await
            .expect_err("zeroes are not a media container");

        assert!(matches!(
            error,
            SourceError::Open(_) | SourceError::Discovery(_)
        ));
    }

    struct TrackingInput {
        reader: Cursor<Vec<u8>>,
        dropped: Arc<AtomicBool>,
    }

    impl AvformatInput for TrackingInput {
        fn read(
            &mut self,
            buffer: &mut [u8],
            _interrupt: &dyn AvformatInterrupt,
        ) -> Result<usize, AvformatInputError> {
            match self.reader.read(buffer) {
                Ok(0) => Err(AvformatInputError::End(InputState::Closed)),
                Ok(read) => Ok(read),
                Err(error) => Err(AvformatInputError::Failed(error.to_string())),
            }
        }
    }

    impl Drop for TrackingInput {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn dropping_the_source_releases_the_worker_owned_input() {
        let dropped = Arc::new(AtomicBool::new(false));
        let meters = SessionMeters::new(ProcessMeters::default());
        let input = TrackingInput {
            reader: Cursor::new(wav()),
            dropped: Arc::clone(&dropped),
        };
        let mut source = AvformatPacketSource::new(
            Box::new(input),
            AvformatConfig {
                packet_channel_capacity: NonZeroUsize::new(1).expect("constant is nonzero"),
                ..AvformatConfig::default()
            },
            InputLimits::permissive(),
            meters.source_view(),
        )
        .expect("configuration is valid");
        source
            .discover(discovery_limits())
            .await
            .expect("WAV is discovered");

        drop(source);

        tokio::time::timeout(Duration::from_secs(2), async {
            while !dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the detached worker exits promptly");
    }
}
