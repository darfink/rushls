//! FFmpeg-backed pass-through CMAF packaging.

mod ffi;

use std::{
    num::{NonZero, NonZeroUsize},
    sync::Arc,
    time::Duration,
};

use crate::{
    domain::{
        Appender, Codec, DiscoveredTrack, MediaKind, MediaParameters, TickDuration, TickTimestamp,
        TrackId, duration_since,
    },
    media::{NormalizedSample, PresentedTiming, PresentedTimingCursor},
    observe::{EventSink, SessionEvent},
    segment::TrackSegmentationPlan,
};

use super::{
    FinishReason, InitializationSegment, MediaSegmentFormat, MuxError, PackagedChunk,
    PackagedMedia, PackagedRendition, PackagedSegmentCompletion, PackagingRenditionId,
    PackagingSegmentId, RenditionConfig, RenditionKey, RenditionMedia, TrackPackager,
};
use ffi::FormatOutput;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SegmentBoundaryPolicy {
    #[default]
    Strict,
    /// Waits past the planned boundary for a random-access sample, by at most
    /// `maximum_extension`.
    ///
    /// The bound is mandatory rather than advisory. HLS advertises one target
    /// duration for a playlist's whole life, and the parts of an over-long
    /// segment are published before its completion is known, so an unbounded
    /// wait would put media into playlists that the advertised target cannot
    /// cover and that no later rejection can withdraw. Exhausting the budget
    /// fails the publication instead.
    ExtendToRandomAccess { maximum_extension: Duration },
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

pub(super) fn build_track(
    rendition_id: PackagingRenditionId,
    track: &DiscoveredTrack,
    plan: TrackSegmentationPlan,
    config: CmafMuxerConfig,
    events: EventSink,
) -> Result<(PackagedRendition, Box<dyn TrackPackager>), MuxError> {
    if track.kind() == MediaKind::Subtitle {
        return Err(invalid("subtitle tracks require WebVTT output"));
    }
    if plan.track_id != track.id || plan.timebase != track.timebase {
        return Err(invalid(format!(
            "segmentation plan does not describe {}",
            track.id
        )));
    }
    let output = FormatOutput::open(track, config.io_buffer_size.get())
        .map_err(|error| invalid(error.to_string()))?;
    let rendition = packaged_rendition(rendition_id, track, &plan, config.segment_boundary_policy)?;
    let packager = CmafTrack::new(
        rendition_id,
        track,
        plan,
        output,
        config.segment_boundary_policy,
        events,
    )?;
    Ok((rendition, Box::new(packager)))
}

/// The chunk currently accumulating, if one is open.
///
/// One `Option` rather than three, because the three cannot be individually
/// absent: a fragment without an end is not a state this packager can be in,
/// and expressing it as one made `flush_fragment` carry an error for a case
/// that could not arise.
#[derive(Clone, Copy, Debug)]
struct OpenFragment {
    /// Where the chunk begins within the segment. Taken from accumulated media
    /// rather than the first sample's timestamp, so consecutive chunks are
    /// contiguous by construction — which is what the store's continuity check
    /// requires.
    start: TickTimestamp,
    /// Furthest media end observed. Reordering can only move this forward.
    end: TickTimestamp,
    independent: bool,
}

/// Where the open segment starts and how far it has filled.
#[derive(Clone, Copy, Debug)]
struct SegmentCursor {
    id: u64,
    chunk_index: u32,
    start: TickTimestamp,
    /// Media accumulated by the chunks already emitted for this segment.
    filled: TickDuration,
    next_boundary: TickTimestamp,
    /// Presentable access units accumulated into the open chunk.
    ///
    /// Parts are counted rather than scheduled because HLS refuses a part
    /// longer than the advertised `PART-TARGET`, and a target the plan derived
    /// from an access-unit count is one a count can never exceed.
    part_access_units: u32,
}

impl SegmentCursor {
    fn new(plan: &TrackSegmentationPlan) -> Result<Self, MuxError> {
        let start = plan
            .segmentation_origin_pts
            .checked_sub(plan.presentation_origin_pts)
            .ok_or_else(|| invalid("segmentation origin rebasing overflowed"))?;
        let next_boundary = plan
            .first_segment_boundary_pts
            .checked_sub(plan.presentation_origin_pts)
            .ok_or_else(|| invalid("first segment boundary rebasing overflowed"))?;
        Ok(Self {
            id: 0,
            chunk_index: 0,
            start,
            filled: 0,
            next_boundary,
            part_access_units: 0,
        })
    }

    /// Where the media accumulated so far ends.
    fn filled_to(&self) -> Result<TickTimestamp, MuxError> {
        self.start
            .checked_add_unsigned(self.filled)
            .ok_or_else(|| mux_error("segment timing overflowed"))
    }

    /// Opens the next segment at the boundary the closed one actually reached.
    ///
    /// Planned boundaries remain on this track's absolute locked schedule.
    ///
    /// Re-anchoring on an achieved cut would accumulate an occasional late
    /// audio unit or video keyframe into permanent drift. Keeping the planned
    /// schedule makes the following segment absorb that bounded quantization.
    fn advance(&mut self, plan: &TrackSegmentationPlan) -> Result<(), MuxError> {
        let boundary = self.filled_to()?;
        let next_boundary = self
            .next_boundary
            .checked_add_unsigned(plan.segment_duration.get())
            .ok_or_else(|| mux_error("next segment boundary overflowed"))?;
        self.id = self.id.saturating_add(1);
        self.chunk_index = 0;
        self.start = boundary;
        self.filled = 0;
        self.next_boundary = next_boundary;
        Ok(())
    }
}

/// CMAF packaging state for one track.
struct CmafTrack {
    rendition_id: PackagingRenditionId,
    track_id: TrackId,
    codec: Codec,
    kind: MediaKind,
    plan: TrackSegmentationPlan,
    output: FormatOutput,
    policy: SegmentBoundaryPolicy,
    events: EventSink,
    segment: SegmentCursor,
    fragment: Option<OpenFragment>,
    presented_timing: PresentedTimingCursor,
    initialized: bool,
    last_dts: Option<TickTimestamp>,
    finished: bool,
}

impl CmafTrack {
    fn new(
        rendition_id: PackagingRenditionId,
        track: &DiscoveredTrack,
        plan: TrackSegmentationPlan,
        output: FormatOutput,
        policy: SegmentBoundaryPolicy,
        events: EventSink,
    ) -> Result<Self, MuxError> {
        Ok(Self {
            rendition_id,
            track_id: track.id,
            codec: track.codec,
            kind: track.kind(),
            plan,
            output,
            policy,
            events,
            segment: SegmentCursor::new(&plan)?,
            fragment: None,
            presented_timing: PresentedTimingCursor::for_track(track),
            initialized: false,
            last_dts: None,
            finished: false,
        })
    }

    fn push(
        &mut self,
        sample: &NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        let (pts, dts, presented) = self.rebase_sample(sample)?;
        let presented_pts = presented
            .start
            .checked_sub(self.plan.presentation_origin_pts)
            .ok_or_else(|| {
                mux_error(format!(
                    "presentation timing rebasing overflowed for {}",
                    self.track_id
                ))
            })?;
        if presented.duration == 0 {
            // A fully trimmed access unit still carries codec priming into
            // movenc, but it must not advance delivery-visible chunk timing.
            return self
                .output
                .write(sample, pts, dts)
                .map_err(|error| mux_error(error.to_string()));
        }

        let closes_segment = self.segment_boundary(sample, presented_pts)?;
        if closes_segment {
            self.close_fragment_at(presented_pts);
            self.flush_fragment(out)?;
            if self.segment.filled == 0 {
                return Err(mux_error(format!(
                    "{} reached an empty segment boundary",
                    self.track_id
                )));
            }
            out.push(PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
                rendition_id: self.rendition_id,
                packaging_segment_id: PackagingSegmentId(self.segment.id),
                media_start: self.segment.start,
                duration: self.segment.filled,
            }));
            self.segment.advance(&self.plan)?;
        } else if self.segment.part_access_units >= self.plan.part_access_units.get()
            && self.fragment.is_some()
        {
            // Audio is in presentation order, so the next PTS is a truthful
            // cut point. Video is in decode order: a reference picture may
            // have a later PTS than B-pictures that follow it, so extending a
            // part to the next packet's PTS can exceed the target despite the
            // part containing exactly the planned number of access units.
            if self.kind != MediaKind::Video {
                self.close_fragment_at(presented_pts);
            }
            self.flush_fragment(out)?;
        }

        self.open_or_extend_fragment(sample, presented_pts, presented.duration)?;
        // Counted after the sample joins the chunk, so the count always
        // describes what the open chunk holds.
        self.segment.part_access_units = self.segment.part_access_units.saturating_add(1);
        self.output
            .write(sample, pts, dts)
            .map_err(|error| mux_error(error.to_string()))
    }

    fn rebase_sample(
        &mut self,
        sample: &NormalizedSample,
    ) -> Result<(TickTimestamp, TickTimestamp, PresentedTiming), MuxError> {
        if sample_codec(sample) != self.codec {
            return Err(mux_error(format!(
                "{} changed codec while muxing",
                self.track_id
            )));
        }
        let pts = sample
            .pts()
            .checked_sub(self.plan.presentation_origin_pts)
            .ok_or_else(|| mux_error(format!("PTS rebasing overflowed for {}", self.track_id)))?;
        let dts = sample_dts(sample)
            .checked_sub(self.plan.presentation_origin_pts)
            .ok_or_else(|| mux_error(format!("DTS rebasing overflowed for {}", self.track_id)))?;
        if self.last_dts.is_some_and(|last| dts < last) {
            return Err(mux_error(format!(
                "{} supplied decreasing DTS",
                self.track_id
            )));
        }
        self.last_dts = Some(dts);
        let presented = self
            .presented_timing
            .next(sample)
            .map_err(|error| mux_error(format!("invalid timing for {}: {error}", self.track_id)))?;
        Ok((pts, dts, presented))
    }

    fn open_or_extend_fragment(
        &mut self,
        sample: &NormalizedSample,
        presented_pts: TickTimestamp,
        presented_duration: TickDuration,
    ) -> Result<(), MuxError> {
        let end = presented_pts
            .checked_add_unsigned(presented_duration)
            .ok_or_else(|| mux_error(format!("sample end overflowed for {}", self.track_id)))?;
        match &mut self.fragment {
            Some(fragment) if self.kind == MediaKind::Video => {
                fragment.end = fragment
                    .end
                    .checked_add_unsigned(presented_duration)
                    .ok_or_else(|| {
                        mux_error(format!("chunk duration overflowed for {}", self.track_id))
                    })?;
            }
            Some(fragment) => fragment.end = fragment.end.max(end),
            none => {
                let start = self.segment.filled_to()?;
                *none = Some(OpenFragment {
                    start,
                    end: if self.kind == MediaKind::Video {
                        start
                            .checked_add_unsigned(presented_duration)
                            .ok_or_else(|| {
                                mux_error(format!(
                                    "chunk duration overflowed for {}",
                                    self.track_id
                                ))
                            })?
                    } else {
                        end
                    },
                    independent: sample.random_access(),
                });
            }
        }
        Ok(())
    }

    /// Closes the delivery interval at the access unit that triggers a cut.
    ///
    /// Packet timestamps remain untouched in FFmpeg. This only defines the
    /// delivery-visible interval so consecutive chunks meet at the cut even
    /// when sparse or imperfect input timing leaves no access unit covering its
    /// final ticks. PTS reordering is harmless because the end only advances.
    fn close_fragment_at(&mut self, presented_pts: TickTimestamp) {
        if let Some(fragment) = &mut self.fragment {
            fragment.end = fragment.end.max(presented_pts);
        }
    }

    fn segment_boundary(
        &mut self,
        sample: &NormalizedSample,
        pts: TickTimestamp,
    ) -> Result<bool, MuxError> {
        if pts < self.segment.next_boundary {
            return Ok(false);
        }
        let exact = pts == self.segment.next_boundary;
        let random_access = sample.random_access();
        let maximum_extension = match self.policy {
            SegmentBoundaryPolicy::Strict => None,
            SegmentBoundaryPolicy::ExtendToRandomAccess { maximum_extension } => {
                // Checked before either cutting or extending: a cut beyond the
                // budget is as unpublishable as a wait beyond it, because both
                // produce a segment longer than the advertised target.
                self.require_within_extension(pts, maximum_extension)?;
                Some(maximum_extension)
            }
        };
        if self.kind != MediaKind::Video {
            // Variable-duration audio may not repeat its observed grid exactly.
            // Cut at the first AU on or after the planned instant; the plan's
            // boundary tolerance bounds the resulting quantization.
            return Ok(true);
        }
        match (maximum_extension, random_access) {
            (None, true) if exact => Ok(true),
            (None, _) => Err(mux_error(format!(
                "{} did not provide a random-access sample at its planned segment boundary",
                self.track_id
            ))),
            (Some(_), true) => {
                if pts > self.segment.next_boundary {
                    let actual_ticks = pts
                        .checked_sub(self.segment.start)
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
            (Some(_), false) => Ok(false),
        }
    }

    /// Rejects a boundary the advertised target duration could not cover.
    ///
    /// The limit is the planned boundary plus the configured extension, which
    /// is exactly the `maximum_segment_duration` this rendition advertises.
    /// Failing here is deliberate: by the time an over-long segment completes,
    /// its parts are already fetchable, so there is no later point at which
    /// the overrun can be contained.
    fn require_within_extension(
        &self,
        pts: TickTimestamp,
        maximum_extension: Duration,
    ) -> Result<(), MuxError> {
        let extension_ticks = self
            .plan
            .timebase
            .duration_to_ticks_floor(maximum_extension);
        let limit = self
            .segment
            .next_boundary
            .checked_add_unsigned(extension_ticks)
            .ok_or_else(|| mux_error("segment extension limit overflowed"))?;
        if pts > limit {
            return Err(mux_error(format!(
                "{} found no random-access sample within its {maximum_extension:?} \
                 maximum segment extension",
                self.track_id
            )));
        }
        Ok(())
    }

    fn flush_fragment(&mut self, out: &mut dyn Appender<PackagedMedia>) -> Result<(), MuxError> {
        let Some(fragment) = self.fragment.take() else {
            return Ok(());
        };
        let duration = duration_since(fragment.end, fragment.start)
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
            packaging_segment_id: PackagingSegmentId(self.segment.id),
            chunk_index: self.segment.chunk_index,
            media_start: fragment.start,
            duration,
            independent: fragment.independent,
            payload,
        }));
        self.segment.chunk_index = self
            .segment
            .chunk_index
            .checked_add(1)
            .ok_or_else(|| mux_error("chunk index overflowed"))?;
        self.segment.part_access_units = 0;
        self.segment.filled = self
            .segment
            .filled
            .checked_add(duration)
            .ok_or_else(|| mux_error("segment duration overflowed"))?;
        Ok(())
    }
}

impl TrackPackager for CmafTrack {
    fn track_id(&self) -> TrackId {
        self.track_id
    }

    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        CmafTrack::push(self, &sample, out)
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
            if self.segment.filled > 0 {
                out.push(PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
                    rendition_id: self.rendition_id,
                    packaging_segment_id: PackagingSegmentId(self.segment.id),
                    media_start: self.segment.start,
                    duration: self.segment.filled,
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
    policy: SegmentBoundaryPolicy,
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
    let first_segment_target = duration_since(
        plan.first_segment_boundary_pts,
        plan.segmentation_origin_pts,
    )
    .ok_or_else(|| invalid("first segment target is invalid"))?;
    let maximum_target = first_segment_target.max(plan.segment_duration.get());
    let mut maximum_segment_duration = maximum_target
        .checked_add(plan.boundary_tolerance)
        .ok_or_else(|| invalid("maximum segment duration overflowed"))?;
    if let SegmentBoundaryPolicy::ExtendToRandomAccess { maximum_extension } = policy {
        maximum_segment_duration = maximum_segment_duration
            .checked_add(plan.timebase.duration_to_ticks_floor(maximum_extension))
            .ok_or_else(|| invalid("maximum segment duration overflowed"))?;
    }
    let maximum_segment_duration = NonZero::new(maximum_segment_duration)
        .ok_or_else(|| invalid("maximum segment duration is zero"))?;
    let fallback_name = format!("{:?} {}", track.kind(), rendition_id.0 + 1);
    Ok(PackagedRendition {
        packaging_rendition_id: rendition_id,
        key: RenditionKey::for_source(track),
        source_tracks: Arc::from([track.id]),
        config: RenditionConfig {
            timebase: plan.timebase,
            segment_target: plan.segment_duration,
            maximum_segment_duration,
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
        num::NonZero,
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use parking_lot::Mutex;

    use crate::{
        admission::StreamPolicy,
        domain::{
            AudioTiming, AudioTrim, FrameRate, MediaKind, MediaParameters, Payload, SessionId,
            Timebase, TrackId,
            fixtures::{TrackBuilder, catalog},
        },
        media::{
            NormalizedSample, NormalizerFactory, PassThroughNormalizerFactory, VideoSample,
            calibrate, validate,
        },
        mux::{
            InitializationSegment, MuxerStartRequest, PackagedMedia, SegmentBoundaryPolicy,
            fixtures::{
                AAC_EXTRADATA, AAC_FRAME, AAC_FRAME_SAMPLES, H264_EXTRADATA, H264_IDR, H264_P,
            },
        },
        observe::{EventObserver, Events, SessionEvent},
        segment::{SegmentationPlan, fixtures::PlanBuilder},
        source::{
            DiscoveryLimits, DiscoveryReport, InputLimits, Packet, PacketSource,
            avformat::{AvformatConfig, AvformatPacketSource, ReadInput},
        },
    };

    use super::CmafMuxerConfig;
    use crate::mux::{MuxerFactory, PassThroughMuxerFactory};

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
                frame_rate: Some(FrameRate::new(nz::u32!(2), nz::u32!(1))),
                video_delay: 0,
            })
            .codec_extradata(H264_EXTRADATA.to_vec())
            .build()
    }

    fn concat_cmaf_bytes(media: &[PackagedMedia]) -> Vec<u8> {
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
        bytes
    }

    fn collect_rendition_bytes(media: &[PackagedMedia]) -> [Vec<u8>; 2] {
        let mut outputs = [Vec::new(), Vec::new()];
        let mut initialized = [false, false];
        for event in media {
            match event {
                PackagedMedia::Initialization(initialization) => {
                    let index = usize::try_from(initialization.rendition_id.0)
                        .expect("fixture rendition id fits usize");
                    initialized[index] = true;
                    outputs[index].extend_from_slice(initialization.payload.as_bytes());
                }
                PackagedMedia::Chunk(chunk) => {
                    let index = usize::try_from(chunk.rendition_id.0)
                        .expect("fixture rendition id fits usize");
                    assert!(
                        initialized[index],
                        "rendition initialization precedes its media"
                    );
                    outputs[index].extend_from_slice(chunk.payload.as_bytes());
                }
                PackagedMedia::Segment(_) | PackagedMedia::SegmentCompleted(_) => {}
            }
        }
        outputs
    }

    async fn demux_cmaf_packets(bytes: Vec<u8>) -> (DiscoveryReport, Vec<Packet>) {
        let session = crate::observe::SessionMeters::new(crate::observe::ProcessMeters::default());
        let mut source = AvformatPacketSource::new(
            Box::new(ReadInput::closed(Cursor::new(bytes))),
            AvformatConfig::default(),
            InputLimits::permissive(),
            session.source_view(),
        )
        .expect("CMAF source starts");
        let discovery = source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 1024 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("CMAF output is discoverable");
        let mut packets = Vec::new();
        while source
            .fill(&mut packets)
            .await
            .expect("CMAF packets demux")
            .is_open()
        {}
        (discovery, packets)
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

    fn audio_sample(pts: i64) -> NormalizedSample {
        NormalizedSample::Audio(crate::media::AudioSample {
            track_id: TrackId(0),
            codec: crate::domain::Codec::Aac,
            pts,
            duration: AAC_FRAME_SAMPLES,
            trim: crate::domain::AudioTrim::default(),
            payload: Payload::from(AAC_FRAME.to_vec()),
        })
    }

    /// The video schedule every test starts from: 16 384-tick segments cut
    /// into two parts, on the ordinary grid.
    ///
    /// Returned unbuilt so a test can override the one field it is about.
    fn video_plan(track_id: u32, timebase: Timebase) -> PlanBuilder {
        PlanBuilder::new(track_id, timebase, nz::u64!(16_384)).part(nz::u32!(1), nz::u64!(8_192))
    }

    /// The audio equivalent: whole AAC frames, `frames` of them per segment,
    /// `frames_per_part` to a part.
    fn audio_plan(timebase: Timebase, frames: u64, frames_per_part: u32) -> PlanBuilder {
        PlanBuilder::new(
            0,
            timebase,
            NonZero::new(frames * AAC_FRAME_SAMPLES).expect("a segment spans whole frames"),
        )
        .part(
            NonZero::new(frames_per_part).expect("a part spans whole frames"),
            NonZero::new(u64::from(frames_per_part) * AAC_FRAME_SAMPLES).expect("nonzero"),
        )
    }

    /// Plans these tracks and starts the default pass-through muxer over them.
    ///
    /// The pair is always taken together — a schedule exists to be muxed to —
    /// and both failures are fixture bugs rather than anything under test, so
    /// neither is worth restating at a dozen call sites.
    fn started(
        input: &crate::media::PresentationPlan,
        plans: Vec<crate::segment::TrackSegmentationPlan>,
        events: &crate::observe::EventSink,
    ) -> crate::mux::StartedMuxer {
        let segmentation =
            SegmentationPlan::new(input, plans).expect("fixture segmentation validates");
        PassThroughMuxerFactory::default()
            .start(MuxerStartRequest {
                presentation: input,
                segmentation: &segmentation,
                time_anchor: SystemTime::UNIX_EPOCH,
                events,
            })
            .expect("the fixture CMAF output starts")
    }

    fn start(
        policy: SegmentBoundaryPolicy,
        timebase: Timebase,
        events: &crate::observe::EventSink,
    ) -> Result<crate::mux::StartedMuxer, crate::mux::MuxError> {
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())
            .expect("fixture presentation validates");
        let segmentation = SegmentationPlan::new(&input, vec![video_plan(0, timebase).build()])
            .expect("fixture segmentation validates");
        PassThroughMuxerFactory::new(CmafMuxerConfig {
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

    fn start_audio(
        timing: AudioTiming,
        segmentation_origin_pts: i64,
        events: &crate::observe::EventSink,
    ) -> crate::mux::StartedMuxer {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(timebase)
            .parameters(MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(1),
                frame_size: Some(nz::u32!(1_024)),
                bit_depth: None,
                timing,
            })
            .codec(crate::domain::Codec::Aac)
            .codec_extradata(AAC_EXTRADATA.to_vec())
            .build();
        let input = validate(&catalog(vec![audio]), &StreamPolicy::permissive())
            .expect("audio fixture validates");
        started(
            &input,
            vec![
                audio_plan(timebase, 8, 2)
                    .segmentation_origin(segmentation_origin_pts)
                    .build(),
            ],
            events,
        )
    }

    fn trimmed_audio_sample(pts: i64, trim: AudioTrim) -> NormalizedSample {
        NormalizedSample::Audio(crate::media::AudioSample {
            track_id: TrackId(0),
            codec: crate::domain::Codec::Aac,
            pts,
            duration: AAC_FRAME_SAMPLES,
            trim,
            payload: Payload::from(AAC_FRAME.to_vec()),
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

    fn edit_list(payload: &Payload) -> Vec<(u64, i64)> {
        let bytes = payload.as_bytes();
        let Some(type_offset) = bytes.windows(4).position(|window| window == b"elst") else {
            return Vec::new();
        };
        let box_start = type_offset
            .checked_sub(4)
            .expect("box type follows its size");
        let box_size = u32::from_be_bytes(
            bytes[box_start..type_offset]
                .try_into()
                .expect("box size is four bytes"),
        ) as usize;
        let box_end = box_start + box_size;
        assert!(box_size >= 16 && box_end <= bytes.len());
        let body = &bytes[type_offset + 4..box_end];
        let version = body[0];
        let entries = u32::from_be_bytes(body[4..8].try_into().expect("entry count is four bytes"));
        let mut cursor = 8;
        (0..entries)
            .map(|_| match version {
                0 => {
                    let duration = u64::from(u32::from_be_bytes(
                        body[cursor..cursor + 4]
                            .try_into()
                            .expect("duration is four bytes"),
                    ));
                    let media_time = i64::from(i32::from_be_bytes(
                        body[cursor + 4..cursor + 8]
                            .try_into()
                            .expect("media time is four bytes"),
                    ));
                    cursor += 12;
                    (duration, media_time)
                }
                1 => {
                    let duration = u64::from_be_bytes(
                        body[cursor..cursor + 8]
                            .try_into()
                            .expect("duration is eight bytes"),
                    );
                    let media_time = i64::from_be_bytes(
                        body[cursor + 8..cursor + 16]
                            .try_into()
                            .expect("media time is eight bytes"),
                    );
                    cursor += 20;
                    (duration, media_time)
                }
                _ => panic!("unsupported edit-list version"),
            })
            .collect()
    }

    #[test]
    fn a_timestamp_gap_at_a_boundary_keeps_delivery_timing_contiguous() {
        let sink = event_sink();
        let mut started = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &sink,
        )
        .expect("CMAF muxer starts");

        let mut media = Vec::new();
        // Frame 8_192 is absent. The boundary sample still closes segment zero;
        // packet timestamps remain sparse while delivery timing spans the full
        // interval up to the cut.
        for sample in [sample(0, true), sample(16_384, true)] {
            started
                .muxer
                .push(sample, &mut media)
                .expect("sparse timestamps remain packageable");
        }

        assert!(matches!(
            media.as_slice(),
            [
                PackagedMedia::Initialization(_),
                PackagedMedia::Chunk(chunk),
                PackagedMedia::SegmentCompleted(completion),
            ] if chunk.media_start == 0
                && chunk.duration == 16_384
                && completion.media_start == 0
                && completion.duration == 16_384
        ));
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
            .push(
                sample(i64::try_from(FRAME).expect("fixture frame fits i64"), false),
                &mut media,
            )
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
        assert_ne!(
            edit_list(initialization).first().map(|entry| entry.1),
            Some(-1),
            "a video track starting at presentation zero needs no empty edit"
        );
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
    fn reordered_video_parts_are_measured_on_the_decode_timeline() {
        let sink = event_sink();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(16_384));
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())
            .expect("fixture presentation validates");
        let mut started = started(
            &input,
            vec![
                PlanBuilder::new(0, timebase, NonZero::new(16 * FRAME).expect("nonzero"))
                    .part(nz::u32!(1), NonZero::new(FRAME).expect("nonzero"))
                    .build(),
            ],
            &sink,
        );
        let frame = i64::try_from(FRAME).expect("fixture duration fits");
        let mut media = Vec::new();

        // The second decoded access unit is a future reference picture. Its
        // PTS must neither stretch the preceding part nor make its own part
        // exceed the one-access-unit target.
        for sample in [
            sample_with_dts(0, 0, -3 * frame, true),
            sample_with_dts(0, 4 * frame, -2 * frame, false),
            sample_with_dts(0, frame, -frame, false),
        ] {
            started
                .muxer
                .push(sample, &mut media)
                .expect("reordered sample packages");
        }

        let durations: Vec<_> = media
            .iter()
            .filter_map(|event| match event {
                PackagedMedia::Chunk(chunk) => Some(chunk.duration),
                _ => None,
            })
            .collect();
        assert_eq!(durations, [FRAME, FRAME]);
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
        let mut started = started(
            &input,
            [0, 1]
                .into_iter()
                .map(|track_id| video_plan(track_id, timebase).build())
                .collect(),
            &sink,
        );
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

    #[test]
    fn audio_segments_repeat_on_the_access_unit_grid() {
        // Exact AAC-aligned schedules should retain exact starts and durations;
        // the absolute-schedule test below covers the non-aligned case.
        let sink = event_sink();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let frames_per_segment = 16_u64;
        let segment_ticks = AAC_FRAME_SAMPLES * frames_per_segment;
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(timebase)
            .codec(crate::domain::Codec::Aac)
            .codec_extradata(AAC_EXTRADATA.to_vec())
            .build();
        let input = validate(&catalog(vec![audio]), &StreamPolicy::permissive())
            .expect("audio fixture validates");
        let mut started = started(
            &input,
            vec![audio_plan(timebase, frames_per_segment, 4).build()],
            &sink,
        );

        let mut media = Vec::new();
        // Three full segments: enough that a per-segment residue would have
        // accumulated past the second boundary.
        for frame in 0..(frames_per_segment * 3) {
            let pts = i64::try_from(frame * AAC_FRAME_SAMPLES).expect("fixture pts fits i64");
            started
                .muxer
                .push(audio_sample(pts), &mut media)
                .unwrap_or_else(|error| panic!("audio frame at {pts} packages: {error}"));
        }

        let completions: Vec<_> = media
            .iter()
            .filter_map(|event| match event {
                PackagedMedia::SegmentCompleted(completion) => Some(completion),
                _ => None,
            })
            .collect();
        assert_eq!(completions.len(), 2, "two boundaries are reached");
        for (index, completion) in completions.iter().enumerate() {
            assert_eq!(
                completion.media_start,
                i64::try_from(index).expect("fixture index fits i64")
                    * i64::try_from(segment_ticks).expect("fixture ticks fit i64"),
                "segment {index} starts on the grid"
            );
            assert_eq!(
                completion.duration, segment_ticks,
                "segment {index} spans exactly the planned period"
            );
        }
    }

    /// Drives one audio track and reports `(part durations, PART-TARGET)`.
    ///
    /// Access-unit durations are supplied per frame so a jittery cadence can be
    /// exercised alongside a constant one.
    fn packaged_parts(
        access_units: &[u64],
        part_access_units: NonZero<u32>,
        part_target: NonZero<u64>,
        segment_ticks: u64,
    ) -> (Vec<u64>, u64) {
        let sink = event_sink();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(timebase)
            .codec(crate::domain::Codec::Aac)
            .codec_extradata(AAC_EXTRADATA.to_vec())
            .build();
        let input = validate(&catalog(vec![audio]), &StreamPolicy::permissive())
            .expect("audio fixture validates");
        let mut started = started(
            &input,
            vec![
                PlanBuilder::new(0, timebase, NonZero::new(segment_ticks).expect("nonzero"))
                    .part(part_access_units, part_target)
                    // A jittery cadence needs room for its longest access unit.
                    .boundary_tolerance(access_units.iter().copied().max().unwrap_or(0))
                    .build(),
            ],
            &sink,
        );

        let mut media = Vec::new();
        let mut pts = 0_i64;
        for duration in access_units {
            let mut sample = audio_sample(pts);
            if let NormalizedSample::Audio(sample) = &mut sample {
                sample.duration = *duration;
            }
            started
                .muxer
                .push(sample, &mut media)
                .unwrap_or_else(|error| panic!("audio frame at {pts} packages: {error}"));
            pts += i64::try_from(*duration).expect("fixture duration fits");
        }

        let durations = media
            .iter()
            .filter_map(|event| match event {
                PackagedMedia::Chunk(chunk) => Some(chunk.duration),
                _ => None,
            })
            .collect();
        (
            durations,
            started.presentation.renditions[0]
                .config
                .chunk_target
                .expect("a CMAF audio rendition publishes parts")
                .get(),
        )
    }

    #[test]
    fn every_non_final_part_lands_between_85_and_100_percent_of_the_target() {
        // The rule delivery enforces, and the reason parts are counted in
        // access units rather than scheduled on ticks: a tick schedule cuts at
        // the first unit past the planned instant, so it can overshoot the
        // advertised target by a whole access unit and be refused.
        let uniform = vec![AAC_FRAME_SAMPLES; 24];
        // A three-unit jitter cycle against a four-unit part, so the two stay
        // permanently out of phase and consecutive parts genuinely differ. A
        // target derived from the mean would be exceeded by every window that
        // happens to hold only one short unit.
        let jittery: Vec<u64> = (0..24)
            .map(|index| if index % 3 == 0 { 900 } else { 1_000 })
            .collect();

        for (access_units, target, expected) in [
            (uniform, nz::u64!(4 * AAC_FRAME_SAMPLES), vec![4_096_u64; 5]),
            // Alternating windows, and the wider one is what the target has to
            // cover — 3900 against a 4000 target, with 3800 still at 95%.
            (
                jittery,
                nz::u64!(4_000),
                vec![3_800, 3_900, 3_900, 3_800, 3_900],
            ),
        ] {
            let (parts, part_target) =
                packaged_parts(&access_units, nz::u32!(4), target, 24 * 1_024);
            let minimum = part_target * 85 / 100;

            assert_eq!(parts, expected);
            // The last part is the one a segment boundary or the end of input
            // truncated, and HLS exempts exactly that one from the floor.
            for (index, duration) in parts[..parts.len() - 1].iter().enumerate() {
                assert!(
                    *duration <= part_target,
                    "part {index} of {duration} exceeds the advertised \
                     PART-TARGET of {part_target}"
                );
                assert!(
                    *duration >= minimum,
                    "part {index} of {duration} is below the 85% floor of \
                     {minimum} for a PART-TARGET of {part_target}"
                );
            }
        }
    }

    #[test]
    fn part_counting_restarts_at_every_segment_boundary() {
        // Five units to a segment against a four-unit part: each segment ends
        // with a short part, and the next segment's first part must be a full
        // four units rather than continuing the previous count.
        let (parts, _) = packaged_parts(
            &[AAC_FRAME_SAMPLES; 15],
            nz::u32!(4),
            nz::u64!(4 * AAC_FRAME_SAMPLES),
            5 * AAC_FRAME_SAMPLES,
        );

        assert_eq!(
            parts,
            [
                4 * AAC_FRAME_SAMPLES,
                AAC_FRAME_SAMPLES,
                4 * AAC_FRAME_SAMPLES,
                AAC_FRAME_SAMPLES,
                4 * AAC_FRAME_SAMPLES,
            ]
        );
    }

    #[test]
    fn audio_boundary_quantization_does_not_move_the_planned_grid() {
        let sink = event_sink();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(timebase)
            .codec(crate::domain::Codec::Aac)
            .codec_extradata(AAC_EXTRADATA.to_vec())
            .build();
        let input = validate(&catalog(vec![audio]), &StreamPolicy::permissive())
            .expect("audio fixture validates");
        let mut started = started(
            &input,
            vec![
                // Deliberately off the access-unit grid: 1_900 is not a
                // multiple of the 1_500-tick period, which is what this test
                // is about.
                PlanBuilder::new(0, timebase, nz::u64!(1_500))
                    .part(nz::u32!(1), nz::u64!(500))
                    .first_boundary(1_900)
                    .boundary_tolerance(AAC_FRAME_SAMPLES)
                    .build(),
            ],
            &sink,
        );

        let mut media = Vec::new();
        for frame in 0..=5 {
            let pts = i64::try_from(frame * AAC_FRAME_SAMPLES).expect("fixture PTS fits");
            started
                .muxer
                .push(audio_sample(pts), &mut media)
                .unwrap_or_else(|error| panic!("audio frame at {pts} packages: {error}"));
        }

        let completions: Vec<_> = media
            .iter()
            .filter_map(|event| match event {
                PackagedMedia::SegmentCompleted(completion) => {
                    Some((completion.media_start, completion.duration))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            completions,
            [(0, 2_048), (2_048, 2_048), (4_096, 1_024)],
            "each cut rounds independently from the absolute 1900 + n*1500 schedule"
        );
    }

    #[tokio::test]
    async fn audio_priming_is_muxed_but_excluded_from_chunk_timing() {
        let sink = event_sink();
        let mut started = start_audio(
            AudioTiming {
                initial_padding_samples: 1_024,
                ..AudioTiming::default()
            },
            0,
            &sink,
        );
        let mut media = Vec::new();
        for sample in [
            trimmed_audio_sample(
                -1_024,
                AudioTrim {
                    leading_samples: 1_024,
                    trailing_samples: 0,
                },
            ),
            trimmed_audio_sample(0, AudioTrim::default()),
            trimmed_audio_sample(1_024, AudioTrim::default()),
            trimmed_audio_sample(2_048, AudioTrim::default()),
        ] {
            started
                .muxer
                .push(sample, &mut media)
                .expect("primed AAC packages");
        }

        let initialization = media.iter().find_map(|event| match event {
            PackagedMedia::Initialization(initialization) => Some(initialization),
            _ => None,
        });
        let initialization = initialization.expect("delayed initialization is emitted");
        assert!(
            initialization
                .payload
                .as_bytes()
                .windows(4)
                .any(|window| window == b"edts")
        );
        assert!(
            initialization
                .payload
                .as_bytes()
                .windows(4)
                .any(|window| window == b"elst")
        );
        assert_eq!(
            edit_list(&initialization.payload)
                .first()
                .map(|entry| entry.1),
            Some(1_024),
            "the edit list selects past the encoded priming frame"
        );
        assert!(matches!(
            media.iter().find(|event| matches!(event, PackagedMedia::Chunk(_))),
            Some(PackagedMedia::Chunk(chunk))
                if chunk.media_start == 0 && chunk.duration == 2_048
        ));
        assert_priming_round_trip(&media).await;
    }

    async fn assert_priming_round_trip(media: &[PackagedMedia]) {
        let bytes = concat_cmaf_bytes(media);
        let (discovery, packets) = demux_cmaf_packets(bytes).await;
        assert_eq!(discovery.tracks.tracks()[0].first_pts, Some(0));
        assert_eq!(
            packets.first().map(|packet| packet.audio_trim),
            Some(AudioTrim {
                leading_samples: 1_024,
                trailing_samples: 0,
            })
        );
    }

    #[test]
    fn audio_priming_spanning_access_units_keeps_packet_metadata_and_grid_timing() {
        let sink = event_sink();
        let mut started = start_audio(
            AudioTiming {
                initial_padding_samples: 2_112,
                ..AudioTiming::default()
            },
            -64,
            &sink,
        );
        let mut media = Vec::new();
        for sample in [
            trimmed_audio_sample(
                -2_112,
                AudioTrim {
                    leading_samples: 2_112,
                    trailing_samples: 0,
                },
            ),
            trimmed_audio_sample(-1_088, AudioTrim::default()),
            trimmed_audio_sample(-64, AudioTrim::default()),
            trimmed_audio_sample(960, AudioTrim::default()),
            trimmed_audio_sample(1_984, AudioTrim::default()),
        ] {
            started
                .muxer
                .push(sample, &mut media)
                .expect("multi-frame AAC priming packages");
        }

        let initialization = media
            .iter()
            .find_map(|event| match event {
                PackagedMedia::Initialization(initialization) => Some(initialization),
                _ => None,
            })
            .expect("delayed initialization is emitted");
        assert_eq!(
            edit_list(&initialization.payload)
                .first()
                .map(|entry| entry.1),
            Some(2_112),
            "movenc receives the original whole skip count"
        );
        assert!(matches!(
            media.iter().find(|event| matches!(event, PackagedMedia::Chunk(_))),
            Some(PackagedMedia::Chunk(chunk))
                if chunk.media_start == -64 && chunk.duration == 2_048
        ));
    }

    #[test]
    fn trailing_audio_padding_is_excluded_from_final_delivery_timing() {
        let sink = event_sink();
        let mut started = start_audio(
            AudioTiming {
                trailing_padding_samples: 24,
                ..AudioTiming::default()
            },
            0,
            &sink,
        );
        let mut media = Vec::new();
        started
            .muxer
            .push(
                trimmed_audio_sample(
                    0,
                    AudioTrim {
                        leading_samples: 0,
                        trailing_samples: 24,
                    },
                ),
                &mut media,
            )
            .expect("trimmed tail packages");
        started
            .muxer
            .finish(crate::mux::FinishReason::Final, &mut media)
            .expect("tail finishes");

        assert!(matches!(
            media.as_slice(),
            [
                PackagedMedia::Initialization(_),
                PackagedMedia::Chunk(chunk),
                PackagedMedia::SegmentCompleted(segment)
            ] if chunk.media_start == 0
                && chunk.duration == 1_000
                && segment.media_start == 0
                && segment.duration == 1_000
        ));
    }

    #[test]
    fn a_genuine_later_video_start_is_exposed_as_positive_media_start() {
        let sink = event_sink();
        let timebase = Timebase::hz90k();
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())
            .expect("fixture presentation validates");
        let mut started = started(
            &input,
            vec![video_plan(0, timebase).presentation_origin(-1_980).build()],
            &sink,
        );
        let mut media = Vec::new();
        for sample in [sample(0, true), sample(8_192, false)] {
            started
                .muxer
                .push(sample, &mut media)
                .expect("offset video packages");
        }

        assert!(matches!(
            media.as_slice(),
            [
                PackagedMedia::Initialization(initialization),
                PackagedMedia::Chunk(chunk)
            ] if initialization
                .payload
                .as_bytes()
                .windows(4)
                .any(|window| window == b"elst")
                && edit_list(&initialization.payload).first().map(|entry| entry.1)
                    == Some(-1)
                && chunk.media_start == 1_980
                && chunk.duration == FRAME
        ));
    }

    #[test]
    fn audio_and_video_keep_their_relative_offset_through_packaging() {
        // The one shape no other CMAF test covers: both kinds in one
        // publication, with different timebases and different start times. If a
        // per-track origin adjustment ever creeps back in, the two renditions
        // drift apart here and nowhere else.
        let sink = event_sink();
        let video_base = Timebase::new(nz::u32!(1), nz::u32!(16_384));
        let audio_base = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        // The shared instant is half a second before video's first frame, so
        // audio begins there and video half a second later.
        let video_origin = -8_192_i64;
        let mut video = track(video_base);
        video.id = TrackId(1);
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(audio_base)
            .parameters(MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(1),
                frame_size: Some(nz::u32!(1_024)),
                bit_depth: None,
                timing: AudioTiming::default(),
            })
            .codec(crate::domain::Codec::Aac)
            .codec_extradata(AAC_EXTRADATA.to_vec())
            .build();
        let input = validate(&catalog(vec![audio, video]), &StreamPolicy::permissive())
            .expect("audio and video fixture validates");
        let mut started = started(
            &input,
            vec![
                audio_plan(audio_base, 8, 2).build(),
                video_plan(1, video_base)
                    .presentation_origin(video_origin)
                    .build(),
            ],
            &sink,
        );

        let mut media = Vec::new();
        for frame in 0..4_i64 {
            started
                .muxer
                .push(
                    audio_sample(
                        frame * i64::try_from(AAC_FRAME_SAMPLES).expect("fixture samples fit i64"),
                    ),
                    &mut media,
                )
                .expect("audio packages");
        }
        // Two frames: enough to close video's first part without reaching its
        // segment boundary, which would need another keyframe.
        for frame in 0..2_i64 {
            started
                .muxer
                .push(sample_for(1, frame * 8_192, frame == 0), &mut media)
                .expect("video packages");
        }

        let first_chunk = |rendition: u32| {
            media
                .iter()
                .find_map(|event| match event {
                    PackagedMedia::Chunk(chunk)
                        if chunk.rendition_id == crate::mux::PackagingRenditionId(rendition) =>
                    {
                        Some(chunk)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("rendition {rendition} produced a chunk"))
        };
        let audio_start = audio_base.ticks_to_duration(
            u64::try_from(first_chunk(0).media_start).expect("audio starts at or after zero"),
        );
        let video_start = video_base.ticks_to_duration(
            u64::try_from(first_chunk(1).media_start).expect("video starts at or after zero"),
        );

        assert_eq!(
            audio_start,
            Duration::ZERO,
            "audio anchors the shared instant"
        );
        assert_eq!(
            video_start.checked_sub(audio_start).unwrap(),
            Duration::from_millis(500),
            "video's later start must survive packaging as delivery-visible timing"
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
        // `fill` yields one batch, and where that batch ends depends on how far
        // the blocking demux worker has run: it waits only for the first packet
        // and then takes whatever else is already queued. Draining to the end
        // of input is the only assertion the contract supports — a single call
        // is a race with the worker thread.
        while source
            .fill(&mut packets)
            .await
            .expect("CMAF packets demux")
            .is_open()
        {}
        assert_eq!(packets.len(), 4);
        assert!(packets[0].random_access);
    }

    #[tokio::test]
    async fn mpeg_ts_normalizes_and_packages_as_demuxable_cmaf() {
        let process = crate::observe::ProcessMeters::default();
        let session = crate::observe::SessionMeters::new(process);
        let mut source = AvformatPacketSource::new(
            Box::new(ReadInput::closed(Cursor::new(
                crate::source::avformat::fixtures::h264_adts_aac_mpeg_ts(),
            ))),
            AvformatConfig::default(),
            InputLimits::permissive(),
            session.source_view(),
        )
        .expect("MPEG-TS source starts");
        let discovery = source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 1024 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("MPEG-TS tracks are discoverable");
        let input = validate(&discovery.tracks, &StreamPolicy::permissive())
            .expect("fixture tracks are admitted");
        let timeline = calibrate(&input).expect("fixture timeline calibrates");
        let mut normalized = PassThroughNormalizerFactory
            .start(&input, &timeline)
            .expect("fixture normalization starts");

        let mut packets = Vec::new();
        while source
            .fill(&mut packets)
            .await
            .expect("MPEG-TS packets demux")
            .is_open()
        {}
        let mut samples = Vec::new();
        for packet in packets {
            normalized
                .normalizer
                .push(packet, &mut samples)
                .expect("MPEG-TS packet normalizes");
        }
        normalized
            .normalizer
            .finish(&mut samples)
            .expect("held video timing resolves at end of input");
        assert!(
            samples
                .iter()
                .any(|sample| matches!(sample, NormalizedSample::Video(_)))
                && samples
                    .iter()
                    .any(|sample| matches!(sample, NormalizedSample::Audio(_)))
        );

        let tracks = normalized
            .presentation
            .tracks()
            .iter()
            .map(|track| {
                let mut track_samples = samples
                    .iter()
                    .filter(|sample| sample.track_id() == track.id);
                let first = track_samples
                    .next()
                    .expect("each discovered track produced media");
                let longest = track_samples
                    .map(NormalizedSample::duration)
                    .fold(first.duration(), u64::max);
                let segment_duration = track
                    .timebase
                    .duration_to_ticks_ceil(Duration::from_secs(2));
                PlanBuilder::new(
                    track.id.0,
                    track.timebase,
                    NonZero::new(segment_duration).expect("two seconds is representable"),
                )
                .part(
                    nz::u32!(1),
                    NonZero::new(longest).expect("samples have duration"),
                )
                .presentation_origin(
                    normalized
                        .timeline
                        .get(track.id)
                        .expect("normalized track has a timeline")
                        .origin_pts,
                )
                .segmentation_origin(first.pts())
                .build()
            })
            .collect();
        let segmentation = SegmentationPlan::new(&normalized.presentation, tracks)
            .expect("fixture segmentation is valid");
        let sink = event_sink();
        let mut mux = PassThroughMuxerFactory::default()
            .start(MuxerStartRequest {
                presentation: &normalized.presentation,
                segmentation: &segmentation,
                time_anchor: SystemTime::UNIX_EPOCH,
                events: &sink,
            })
            .expect("MPEG-TS presentation packages");
        let mut media = Vec::new();
        for sample in samples {
            mux.muxer
                .push(sample, &mut media)
                .expect("normalized sample packages");
        }
        mux.muxer
            .finish(crate::mux::FinishReason::Final, &mut media)
            .expect("CMAF tails finish");

        let outputs = collect_rendition_bytes(&media);
        for (bytes, expected) in outputs
            .into_iter()
            .zip([crate::domain::Codec::H264, crate::domain::Codec::Aac])
        {
            assert!(!bytes.is_empty());
            let (discovery, packets) = demux_cmaf_packets(bytes).await;
            assert_eq!(discovery.tracks.tracks()[0].codec, expected);
            assert!(!packets.is_empty());
        }
    }

    #[test]
    fn rebases_pts_and_preserves_negative_dts_at_the_shared_origin() {
        let sink = event_sink();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(16_384));
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())
            .expect("fixture presentation validates");
        let mut started = started(
            &input,
            vec![
                video_plan(0, timebase)
                    .presentation_origin(1_000)
                    .segmentation_origin(1_000)
                    .build(),
            ],
            &sink,
        );
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
            SegmentBoundaryPolicy::ExtendToRandomAccess {
                maximum_extension: Duration::from_secs(1),
            },
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
    fn an_exhausted_extension_budget_fails_instead_of_overrunning_the_target() {
        let sink = event_sink();
        let mut started = start(
            SegmentBoundaryPolicy::ExtendToRandomAccess {
                maximum_extension: Duration::from_millis(500),
            },
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &sink,
        )
        .expect("CMAF muxer starts");

        assert_eq!(
            started.presentation.renditions[0]
                .config
                .maximum_segment_duration,
            nz::u64!(24_576),
            "the advertised maximum is the planned segment plus the whole budget, \
             so delivery can fix a target duration that covers every extension"
        );

        let mut media = Vec::new();
        for value in [
            sample(0, true),
            sample(8_192, false),
            sample(16_384, false),
            sample(24_576, false),
        ] {
            started
                .muxer
                .push(value, &mut media)
                .expect("waiting inside the budget is what extension mode is for");
        }

        assert!(
            matches!(
                started.muxer.push(sample(32_768, false), &mut media),
                Err(crate::mux::MuxError::Mux(_))
            ),
            "past the budget the publication fails: its parts are already \
             fetchable, so there is no later point at which an overrun could be \
             contained"
        );
    }

    #[test]
    fn accepts_webvtt_and_rejects_negotiated_cmaf_timebase_changes() {
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
            [0, 1]
                .into_iter()
                .map(|track_id| {
                    PlanBuilder::new(track_id, Timebase::hz90k(), nz::u64!(180_000))
                        .part(nz::u32!(1), nz::u64!(90_000))
                        .build()
                })
                .collect(),
        )
        .expect("fixture segmentation validates");
        let started = PassThroughMuxerFactory::default()
            .start(MuxerStartRequest {
                presentation: &subtitle_input,
                segmentation: &segmentation,
                time_anchor: SystemTime::UNIX_EPOCH,
                events: &sink,
            })
            .expect("mixed CMAF and WebVTT presentation starts");
        assert_eq!(
            started.presentation.renditions[1].config.segment_format,
            crate::mux::MediaSegmentFormat::WebVtt
        );
        assert_eq!(started.presentation.renditions[1].codecs.as_ref(), "wvtt");

        let changed = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(1_000)),
            &sink,
        );
        assert!(matches!(changed, Err(crate::mux::MuxError::InvalidPlan(_))));
    }
}
