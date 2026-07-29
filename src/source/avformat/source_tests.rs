use std::{
    io::{Cursor, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    domain::{MediaParameters, TrackId},
    observe::{ProcessMeters, SessionMeters},
    source::avformat::{AvformatInput, AvformatInputError, AvformatInterrupt, ReadInput, fixtures},
};

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
    let input: Box<dyn AvformatInput> = match end {
        InputState::Closed => Box::new(ReadInput::closed(Cursor::new(wav()))),
        InputState::Interrupted | InputState::Open => {
            Box::new(ReadInput::interrupted(Cursor::new(wav())))
        }
    };
    fixtures::packet_source(input, limits)
}

#[tokio::test]
async fn discovers_and_demuxes_a_nonseekable_byte_stream() {
    let mut source = source(InputState::Closed, InputLimits::permissive());

    let discovery = source
        .discover(fixtures::discovery_limits())
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
            timing: crate::domain::AudioTiming::default(),
        }
    );
    assert_eq!(discovery.tracks.tracks()[0].title, None);
    assert_eq!(discovery.tracks.tracks()[0].language, None);
    assert!(discovery.tracks.tracks()[0].codec_extradata.is_empty());
    assert_eq!(
        source
            .discover(fixtures::discovery_limits())
            .await
            .expect("discovery is cached"),
        discovery
    );

    let (packets, state) = fixtures::drain_packets(&mut source).await;

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
async fn preserves_matroska_aac_priming_through_discovery_and_packets() {
    let mut source = fixtures::packet_source(
        Box::new(ReadInput::closed(Cursor::new(fixtures::primed_aac_mkv()))),
        InputLimits::permissive(),
    );

    let discovery = source
        .discover(fixtures::discovery_limits())
        .await
        .expect("AAC-in-Matroska is discovered");
    let track = &discovery.tracks.tracks()[0];
    assert_eq!(
        track.first_pts,
        Some(0),
        "stream start is the audible start"
    );
    let MediaParameters::Audio { timing, .. } = track.parameters else {
        panic!("fixture has one audio track");
    };
    assert_eq!(timing.initial_padding_samples, 1_024);

    let mut packets = Vec::new();
    while source
        .fill(&mut packets)
        .await
        .expect("fixture demuxes")
        .is_open()
    {}
    assert_eq!(packets[0].pts, Some(-21));
    assert_eq!(
        packets[0].audio_trim,
        crate::domain::AudioTrim {
            leading_samples: 1_024,
            trailing_samples: 0,
        }
    );

    let retained_payload = packets[0].payload.clone();
    let retained_trim = packets[0].audio_trim;
    drop(source);
    assert!(!retained_payload.is_empty());
    assert_eq!(retained_trim, packets[0].audio_trim);
}

#[tokio::test]
async fn adapts_mpeg_ts_aac_without_consuming_its_first_packet() {
    let mut source = fixtures::packet_source(
        Box::new(ReadInput::closed(Cursor::new(
            fixtures::h264_adts_aac_mpeg_ts(),
        ))),
        InputLimits::permissive(),
    );

    let discovery = source
        .discover(fixtures::discovery_limits())
        .await
        .expect("H.264 and ADTS AAC in MPEG-TS are discovered");
    let tracks = discovery.tracks.tracks();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].codec, crate::domain::Codec::H264);
    assert_eq!(tracks[1].codec, crate::domain::Codec::Aac);
    assert!(tracks.iter().all(|track| {
        track.timebase == crate::domain::Timebase::new(nz::u32!(1), nz::u32!(90_000))
    }));
    assert!(
        tracks.iter().all(|track| !track.codec_extradata.is_empty()),
        "Annex-B video and filtered AAC both expose decoder configuration"
    );
    assert_eq!(tracks[0].rfc6381_codec().as_deref(), Some("avc1.42c01e"));
    assert_eq!(tracks[1].rfc6381_codec().as_deref(), Some("mp4a.40.2"));

    let (packets, state) = fixtures::drain_packets(&mut source).await;
    assert_eq!(state, InputState::Closed);
    assert_eq!(packets.len(), 8, "one video plus all seven AAC packets");
    assert_eq!(
        packets
            .iter()
            .map(|packet| packet.track_id)
            .collect::<Vec<_>>(),
        [
            TrackId(1),
            TrackId(1),
            TrackId(1),
            TrackId(1),
            TrackId(1),
            TrackId(1),
            TrackId(1),
            TrackId(0),
        ],
        "discovery prefetch preserves the demuxer's interleaving"
    );
    let audio = packets
        .iter()
        .filter(|packet| packet.track_id == TrackId(1));
    assert!(audio.clone().all(|packet| {
        !matches!(
            packet.payload.as_bytes(),
            [0xff, second, ..] if second & 0xf6 == 0xf0
        )
    }));
    assert_eq!(
        audio.map(|packet| packet.pts).collect::<Vec<_>>(),
        [Some(126_000), None, None, None, None, None, None,],
        "MPEG-TS timestamps the PES; the audio normalizer derives each AAC frame's cadence"
    );
}

#[tokio::test]
async fn mpeg_ts_prefetch_remains_inside_the_discovery_budget() {
    let mut source = fixtures::packet_source(
        Box::new(ReadInput::closed(Cursor::new(
            fixtures::h264_adts_aac_mpeg_ts(),
        ))),
        InputLimits::permissive(),
    );

    let error = source
        .discover(DiscoveryLimits {
            maximum_probe_bytes: 188,
            maximum_wall_time: Duration::from_secs(2),
        })
        .await
        .expect_err("one transport packet cannot finish discovery and AAC prefetch");
    assert_eq!(
        error,
        SourceError::Discovery(DiscoveryProblem::ProbeLimitExceeded)
    );
}

#[tokio::test]
async fn mpeg_ts_without_an_aac_access_unit_reports_missing_configuration() {
    let mut bytes = fixtures::h264_adts_aac_mpeg_ts();
    // PAT, PMT, and SDT describe the AAC stream, but no PES follows from
    // which the bitstream filter could derive AudioSpecificConfig.
    bytes.truncate(3 * 188);
    let mut source = fixtures::packet_source(
        Box::new(ReadInput::closed(Cursor::new(bytes))),
        InputLimits::permissive(),
    );

    let error = source
        .discover(fixtures::discovery_limits())
        .await
        .expect_err("AAC configuration never arrives");
    assert_eq!(
        error,
        SourceError::Discovery(DiscoveryProblem::Missing {
            field: "MPEG-TS AAC codec configuration",
        })
    );
}

#[tokio::test]
async fn preserves_webvtt_cue_metadata_after_the_source_is_released() {
    let mut source = fixtures::packet_source(
        Box::new(ReadInput::closed(Cursor::new(fixtures::WEBVTT))),
        InputLimits::permissive(),
    );

    let discovery = source
        .discover(fixtures::discovery_limits())
        .await
        .expect("WebVTT is discovered");
    assert_eq!(
        discovery.tracks.tracks()[0].codec,
        crate::domain::Codec::WebVtt
    );
    assert_eq!(
        discovery.tracks.tracks()[0].timebase,
        crate::domain::Timebase::new(nz::u32!(1), nz::u32!(1_000))
    );

    let (packets, state) = fixtures::drain_packets(&mut source).await;
    assert_eq!(state, InputState::Closed);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].pts, Some(0));
    assert_eq!(packets[0].duration, Some(2_000));
    assert_eq!(packets[0].webvtt.identifier.as_deref(), Some("cue-one"));
    assert_eq!(packets[0].webvtt.settings.as_deref(), Some("align:start"));
    assert_eq!(packets[0].payload.as_bytes(), b"Hello <b>world</b>");

    let retained = packets[0].clone();
    drop(source);
    assert_eq!(retained.webvtt.identifier.as_deref(), Some("cue-one"));
    assert_eq!(retained.payload.as_bytes(), b"Hello <b>world</b>");
}

#[tokio::test]
async fn discovers_subrip_text_and_its_optional_position_side_data() {
    let mut source = fixtures::packet_source(
        Box::new(ReadInput::closed(Cursor::new(fixtures::SUBRIP))),
        InputLimits::permissive(),
    );

    let discovery = source
        .discover(fixtures::discovery_limits())
        .await
        .expect("SubRip is discovered");
    assert_eq!(
        discovery.tracks.tracks()[0].codec,
        crate::domain::Codec::SubRip
    );

    let (packets, state) = fixtures::drain_packets(&mut source).await;
    assert_eq!(state, InputState::Closed);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].payload.as_bytes(), b"<b>Hello</b> &amp; world");
    assert_eq!(packets[0].subtitle_position, None);
    assert_eq!(
        packets[1].subtitle_position,
        Some(crate::domain::SubtitlePosition {
            x1: 10,
            y1: 20,
            x2: 100,
            y2: 80,
        })
    );
    assert_eq!(packets[1].pts, Some(1_000));
    assert_eq!(packets[1].duration, Some(1_500));
}

#[tokio::test]
async fn preserves_an_interrupted_byte_input_terminal_state() {
    let mut source = source(InputState::Interrupted, InputLimits::permissive());
    source
        .discover(fixtures::discovery_limits())
        .await
        .expect("WAV is discovered");

    let (packets, state) = fixtures::drain_packets(&mut source).await;

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
        .discover(fixtures::discovery_limits())
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

    assert_eq!(
        error,
        SourceError::Discovery(DiscoveryProblem::ProbeLimitExceeded),
        "the budget ran out, which is a different operator response from \
         the deadline running out"
    );
}

#[tokio::test]
async fn reports_unrecognized_input_as_a_discovery_failure() {
    let mut source = fixtures::packet_source(
        Box::new(ReadInput::closed(Cursor::new(vec![0_u8; 1_024]))),
        InputLimits::permissive(),
    );

    let error = source
        .discover(fixtures::discovery_limits())
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
            Err(error) => Err(AvformatInputError::Failed(error.to_string().into())),
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
            packet_channel_capacity: nz::usize!(1),
            ..AvformatConfig::default()
        },
        InputLimits::permissive(),
        meters.source_view(),
    )
    .expect("configuration is valid");
    source
        .discover(fixtures::discovery_limits())
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
