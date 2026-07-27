//! FFmpeg-backed pass-through CMAF packaging.

mod ffi;

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use crate::{
    domain::{
        Appender, Codec, DiscoveredTrack, MediaKind, MediaParameters, TickDuration, TickTimestamp,
        TrackId, duration_since,
    },
    media::NormalizedSample,
    observe::{EventSink, SessionEvent},
    segment::TrackSegmentationPlan,
};

use super::{
    FinishReason, InitializationSegment, MediaSegmentFormat, MuxError, Muxer, MuxerFactory,
    MuxerStartRequest, PackagedChunk, PackagedMedia, PackagedPresentation, PackagedRendition,
    PackagedSegmentCompletion, PackagingRenditionId, PackagingSegmentId, RenditionConfig,
    RenditionKey, RenditionMedia, StartedMuxer,
};
use ffi::FormatOutput;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SegmentBoundaryPolicy {
    #[default]
    Strict,
    ExtendToRandomAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CmafMuxerConfig {
    pub io_buffer_size: NonZeroUsize,
    pub segment_boundary_policy: SegmentBoundaryPolicy,
}

impl Default for CmafMuxerConfig {
    fn default() -> Self {
        Self {
            io_buffer_size: nz::usize!(32 * 1024),
            segment_boundary_policy: SegmentBoundaryPolicy::Strict,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CmafMuxerFactory {
    config: CmafMuxerConfig,
}

impl CmafMuxerFactory {
    pub fn new(config: CmafMuxerConfig) -> Self {
        Self { config }
    }
}

impl MuxerFactory for CmafMuxerFactory {
    fn start(&self, request: MuxerStartRequest<'_>) -> Result<StartedMuxer, MuxError> {
        if request
            .presentation
            .tracks()
            .iter()
            .any(|track| track.kind() == MediaKind::Subtitle)
        {
            return Err(invalid("subtitle tracks require a WebVTT muxer"));
        }

        let mut renditions = Vec::with_capacity(request.presentation.tracks().len());
        let mut states = Vec::with_capacity(request.presentation.tracks().len());
        for (index, track) in request.presentation.tracks().iter().enumerate() {
            let plan = request
                .segmentation
                .get(track.id)
                .ok_or_else(|| invalid(format!("segmentation plan omits {}", track.id)))?;
            if plan.timebase != track.timebase {
                return Err(invalid(format!(
                    "segmentation and input timebases differ for {}",
                    track.id
                )));
            }
            let rendition_id = PackagingRenditionId(
                u32::try_from(index)
                    .map_err(|_| invalid("too many tracks for packaging rendition IDs"))?,
            );
            let output = FormatOutput::open(track, self.config.io_buffer_size.get())
                .map_err(|error| invalid(error.to_string()))?;
            renditions.push(packaged_rendition(rendition_id, track, plan)?);
            states.push(RenditionState::new(
                rendition_id,
                track,
                *plan,
                output,
                self.config.segment_boundary_policy,
                request.events.clone(),
            ));
        }
        let presentation = PackagedPresentation::with_default_topology(
            request.time_anchor,
            request.presentation,
            renditions,
        )
        .map_err(|error| invalid(error.to_string()))?;
        let expected_publication_interval = request.segmentation.shortest_part_duration();
        Ok(StartedMuxer {
            muxer: Box::new(CmafMuxer {
                renditions: states,
                expected_publication_interval,
                finished: false,
            }),
            presentation: Arc::new(presentation),
        })
    }
}

struct CmafMuxer {
    renditions: Vec<RenditionState>,
    expected_publication_interval: Duration,
    finished: bool,
}

impl Muxer for CmafMuxer {
    fn expected_publication_interval(&self) -> Duration {
        self.expected_publication_interval
    }

    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.finished {
            return Err(mux_error("cannot push media after the CMAF muxer finished"));
        }
        let track_id = sample.track_id();
        let rendition = self
            .renditions
            .iter_mut()
            .find(|rendition| rendition.track_id == track_id)
            .ok_or_else(|| mux_error(format!("sample references unknown {track_id}")))?;
        rendition.push(sample, out)
    }

    fn finish(
        &mut self,
        reason: FinishReason,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let mut first_error = None;
        for rendition in &mut self.renditions {
            if let Err(error) = rendition.finish(reason, out)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

struct RenditionState {
    rendition_id: PackagingRenditionId,
    track_id: TrackId,
    codec: Codec,
    kind: MediaKind,
    plan: TrackSegmentationPlan,
    output: FormatOutput,
    policy: SegmentBoundaryPolicy,
    events: EventSink,
    initialized: bool,
    segment_id: u64,
    chunk_index: u32,
    segment_start: TickTimestamp,
    segment_duration: TickDuration,
    next_segment_boundary: TickTimestamp,
    next_part_boundary: TickTimestamp,
    fragment_start: Option<TickTimestamp>,
    fragment_end: Option<TickTimestamp>,
    fragment_independent: bool,
    last_dts: Option<TickTimestamp>,
    finished: bool,
}

impl RenditionState {
    fn new(
        rendition_id: PackagingRenditionId,
        track: &DiscoveredTrack,
        plan: TrackSegmentationPlan,
        output: FormatOutput,
        policy: SegmentBoundaryPolicy,
        events: EventSink,
    ) -> Self {
        Self {
            rendition_id,
            track_id: track.id,
            codec: track.codec,
            kind: track.kind(),
            plan,
            output,
            policy,
            events,
            initialized: false,
            segment_id: 0,
            chunk_index: 0,
            segment_start: 0,
            segment_duration: 0,
            next_segment_boundary: i64::try_from(plan.segment_duration.get()).unwrap_or(i64::MAX),
            next_part_boundary: i64::try_from(plan.part_duration.get()).unwrap_or(i64::MAX),
            fragment_start: None,
            fragment_end: None,
            fragment_independent: false,
            last_dts: None,
            finished: false,
        }
    }

    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if sample_codec(&sample) != self.codec {
            return Err(mux_error(format!(
                "{} changed codec while muxing",
                self.track_id
            )));
        }
        let pts = sample
            .pts()
            .checked_sub(self.plan.origin_pts)
            .ok_or_else(|| mux_error(format!("PTS rebasing overflowed for {}", self.track_id)))?;
        let dts = sample_dts(&sample)
            .checked_sub(self.plan.origin_pts)
            .ok_or_else(|| mux_error(format!("DTS rebasing overflowed for {}", self.track_id)))?;
        if pts < 0 {
            // Pre-roll can contain media from a track that began before the
            // shared origin. Encoded access units cannot be clipped safely.
            return Ok(());
        }
        if self.last_dts.is_some_and(|last| dts < last) {
            return Err(mux_error(format!(
                "{} supplied decreasing DTS",
                self.track_id
            )));
        }
        self.last_dts = Some(dts);

        let closes_segment = self.segment_boundary(&sample, pts)?;
        if closes_segment {
            if self.fragment_end != Some(pts) {
                return Err(mux_error(format!(
                    "{} has a gap or overlap at its segment boundary",
                    self.track_id
                )));
            }
            self.flush_fragment(out)?;
            if self.segment_duration == 0 {
                return Err(mux_error(format!(
                    "{} reached an empty segment boundary",
                    self.track_id
                )));
            }
            out.push(PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
                rendition_id: self.rendition_id,
                packaging_segment_id: PackagingSegmentId(self.segment_id),
                media_start: self.segment_start,
                duration: self.segment_duration,
            }));
            let actual_boundary = self
                .segment_start
                .checked_add_unsigned(self.segment_duration)
                .ok_or_else(|| mux_error("segment timing overflowed"))?;
            self.segment_id = self.segment_id.saturating_add(1);
            self.chunk_index = 0;
            self.segment_start = actual_boundary;
            self.segment_duration = 0;
            self.next_segment_boundary = actual_boundary
                .checked_add_unsigned(self.plan.segment_duration.get())
                .ok_or_else(|| mux_error("next segment boundary overflowed"))?;
            self.next_part_boundary = actual_boundary
                .checked_add_unsigned(self.plan.part_duration.get())
                .ok_or_else(|| mux_error("next part boundary overflowed"))?;
        } else if pts >= self.next_part_boundary && self.fragment_start.is_some() {
            self.flush_fragment(out)?;
            while self.next_part_boundary <= pts {
                self.next_part_boundary = self
                    .next_part_boundary
                    .checked_add_unsigned(self.plan.part_duration.get())
                    .ok_or_else(|| mux_error("next part boundary overflowed"))?;
            }
        }

        let end = pts
            .checked_add_unsigned(sample.duration())
            .ok_or_else(|| mux_error(format!("sample end overflowed for {}", self.track_id)))?;
        if self.fragment_start.is_none() {
            self.fragment_start = Some(
                self.segment_start
                    .checked_add_unsigned(self.segment_duration)
                    .ok_or_else(|| mux_error("fragment start overflowed"))?,
            );
            self.fragment_independent = sample.random_access();
        }
        self.fragment_end = Some(self.fragment_end.map_or(end, |current| current.max(end)));
        self.output
            .write(&sample, pts, dts)
            .map_err(|error| mux_error(error.to_string()))
    }

    fn segment_boundary(
        &mut self,
        sample: &NormalizedSample,
        pts: TickTimestamp,
    ) -> Result<bool, MuxError> {
        if pts < self.next_segment_boundary {
            return Ok(false);
        }
        let exact = pts == self.next_segment_boundary;
        let random_access = sample.random_access();
        if self.kind != MediaKind::Video {
            if !exact && self.policy == SegmentBoundaryPolicy::Strict {
                return Err(mux_error(format!(
                    "{} missed its planned segment boundary",
                    self.track_id
                )));
            }
            return Ok(true);
        }
        match self.policy {
            SegmentBoundaryPolicy::Strict if exact && random_access => Ok(true),
            SegmentBoundaryPolicy::Strict => Err(mux_error(format!(
                "{} did not provide a random-access sample at its planned segment boundary",
                self.track_id
            ))),
            SegmentBoundaryPolicy::ExtendToRandomAccess if random_access => {
                if pts > self.next_segment_boundary {
                    let actual_ticks = pts
                        .checked_sub(self.segment_start)
                        .and_then(|duration| u64::try_from(duration).ok())
                        .ok_or_else(|| mux_error("extended segment duration overflowed"))?;
                    self.events.emit(SessionEvent::SegmentationExtended {
                        track: self.track_id,
                        planned: self
                            .plan
                            .timebase
                            .ticks_to_duration(self.plan.segment_duration.get()),
                        actual: self.plan.timebase.ticks_to_duration(actual_ticks),
                    });
                }
                Ok(true)
            }
            SegmentBoundaryPolicy::ExtendToRandomAccess => Ok(false),
        }
    }

    fn flush_fragment(&mut self, out: &mut dyn Appender<PackagedMedia>) -> Result<(), MuxError> {
        let Some(start) = self.fragment_start else {
            return Ok(());
        };
        let end = self
            .fragment_end
            .ok_or_else(|| mux_error("open fragment has no media end"))?;
        let duration = duration_since(end, start)
            .filter(|duration| *duration > 0)
            .ok_or_else(|| mux_error(format!("{} produced a zero-length chunk", self.track_id)))?;

        let mut payload = self
            .output
            .flush_fragment()
            .map_err(|error| mux_error(error.to_string()))?;
        if !self.initialized {
            if payload.is_empty() {
                return Err(mux_error("delay_moov produced an empty initialization"));
            }
            out.push(PackagedMedia::Initialization(InitializationSegment {
                rendition_id: self.rendition_id,
                version: 0,
                payload,
            }));
            self.initialized = true;
            payload = self
                .output
                .flush_fragment()
                .map_err(|error| mux_error(error.to_string()))?;
        }
        if payload.is_empty() {
            return Err(mux_error("FFmpeg produced an empty CMAF chunk"));
        }
        out.push(PackagedMedia::Chunk(PackagedChunk {
            rendition_id: self.rendition_id,
            packaging_segment_id: PackagingSegmentId(self.segment_id),
            chunk_index: self.chunk_index,
            media_start: start,
            duration,
            independent: self.fragment_independent,
            payload,
        }));
        self.chunk_index = self
            .chunk_index
            .checked_add(1)
            .ok_or_else(|| mux_error("chunk index overflowed"))?;
        self.segment_duration = self
            .segment_duration
            .checked_add(duration)
            .ok_or_else(|| mux_error("segment duration overflowed"))?;
        self.fragment_start = None;
        self.fragment_end = None;
        self.fragment_independent = false;
        Ok(())
    }

    fn finish(
        &mut self,
        reason: FinishReason,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if !matches!(reason, FinishReason::Superseded) {
            self.flush_fragment(out)?;
            if self.segment_duration > 0 {
                out.push(PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
                    rendition_id: self.rendition_id,
                    packaging_segment_id: PackagingSegmentId(self.segment_id),
                    media_start: self.segment_start,
                    duration: self.segment_duration,
                }));
            }
        }
        self.output
            .finalize()
            .map_err(|error| mux_error(error.to_string()))
    }
}

fn packaged_rendition(
    rendition_id: PackagingRenditionId,
    track: &DiscoveredTrack,
    plan: &TrackSegmentationPlan,
) -> Result<PackagedRendition, MuxError> {
    let codecs = track
        .rfc6381_codec()
        .ok_or_else(|| invalid(format!("{} has no RFC 6381 codec", track.id)))?;
    let media = match track.parameters {
        MediaParameters::Video {
            width,
            height,
            frame_rate,
            ..
        } => RenditionMedia::Video {
            width,
            height,
            frame_rate,
            video_range: None,
        },
        MediaParameters::Audio {
            sample_rate,
            channels,
            ..
        } => RenditionMedia::Audio {
            sample_rate,
            channels,
        },
        MediaParameters::Subtitle => return Err(invalid("subtitle tracks require WebVTT output")),
    };
    let fallback_name = format!("{:?} {}", track.kind(), rendition_id.0 + 1);
    Ok(PackagedRendition {
        packaging_rendition_id: rendition_id,
        key: RenditionKey::for_source(track),
        source_tracks: Arc::from([track.id]),
        config: RenditionConfig {
            timebase: plan.timebase,
            segment_target: plan.segment_duration,
            chunk_target: Some(plan.part_duration),
            segment_format: MediaSegmentFormat::Cmaf,
        },
        media,
        codecs,
        name: Arc::from(track.title.as_deref().unwrap_or(&fallback_name)),
        language: track.language.as_deref().map(Arc::from),
        is_default: false,
        declared_bandwidth: None,
    })
}

fn sample_codec(sample: &NormalizedSample) -> Codec {
    match sample {
        NormalizedSample::Video(sample) => sample.codec,
        NormalizedSample::Audio(sample) => sample.codec,
        NormalizedSample::Subtitle(sample) => sample.codec,
    }
}

fn sample_dts(sample: &NormalizedSample) -> TickTimestamp {
    match sample {
        NormalizedSample::Video(sample) => sample.dts,
        NormalizedSample::Audio(sample) => sample.pts,
        NormalizedSample::Subtitle(sample) => sample.pts,
    }
}

fn invalid(message: impl Into<Box<str>>) -> MuxError {
    MuxError::InvalidPlan(message.into())
}

fn mux_error(message: impl Into<Box<str>>) -> MuxError {
    MuxError::Mux(message.into())
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        num::{NonZeroU32, NonZeroU64},
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use parking_lot::Mutex;

    use crate::{
        admission::StreamPolicy,
        domain::{
            FrameRate, MediaKind, MediaParameters, Payload, SessionId, Timebase, TrackId,
            fixtures::{TrackBuilder, catalog},
        },
        media::{NormalizedSample, VideoSample, validate},
        mux::{
            InitializationSegment, MuxerStartRequest, PackagedMedia, SegmentBoundaryPolicy,
            fixtures::{H264_EXTRADATA, H264_IDR, H264_P},
        },
        observe::{EventObserver, Events, SessionEvent},
        segment::{SegmentationAlignment, SegmentationPlan, TrackSegmentationPlan},
        source::{
            DiscoveryLimits, InputLimits, PacketSource,
            avformat::{AvformatConfig, AvformatPacketSource, ReadInput},
        },
    };

    use super::{CmafMuxerConfig, CmafMuxerFactory};
    use crate::mux::MuxerFactory;

    const FRAME: u64 = 8_192;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<SessionEvent>>);

    impl EventObserver for Recorder {
        fn observe(&self, _session: SessionId, event: SessionEvent) {
            self.0.lock().push(event);
        }
    }

    fn track(timebase: Timebase) -> crate::domain::DiscoveredTrack {
        TrackBuilder::new(0, MediaKind::Video)
            .timebase(timebase)
            .parameters(MediaParameters::Video {
                width: nz::u32!(16),
                height: nz::u32!(16),
                frame_rate: Some(FrameRate::new(
                    nz::u32!(2),
                    nz::u32!(1),
                )),
                video_delay: 0,
            })
            .codec_extradata(H264_EXTRADATA.to_vec())
            .build()
    }

    fn sample(pts: i64, random_access: bool) -> NormalizedSample {
        sample_for(0, pts, random_access)
    }

    fn sample_for(track_id: u32, pts: i64, random_access: bool) -> NormalizedSample {
        sample_with_dts(track_id, pts, pts, random_access)
    }

    fn sample_with_dts(track_id: u32, pts: i64, dts: i64, random_access: bool) -> NormalizedSample {
        NormalizedSample::Video(VideoSample {
            track_id: TrackId(track_id),
            codec: crate::domain::Codec::H264,
            pts,
            dts,
            duration: FRAME,
            random_access,
            payload: Payload::from(if random_access {
                H264_IDR.to_vec()
            } else {
                H264_P.to_vec()
            }),
        })
    }

    fn start(
        policy: SegmentBoundaryPolicy,
        timebase: Timebase,
        events: &crate::observe::EventSink,
    ) -> Result<crate::mux::StartedMuxer, crate::mux::MuxError> {
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())
            .expect("fixture presentation validates");
        let segmentation = SegmentationPlan::new(
            &input,
            vec![TrackSegmentationPlan {
                track_id: TrackId(0),
                timebase,
                origin_pts: 0,
                segment_duration: nz::u64!(16_384),
                part_duration: nz::u64!(8_192),
            }],
            SegmentationAlignment::Aligned {
                timing_authority: TrackId(0),
            },
        )
        .expect("fixture segmentation validates");
        CmafMuxerFactory::new(CmafMuxerConfig {
            segment_boundary_policy: policy,
            ..CmafMuxerConfig::default()
        })
        .start(MuxerStartRequest {
            presentation: &input,
            segmentation: &segmentation,
            time_anchor: SystemTime::UNIX_EPOCH,
            events,
        })
    }

    fn event_sink() -> crate::observe::EventSink {
        Events::default().scoped(SessionId(nz::u64!(1)))
    }

    fn top_level_boxes(payload: &Payload) -> Vec<[u8; 4]> {
        let bytes = payload.as_bytes();
        let mut offset = 0;
        let mut boxes = Vec::new();
        while offset + 8 <= bytes.len() {
            let size = u32::from_be_bytes(
                bytes[offset..offset + 4]
                    .try_into()
                    .expect("box size is four bytes"),
            ) as usize;
            if size < 8 || offset + size > bytes.len() {
                break;
            }
            boxes.push(
                bytes[offset + 4..offset + 8]
                    .try_into()
                    .expect("box type is four bytes"),
            );
            offset += size;
        }
        boxes
    }


    #[test]
    fn delay_moov_emits_initialization_then_first_chunk() {
        let sink = event_sink();
        let mut started = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &sink,
        )
        .expect("CMAF muxer starts");
        let mut media = Vec::new();

        started
            .muxer
            .push(sample(0, true), &mut media)
            .expect("first sample is buffered");
        started
            .muxer
            .push(sample(FRAME as i64, false), &mut media)
            .expect("first part closes");

        assert_eq!(media.len(), 2);
        let PackagedMedia::Initialization(InitializationSegment {
            payload: initialization,
            ..
        }) = &media[0]
        else {
            panic!("initialization must be emitted first");
        };
        let PackagedMedia::Chunk(chunk) = &media[1] else {
            panic!("the buffered fragment follows initialization");
        };
        let initialization_boxes = top_level_boxes(initialization);
        assert!(initialization_boxes.contains(b"ftyp"));
        assert!(initialization_boxes.contains(b"moov"));
        let chunk_boxes = top_level_boxes(&chunk.payload);
        assert!(chunk_boxes.contains(b"moof"));
        assert!(chunk_boxes.contains(b"mdat"));
        assert!(!chunk_boxes.contains(b"sidx"));
        assert_eq!(chunk.media_start, 0);
        assert_eq!(chunk.duration, FRAME);

        // Frozen output bytes own their allocation independently of FFmpeg.
        let retained = chunk.payload.clone();
        drop(started);
        assert!(!retained.is_empty());
    }

    #[test]
    fn strict_boundaries_complete_zero_based_segments() {
        let sink = event_sink();
        let mut started = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &sink,
        )
        .expect("CMAF muxer starts");
        let mut media = Vec::new();
        for value in [sample(0, true), sample(8_192, false), sample(16_384, true)] {
            started.muxer.push(value, &mut media).expect("sample muxes");
        }

        let chunks: Vec<_> = media
            .iter()
            .filter_map(|event| match event {
                PackagedMedia::Chunk(chunk) => Some(chunk),
                _ => None,
            })
            .collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].packaging_segment_id.0, 0);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[1].chunk_index, 1);
        assert!(matches!(
            media.last(),
            Some(PackagedMedia::SegmentCompleted(completion))
                if completion.packaging_segment_id.0 == 0
                    && completion.media_start == 0
                    && completion.duration == 16_384
        ));
    }

    #[test]
    fn multiple_tracks_use_distinct_output_contexts_and_rendition_ids() {
        let sink = event_sink();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(16_384));
        let input = validate(
            &catalog(vec![track(timebase), {
                let mut second = track(timebase);
                second.id = TrackId(1);
                second
            }]),
            &StreamPolicy::permissive(),
        )
        .expect("two-video fixture validates");
        let segmentation = SegmentationPlan::new(
            &input,
            [0, 1]
                .into_iter()
                .map(|track_id| TrackSegmentationPlan {
                    track_id: TrackId(track_id),
                    timebase,
                    origin_pts: 0,
                    segment_duration: nz::u64!(16_384),
                    part_duration: nz::u64!(8_192),
                })
                .collect(),
            SegmentationAlignment::Aligned {
                timing_authority: TrackId(0),
            },
        )
        .expect("two-track segmentation validates");
        let mut started = CmafMuxerFactory::default()
            .start(MuxerStartRequest {
                presentation: &input,
                segmentation: &segmentation,
                time_anchor: SystemTime::UNIX_EPOCH,
                events: &sink,
            })
            .expect("two CMAF outputs start");
        assert_eq!(started.presentation.renditions.len(), 2);

        let mut media = Vec::new();
        for track_id in [0, 1] {
            started
                .muxer
                .push(sample_for(track_id, 0, true), &mut media)
                .expect("first sample buffers");
            started
                .muxer
                .push(sample_for(track_id, 8_192, false), &mut media)
                .expect("first part flushes");
        }
        let ids: Vec<_> = media
            .iter()
            .filter_map(|event| match event {
                PackagedMedia::Initialization(initialization) => Some(initialization.rendition_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            [
                crate::mux::PackagingRenditionId(0),
                crate::mux::PackagingRenditionId(1)
            ]
        );
    }

    #[tokio::test]
    async fn emitted_initialization_and_chunks_are_demuxable() {
        let sink = event_sink();
        let mut started = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &sink,
        )
        .expect("CMAF muxer starts");
        let mut media = Vec::new();
        for value in [
            sample(0, true),
            sample(8_192, false),
            sample(16_384, true),
            sample(24_576, false),
        ] {
            started.muxer.push(value, &mut media).expect("sample muxes");
        }
        started
            .muxer
            .finish(crate::mux::FinishReason::Final, &mut media)
            .expect("output finishes");

        let mut bytes = Vec::new();
        for event in media {
            match event {
                PackagedMedia::Initialization(initialization) => {
                    bytes.extend_from_slice(initialization.payload.as_bytes());
                }
                PackagedMedia::Chunk(chunk) => {
                    bytes.extend_from_slice(chunk.payload.as_bytes());
                }
                PackagedMedia::Segment(_) | PackagedMedia::SegmentCompleted(_) => {}
            }
        }
        let process = crate::observe::ProcessMeters::default();
        let session = crate::observe::SessionMeters::new(process);
        let mut source = AvformatPacketSource::new(
            Box::new(ReadInput::closed(Cursor::new(bytes))),
            AvformatConfig::default(),
            InputLimits::permissive(),
            session.source_view(),
        )
        .expect("source config validates");
        let discovery = source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 1024 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("CMAF output is discoverable");
        assert_eq!(
            discovery.tracks.tracks()[0].timebase,
            Timebase::new(nz::u32!(1), nz::u32!(16_384))
        );
        let mut packets = Vec::new();
        source.fill(&mut packets).await.expect("CMAF packets demux");
        assert_eq!(packets.len(), 4);
        assert!(packets[0].random_access);
    }

    #[test]
    fn rebases_pts_and_preserves_negative_dts_at_the_shared_origin() {
        let sink = event_sink();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(16_384));
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())
            .expect("fixture presentation validates");
        let segmentation = SegmentationPlan::new(
            &input,
            vec![TrackSegmentationPlan {
                track_id: TrackId(0),
                timebase,
                origin_pts: 1_000,
                segment_duration: nz::u64!(16_384),
                part_duration: nz::u64!(8_192),
            }],
            SegmentationAlignment::Aligned {
                timing_authority: TrackId(0),
            },
        )
        .expect("fixture segmentation validates");
        let mut started = CmafMuxerFactory::default()
            .start(MuxerStartRequest {
                presentation: &input,
                segmentation: &segmentation,
                time_anchor: SystemTime::UNIX_EPOCH,
                events: &sink,
            })
            .expect("CMAF output starts");
        let mut media = Vec::new();

        started
            .muxer
            .push(sample_with_dts(0, 1_000, 900, true), &mut media)
            .expect("negative rebased DTS is accepted");
        started
            .muxer
            .push(sample_for(0, 9_192, false), &mut media)
            .expect("first rebased part flushes");

        assert!(matches!(
            media.as_slice(),
            [PackagedMedia::Initialization(_), PackagedMedia::Chunk(chunk)]
                if chunk.media_start == 0 && chunk.duration == FRAME
        ));
    }

    #[test]
    fn finish_flushes_final_and_interrupted_tails_but_not_superseded_tails() {
        let sink = event_sink();
        for reason in [
            crate::mux::FinishReason::Final,
            crate::mux::FinishReason::Interrupted,
        ] {
            let mut started = start(
                SegmentBoundaryPolicy::Strict,
                Timebase::new(nz::u32!(1), nz::u32!(16_384)),
                &sink,
            )
            .expect("CMAF muxer starts");
            let mut media = Vec::new();
            started
                .muxer
                .push(sample(0, true), &mut media)
                .expect("tail sample buffers");
            started
                .muxer
                .finish(reason, &mut media)
                .expect("tail flushes");
            assert!(matches!(
                media.as_slice(),
                [
                    PackagedMedia::Initialization(_),
                    PackagedMedia::Chunk(_),
                    PackagedMedia::SegmentCompleted(_)
                ]
            ));
            let count = media.len();
            started
                .muxer
                .finish(reason, &mut media)
                .expect("finish is idempotent");
            assert_eq!(media.len(), count);
        }

        let mut superseded = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &sink,
        )
        .expect("CMAF muxer starts");
        let mut discarded = Vec::new();
        superseded
            .muxer
            .push(sample(0, true), &mut discarded)
            .expect("tail sample buffers");
        superseded
            .muxer
            .finish(crate::mux::FinishReason::Superseded, &mut discarded)
            .expect("superseded output finalizes");
        assert!(discarded.is_empty());
    }

    #[test]
    fn strict_mode_rejects_a_missed_random_access_boundary() {
        let sink = event_sink();
        let mut started = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &sink,
        )
        .expect("CMAF muxer starts");
        let mut media = Vec::new();
        for value in [sample(0, true), sample(8_192, false)] {
            started.muxer.push(value, &mut media).expect("sample muxes");
        }

        assert!(matches!(
            started.muxer.push(sample(16_384, false), &mut media),
            Err(crate::mux::MuxError::Mux(_))
        ));
    }

    #[test]
    fn extension_mode_warns_and_keeps_parts_flowing_until_a_keyframe() {
        let recorder = Arc::new(Recorder::default());
        let events = Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>);
        let sink = events.scoped(SessionId(nz::u64!(2)));
        let mut started = start(
            SegmentBoundaryPolicy::ExtendToRandomAccess,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &sink,
        )
        .expect("CMAF muxer starts");
        let mut media = Vec::new();
        for value in [
            sample(0, true),
            sample(8_192, false),
            sample(16_384, false),
            sample(24_576, true),
        ] {
            started.muxer.push(value, &mut media).expect("sample muxes");
        }

        assert_eq!(
            media
                .iter()
                .filter(|event| matches!(event, PackagedMedia::Chunk(_)))
                .count(),
            3
        );
        assert!(matches!(
            recorder.0.lock().as_slice(),
            [SessionEvent::SegmentationExtended {
                track: TrackId(0),
                ..
            }]
        ));
    }

    #[test]
    fn rejects_subtitles_and_negotiated_timebase_changes() {
        let sink = event_sink();
        let subtitle_input = validate(
            &catalog(vec![
                track(Timebase::hz90k()),
                TrackBuilder::new(1, MediaKind::Subtitle).build(),
            ]),
            &StreamPolicy::permissive(),
        )
        .expect("mixed fixture validates");
        let segmentation = SegmentationPlan::new(
            &subtitle_input,
            vec![
                TrackSegmentationPlan {
                    track_id: TrackId(0),
                    timebase: Timebase::hz90k(),
                    origin_pts: 0,
                    segment_duration: nz::u64!(180_000),
                    part_duration: nz::u64!(90_000),
                },
                TrackSegmentationPlan {
                    track_id: TrackId(1),
                    timebase: Timebase::hz90k(),
                    origin_pts: 0,
                    segment_duration: nz::u64!(180_000),
                    part_duration: nz::u64!(90_000),
                },
            ],
            SegmentationAlignment::Independent,
        )
        .expect("fixture segmentation validates");
        assert!(matches!(
            CmafMuxerFactory::default().start(MuxerStartRequest {
                presentation: &subtitle_input,
                segmentation: &segmentation,
                time_anchor: SystemTime::UNIX_EPOCH,
                events: &sink,
            }),
            Err(crate::mux::MuxError::InvalidPlan(_))
        ));

        let changed = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(1_000)),
            &sink,
        );
        assert!(matches!(changed, Err(crate::mux::MuxError::InvalidPlan(_))));
    }
}
