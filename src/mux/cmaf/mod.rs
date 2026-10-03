//! Pass-through CMAF packaging.

mod output;
mod roll;

use std::{collections::VecDeque, num::NonZero, sync::Arc, time::Duration};

use crate::{
    domain::{
        Appender, Codec, DiscoveredTrack, MediaKind, MediaParameters, TickDuration, TickTimestamp,
        TrackId, duration_since,
    },
    media::{NormalizedMedia, PresentedTiming, PresentedTimingCursor},
    observe::EventSink,
    segment::TrackSegmentationPlan,
};

use super::{
    FinishReason, InitializationSegment, MediaSegmentFormat, MuxError, PackagedChunk,
    PackagedMedia, PackagedRendition, PackagedSegmentCompletion, PackagingRenditionId,
    PackagingSegmentId, RenditionConfig, RenditionKey, RenditionMedia, TrackPackager, VideoRange,
};
use output::CmafOutput;

pub(super) fn build_track(
    rendition_id: PackagingRenditionId,
    track: &DiscoveredTrack,
    plan: TrackSegmentationPlan,
    boundary_allowance: Duration,
    events: EventSink,
    budget: crate::domain::PipelineBudget,
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
    let output = CmafOutput::open(track, budget).map_err(|error| invalid(error.to_string()))?;
    let rendition = packaged_rendition(rendition_id, track, &plan, boundary_allowance)?;
    let packager = CmafTrack::new(
        rendition_id,
        track,
        plan,
        Some(output),
        boundary_allowance,
        events,
    )?;
    Ok((rendition, Box::new(packager)))
}

/// Runs exactly the live timing state machine without constructing container bytes.
pub fn timing_track(
    rendition_id: PackagingRenditionId,
    track: &DiscoveredTrack,
    plan: TrackSegmentationPlan,
    allowance: Duration,
    events: EventSink,
) -> Result<Box<dyn TrackPackager>, MuxError> {
    Ok(Box::new(CmafTrack::new(
        rendition_id,
        track,
        plan,
        None,
        allowance,
        events,
    )?))
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
    presentation_end: TickTimestamp,
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
}

impl SegmentCursor {
    fn new(plan: &TrackSegmentationPlan) -> Result<Self, MuxError> {
        let start = plan
            .segmentation_origin_pts
            .checked_sub(plan.presentation_origin_pts)
            .ok_or_else(|| invalid("segmentation origin rebasing overflowed"))?;
        Ok(Self {
            id: 0,
            chunk_index: 0,
            start,
            filled: 0,
        })
    }

    /// Where the media accumulated so far ends.
    fn filled_to(&self) -> Result<TickTimestamp, MuxError> {
        self.start
            .checked_add_unsigned(self.filled)
            .ok_or_else(|| mux_error("segment timing overflowed"))
    }

    /// The coordinator supplies the achieved cut; the writer owns no grid.
    fn advance(&mut self, _plan: &TrackSegmentationPlan) -> Result<(), MuxError> {
        let boundary = self.filled_to()?;
        self.id = self.id.saturating_add(1);
        self.chunk_index = 0;
        self.start = boundary;
        self.filled = 0;
        Ok(())
    }
}

/// Timing is validated once on arrival; serialization waits for a committed cut.
struct PendingSample {
    sample: NormalizedMedia,
    pts: TickTimestamp,
    dts: TickTimestamp,
    presented_pts: TickTimestamp,
    duration: TickDuration,
    /// Output memory reserved when the sample was accepted; `None` for
    /// timing-only tracks, which serialize nothing.
    charge: Option<crate::domain::Reservation>,
}

/// CMAF packaging state for one track.
struct CmafTrack {
    rendition_id: PackagingRenditionId,
    track_id: TrackId,
    codec: Codec,
    kind: MediaKind,
    plan: TrackSegmentationPlan,
    output: Option<CmafOutput>,
    boundary_allowance: Duration,
    partitioner: crate::segment::cutter::PartPartitioner,
    pending: VecDeque<PendingSample>,
    partition_clock: crate::segment::cutter::PartClock,
    segment: SegmentCursor,
    fragment: Option<OpenFragment>,
    presented_timing: PresentedTimingCursor,
    initialized: bool,
    after_gap: bool,
    last_dts: Option<TickTimestamp>,
    composition: crate::segment::cutter::CompositionGroups,
    published_presentation_end: Option<TickTimestamp>,
    finished: bool,
}

impl CmafTrack {
    fn new(
        rendition_id: PackagingRenditionId,
        track: &DiscoveredTrack,
        plan: TrackSegmentationPlan,
        output: Option<CmafOutput>,
        boundary_allowance: Duration,
        _events: EventSink,
    ) -> Result<Self, MuxError> {
        Ok(Self {
            rendition_id,
            track_id: track.id,
            codec: track.codec,
            kind: track.kind(),
            plan,
            output,
            boundary_allowance,
            partitioner: crate::segment::cutter::PartPartitioner::new(plan.part_duration.get()),
            pending: VecDeque::new(),
            partition_clock: crate::segment::cutter::PartClock::new(
                track.kind(),
                plan.segmentation_origin_pts
                    .checked_sub(plan.presentation_origin_pts)
                    .ok_or_else(|| invalid("partition origin overflows"))?,
            ),
            segment: SegmentCursor::new(&plan)?,
            fragment: None,
            presented_timing: PresentedTimingCursor::for_track(track),
            initialized: false,
            after_gap: false,
            last_dts: None,
            composition: crate::segment::cutter::CompositionGroups::default(),
            published_presentation_end: None,
            finished: false,
        })
    }

    fn push(
        &mut self,
        sample: &NormalizedMedia,
        charge: Option<crate::domain::Reservation>,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.output.is_some() && charge.is_none() {
            return Err(mux_error(format!(
                "{} accepted a sample without reserving its output",
                self.track_id
            )));
        }
        if self.last_dts.is_none() && self.kind == MediaKind::Video && !sample.random_access() {
            return Err(MuxError::Boundary {
                track: self.track_id,
                reason: "publication must start at random access",
                observed: 0,
                maximum: 0,
            });
        }
        let (pts, dts, presented) = self.rebase_sample(sample)?;
        // A verified video restart ends dependencies across the missing interval.
        // Audio packet sync flags do not provide the same decoder guarantee.
        if self.kind == MediaKind::Video && sample.random_access() {
            self.after_gap = false;
        }
        let presented_pts = presented
            .start
            .checked_sub(self.plan.presentation_origin_pts)
            .ok_or_else(|| {
                mux_error(format!(
                    "presentation timing rebasing overflowed for {}",
                    self.track_id
                ))
            })?;
        let partition_duration = self
            .partition_clock
            .advance(presented_pts, presented.duration)
            .ok_or_else(|| mux_error("partition span overflows"))?;
        // A decode-order cut inside a B-frame group creates overlapping
        // presentation ranges in adjacent chunks. Wait until the presentation
        // high-water mark fits the elapsed decode span before allowing a cut.
        let can_end = self.kind != MediaKind::Video
            || self
                .composition
                .advance(pts, dts, sample.duration(), self.plan.boundary_tolerance)
                .ok_or_else(|| mux_error("composition group timing overflows"))?;
        let cuts = self
            .partitioner
            .push_with_boundary(
                partition_duration,
                sample.random_access() && !self.after_gap,
                can_end,
            )
            .map_err(|error| MuxError::Part {
                track: self.track_id,
                error,
            })?;
        // A push never seals the final suffix, so these cuts consume only
        // samples already retained before the incoming unit.
        for count in cuts {
            self.write_pending(count)?;
            self.flush_fragment(out)?;
        }
        self.pending.push_back(PendingSample {
            sample: sample.clone(),
            pts,
            dts,
            presented_pts,
            duration: presented.duration,
            charge,
        });
        Ok(())
    }

    fn gap(
        &mut self,
        gap: crate::media::MissingInterval,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if gap.track_id != self.track_id
            || gap.media_kind != self.kind
            || gap.timebase != self.plan.timebase
            || gap.end <= gap.start
        {
            return Err(mux_error("invalid missing interval"));
        }
        let start = gap
            .start
            .checked_sub(self.plan.presentation_origin_pts)
            .ok_or_else(|| mux_error("gap origin overflows"))?;
        let end = gap
            .end
            .checked_sub(self.plan.presentation_origin_pts)
            .ok_or_else(|| mux_error("gap end overflows"))?;
        let maximum = self.plan.maximum_segment_ticks(self.boundary_allowance);
        let count = end.abs_diff(start).div_ceil(self.plan.part_duration.get());
        if maximum == 0
            || count > crate::source::InputLimits::permissive().maximum_samples_per_batch as u64
        {
            return Err(mux_error("gap partition exceeds bounded output capacity"));
        }
        self.prepare_tail(out)?;
        self.flush_fragment(out)?;
        if self.segment.filled_to()? != start {
            return Err(mux_error("gap does not follow available media"));
        }
        if self.segment.filled > 0 {
            TrackPackager::cut(self, gap.start, out)?;
        }
        self.after_gap = true;
        if let Some(output) = &mut self.output {
            output.gap();
        }
        while self.segment.start < end {
            let duration = end.abs_diff(self.segment.start).min(maximum);
            let mut remaining = duration;
            let mut parts = Vec::new();
            // Each item is bounded by the frozen part ceiling. No media bytes
            // or fabricated access units are allocated for the absence.
            while remaining > 0 {
                let part = remaining.min(self.plan.part_duration.get());
                parts.push(part);
                remaining -= part;
            }
            if self.output.is_some() {
                out.push(PackagedMedia::Gap(super::PackagedGap {
                    rendition_id: self.rendition_id,
                    packaging_segment_id: PackagingSegmentId(self.segment.id),
                    media_start: self.segment.start,
                    duration,
                    parts,
                }));
            }
            self.segment.filled = duration;
            self.segment.advance(&self.plan)?;
        }
        self.partition_clock = crate::segment::cutter::PartClock::new(self.kind, end);
        self.published_presentation_end = Some(end);
        Ok(())
    }

    fn write_pending(&mut self, count: usize) -> Result<(), MuxError> {
        for _ in 0..count {
            let pending = self
                .pending
                .pop_front()
                .expect("partition counts match the sample queue");
            if pending.duration > 0 {
                self.open_or_extend_fragment(
                    &pending.sample,
                    pending.presented_pts,
                    pending.duration,
                )?;
            }
            // Fully primed units still initialize the decoder, but contribute
            // no presentation duration to the part.
            if let Some(output) = &mut self.output {
                let charge = pending
                    .charge
                    .expect("serializing tracks reserve every accepted sample");
                output.write(&pending.sample, pending.pts, pending.dts, charge);
            }
        }
        Ok(())
    }

    /// Seal regular prefixes, leaving the true tail open for boundary accounting.
    fn prepare_tail(&mut self, out: &mut dyn Appender<PackagedMedia>) -> Result<(), MuxError> {
        let counts = self.partitioner.finish().map_err(|error| MuxError::Part {
            track: self.track_id,
            error,
        })?;
        for (index, count) in counts.iter().enumerate() {
            self.write_pending(*count)?;
            if index + 1 < counts.len() {
                self.flush_fragment(out)?;
            }
        }
        Ok(())
    }

    fn rebase_sample(
        &mut self,
        sample: &NormalizedMedia,
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
        sample: &NormalizedMedia,
        presented_pts: TickTimestamp,
        presented_duration: TickDuration,
    ) -> Result<(), MuxError> {
        // Malformed or changing presentation order cannot revise a published
        // chunk, even if its decode timestamps continue to advance.
        if self.kind == MediaKind::Video
            && let Some(previous) = self.published_presentation_end
        {
            let overlap = duration_since(previous, presented_pts).unwrap_or(0);
            if overlap > self.plan.boundary_tolerance {
                return Err(MuxError::Boundary {
                    track: self.track_id,
                    reason: "video presentation overlaps a published part",
                    observed: overlap,
                    maximum: self.plan.boundary_tolerance,
                });
            }
        }
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
                    presentation_end: end,
                    independent: sample.random_access() && !self.after_gap,
                });
            }
        }
        if let Some(fragment) = &mut self.fragment {
            fragment.presentation_end = fragment.presentation_end.max(end);
            fragment.independent |= sample.random_access() && !self.after_gap;
        }
        Ok(())
    }

    /// The final open part may absorb timestamp quantization, but already
    /// published parts and encoded sample durations must never be rewritten.
    fn close_fragment_at(&mut self, presented_pts: TickTimestamp) -> Result<(), MuxError> {
        if let Some(fragment) = &mut self.fragment {
            if self.kind == MediaKind::Video && fragment.end > presented_pts {
                let overshoot = duration_since(fragment.end, presented_pts).unwrap_or(u64::MAX);
                if overshoot > self.plan.boundary_tolerance || presented_pts <= fragment.start {
                    return Err(MuxError::Boundary {
                        track: self.track_id,
                        reason: "encoded video duration exceeds selected presentation cut",
                        observed: overshoot,
                        maximum: self.plan.boundary_tolerance,
                    });
                }
                fragment.end = presented_pts;
            } else {
                fragment.end = fragment.end.max(presented_pts);
            }
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

        if duration > self.plan.part_duration.get() {
            return Err(MuxError::Part {
                track: self.track_id,
                error: crate::segment::cutter::CutError::PartTooLong {
                    duration,
                    maximum: self.plan.part_duration.get(),
                },
            });
        }
        let maximum = self.plan.maximum_segment_ticks(self.boundary_allowance);
        if self.segment.filled.saturating_add(duration) > maximum {
            return Err(MuxError::Boundary {
                track: self.track_id,
                reason: "segment ceiling exhausted",
                observed: self.segment.filled.saturating_add(duration),
                maximum,
            });
        }
        if let Some(output) = &mut self.output {
            let mut payload = output
                .flush_fragment()
                .map_err(|error| mux_error(error.to_string()))?;
            if !self.initialized {
                if payload.is_empty() {
                    return Err(mux_error("CMAF initialization segment was empty"));
                }
                out.push(PackagedMedia::Initialization(InitializationSegment {
                    rendition_id: self.rendition_id,
                    version: 0,
                    payload,
                }));
                self.initialized = true;
                payload = output
                    .flush_fragment()
                    .map_err(|error| mux_error(error.to_string()))?;
            }
            if payload.is_empty() {
                return Err(mux_error("CMAF mux produced an empty chunk"));
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
        }
        self.published_presentation_end = Some(fragment.presentation_end);
        self.segment.chunk_index = self
            .segment
            .chunk_index
            .checked_add(1)
            .ok_or_else(|| mux_error("chunk index overflowed"))?;
        self.segment.filled = self
            .segment
            .filled
            .checked_add(duration)
            .ok_or_else(|| mux_error("segment duration overflowed"))?;
        Ok(())
    }
}

impl TrackPackager for CmafTrack {
    fn buffered(&self) -> usize {
        self.pending.len()
    }

    fn cut(
        &mut self,
        pts: TickTimestamp,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        self.prepare_tail(out)?;
        let pts = pts
            .checked_sub(self.plan.presentation_origin_pts)
            .ok_or_else(|| mux_error("boundary rebasing overflowed"))?;
        if self.kind == MediaKind::Video
            && self.fragment.is_none()
            && self.segment.filled_to()? != pts
        {
            return Err(MuxError::Boundary {
                track: self.track_id,
                reason: "published video parts do not end at the selected presentation cut",
                observed: self.segment.filled_to()?.abs_diff(pts),
                maximum: 0,
            });
        }
        self.close_fragment_at(pts)?;
        self.flush_fragment(out)?;
        if self.segment.filled == 0 {
            // A missing interval may already have closed exactly at this cut.
            if self.segment.start == pts {
                return Ok(());
            }
            return Err(mux_error("empty coordinated segment"));
        }
        out.push(PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
            rendition_id: self.rendition_id,
            packaging_segment_id: PackagingSegmentId(self.segment.id),
            media_start: self.segment.start,
            duration: self.segment.filled,
        }));
        self.segment.advance(&self.plan)
    }

    fn track_id(&self) -> TrackId {
        self.track_id
    }

    fn push(
        &mut self,
        sample: NormalizedMedia,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        let charge = self.reserve(&sample)?;
        self.push_reserved(sample, charge, out)
    }

    fn reserve(
        &self,
        sample: &NormalizedMedia,
    ) -> Result<Option<crate::domain::Reservation>, MuxError> {
        // Gaps publish no bytes, and timing-only tracks serialize nothing.
        if matches!(sample, NormalizedMedia::Gap(_)) {
            return Ok(None);
        }
        let Some(output) = &self.output else {
            return Ok(None);
        };
        output
            .reserve(sample.payload_len())
            .map(Some)
            .map_err(|source| MuxError::Memory {
                track: self.track_id,
                source,
            })
    }

    fn push_reserved(
        &mut self,
        sample: NormalizedMedia,
        charge: Option<crate::domain::Reservation>,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if let NormalizedMedia::Gap(gap) = sample {
            return self.gap(gap, out);
        }
        CmafTrack::push(self, &sample, charge, out)
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
            self.prepare_tail(out)?;
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
        self.pending.clear();
        if let Some(output) = &mut self.output {
            output.finalize();
        }
        Ok(())
    }
}

/// The transfer function a video track's own bitstream declares.
///
/// Read from the codec configuration rather than carried down from discovery,
/// because that is where it is unambiguous: an ingest adapter may or may not
/// surface colour signalling depending on its container, while the parameter
/// set inside the extradata is the same bytes a decoder will use.
///
/// \`None\` when the bitstream declares nothing. HLS makes \`VIDEO-RANGE\` optional
/// and a player defaults it to SDR, so an absent declaration is honest; a
/// guessed \`SDR\` on unsignalled HDR content is not, and would tell a display
/// not to switch modes for content that needs it.
fn video_range(track: &DiscoveredTrack) -> Option<VideoRange> {
    let colour =
        crate::media::video_config::properties(track.codec, track.codec_extradata.as_bytes())
            .colour?;
    // ISO/IEC 23091-2 transfer characteristics. Everything outside these two
    // is a standard-dynamic-range curve as far as HLS is concerned; the
    // attribute has no third HDR value to offer.
    Some(match colour.1 {
        16 => VideoRange::Pq,
        18 => VideoRange::Hlg,
        _ => VideoRange::Sdr,
    })
}

// HLS RESOLUTION describes display pixels; the sample entry keeps coded size.
fn display_width(
    width: NonZero<u32>,
    aspect: Option<(u16, u16)>,
) -> Result<NonZero<u32>, MuxError> {
    let Some((numerator, denominator)) = aspect else {
        return Ok(width);
    };
    if numerator == 0 || denominator == 0 {
        return Err(invalid("video sample aspect ratio is zero"));
    }
    let scaled = (u64::from(width.get()) * u64::from(numerator) + u64::from(denominator) / 2)
        / u64::from(denominator);
    u32::try_from(scaled)
        .ok()
        .and_then(NonZero::new)
        .ok_or_else(|| invalid("video display width cannot be represented"))
}

fn packaged_rendition(
    rendition_id: PackagingRenditionId,
    track: &DiscoveredTrack,
    plan: &TrackSegmentationPlan,
    boundary_allowance: Duration,
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
        } => {
            let aspect = crate::media::video_config::properties(
                track.codec,
                track.codec_extradata.as_bytes(),
            )
            .aspect;
            RenditionMedia::Video {
                width: display_width(width, aspect)?,
                height,
                frame_rate,
                video_range: video_range(track),
            }
        }
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
    let maximum_segment_duration = NonZero::new(plan.maximum_segment_ticks(boundary_allowance))
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

fn sample_codec(sample: &NormalizedMedia) -> Codec {
    match sample {
        NormalizedMedia::Video(sample) => sample.codec,
        NormalizedMedia::Audio(sample) => sample.codec,
        NormalizedMedia::Subtitle(sample) => sample.codec,
        NormalizedMedia::Gap(_) => unreachable!("gaps are handled before sample writing"),
    }
}

fn sample_dts(sample: &NormalizedMedia) -> TickTimestamp {
    match sample {
        NormalizedMedia::Video(sample) => sample.dts,
        NormalizedMedia::Audio(sample) => sample.pts,
        NormalizedMedia::Subtitle(sample) => sample.pts,
        NormalizedMedia::Gap(gap) => gap.start,
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
    #[derive(Clone, Copy)]
    enum SegmentBoundaryPolicy {
        Strict,
        ExtendToRandomAccess { maximum_extension: Duration },
    }

    use std::{
        io::Cursor,
        num::NonZero,
        time::{Duration, SystemTime},
    };

    use crate::{
        admission::StreamPolicy,
        domain::{
            AudioTiming, AudioTrim, FrameRate, MediaKind, MediaParameters, Payload, Timebase,
            TrackId,
            fixtures::{TrackBuilder, catalog},
        },
        media::{
            NormalizedMedia, NormalizerFactory, PassThroughNormalizerFactory, VideoSample,
            calibrate, validate,
        },
        mux::{
            InitializationSegment, MuxerStartRequest, PackagedMedia,
            fixtures::{
                AAC_EXTRADATA, AAC_FRAME, AAC_FRAME_SAMPLES, H264_EXTRADATA, H264_IDR, H264_P,
                RecordedEvents, discarded_events, edit_list,
            },
        },
        observe::SessionEvent,
        segment::{SegmentationPlan, fixtures::PlanBuilder},
        source::{
            DiscoveryLimits, IngressEvent, InputLimits, InputState, MpegTsConfig,
            MpegTsPacketSource, PacketSource, ReadInput, RtmpPacketSource, channel,
        },
    };

    use crate::mux::{MuxerFactory, PassThroughMuxerFactory};

    #[test]
    fn display_resolution_applies_sar_with_nearest_pixel_rounding()
    -> Result<(), crate::mux::MuxError> {
        assert_eq!(
            super::display_width(nz::u32!(320), Some((4, 3)))?.get(),
            427
        );
        assert_eq!(super::display_width(nz::u32!(320), None)?.get(), 320);
        assert_eq!(
            super::display_width(nz::u32!(320), Some((3, 4)))?.get(),
            240
        );
        assert!(super::display_width(nz::u32!(320), Some((1, 0))).is_err());
        assert!(super::display_width(nz::u32!(u32::MAX), Some((2, 1))).is_err());
        Ok(())
    }

    const FRAME: u64 = 8_192;

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
                PackagedMedia::Segment(_)
                | PackagedMedia::SegmentCompleted(_)
                | PackagedMedia::Gap(_) => {}
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
                PackagedMedia::Segment(_)
                | PackagedMedia::SegmentCompleted(_)
                | PackagedMedia::Gap(_) => {}
            }
        }
        outputs
    }

    fn demux_cmaf(bytes: &[u8]) -> transmux::Media {
        broadcast_common::Unpackage::unpackage(&mut transmux::Fmp4Demux::new(), bytes)
            .unwrap_or_else(|error| panic!("CMAF output is demuxable: {error}"))
    }

    fn cmaf_codec(config: &transmux::CodecConfig) -> crate::domain::Codec {
        match config {
            transmux::CodecConfig::Avc { .. } => crate::domain::Codec::H264,
            transmux::CodecConfig::Hevc { .. } => crate::domain::Codec::Hevc,
            transmux::CodecConfig::Av1 { .. } => crate::domain::Codec::Av1,
            transmux::CodecConfig::Aac { .. } => crate::domain::Codec::Aac,
            transmux::CodecConfig::Opus { .. } => crate::domain::Codec::Opus,
            transmux::CodecConfig::Flac { .. } => crate::domain::Codec::Flac,
            _ => panic!("unexpected CMAF codec"),
        }
    }

    fn sample(pts: i64, random_access: bool) -> NormalizedMedia {
        sample_for(0, pts, random_access)
    }

    fn sample_for(track_id: u32, pts: i64, random_access: bool) -> NormalizedMedia {
        sample_with_dts(track_id, pts, pts, random_access)
    }

    fn sample_with_dts(track_id: u32, pts: i64, dts: i64, random_access: bool) -> NormalizedMedia {
        NormalizedMedia::Video(VideoSample {
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

    fn audio_sample(pts: i64) -> NormalizedMedia {
        NormalizedMedia::Audio(crate::media::AudioSample {
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
        PassThroughMuxerFactory
            .start(MuxerStartRequest {
                presentation: input,
                segmentation: &segmentation,
                time_anchor: SystemTime::UNIX_EPOCH,
                events,
                budget: &crate::domain::PipelineBudget::unlimited(),
            })
            .expect("the fixture CMAF output starts")
    }

    fn start(
        policy: SegmentBoundaryPolicy,
        timebase: Timebase,
        events: &crate::observe::EventSink,
    ) -> Result<crate::mux::StartedMuxer, crate::mux::MuxError> {
        start_with_budget(
            policy,
            timebase,
            events,
            &crate::domain::PipelineBudget::unlimited(),
        )
    }

    fn start_with_budget(
        policy: SegmentBoundaryPolicy,
        timebase: Timebase,
        events: &crate::observe::EventSink,
        budget: &crate::domain::PipelineBudget,
    ) -> Result<crate::mux::StartedMuxer, crate::mux::MuxError> {
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())
            .expect("fixture presentation validates");
        let mut segmentation = SegmentationPlan::new(&input, vec![video_plan(0, timebase).build()])
            .expect("fixture segmentation validates");
        segmentation.late_boundary = match policy {
            SegmentBoundaryPolicy::Strict => Duration::ZERO,
            SegmentBoundaryPolicy::ExtendToRandomAccess { maximum_extension } => maximum_extension,
        };
        PassThroughMuxerFactory.start(MuxerStartRequest {
            presentation: &input,
            segmentation: &segmentation,
            time_anchor: SystemTime::UNIX_EPOCH,
            events,
            budget,
        })
    }

    /// Draining at finish writes and flushes every accepted sample. Their
    /// output was reserved on acceptance, so a full budget cannot stop it.
    #[test]
    fn accepted_samples_drain_after_the_budget_fills() -> Result<(), Box<dyn std::error::Error>> {
        use crate::domain::{PipelineBudget, Stage};
        let budget = PipelineBudget::with_reserve(1024 * 1024, 256 * 1024);
        let mut started = start_with_budget(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &discarded_events(),
            &budget,
        )?;
        let mut media = Vec::new();
        started.muxer.push(sample(0, true), &mut media)?;
        assert!(
            media.is_empty(),
            "the sample is still held for partitioning"
        );

        // The rest of the pipeline takes everything, reserve included.
        let filler = budget.try_reserve(budget.limit() - budget.used(), Stage::MuxOutput)?;
        let full = budget.used();
        let next = i64::try_from(FRAME)?;
        assert!(matches!(
            started.muxer.push(sample(next, false), &mut media),
            Err(crate::mux::MuxError::Memory { .. })
        ));

        started
            .muxer
            .finish(crate::mux::FinishReason::Final, &mut media)?;
        assert!(
            media
                .iter()
                .any(|item| matches!(item, PackagedMedia::Chunk(_))),
            "the accepted sample was published"
        );
        assert!(budget.used() <= full, "draining never grows usage");
        drop((media, filler, started));
        assert_eq!(budget.used(), 0);
        Ok(())
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

    fn trimmed_audio_sample(pts: i64, trim: AudioTrim) -> NormalizedMedia {
        NormalizedMedia::Audio(crate::media::AudioSample {
            track_id: TrackId(0),
            codec: crate::domain::Codec::Aac,
            pts,
            duration: AAC_FRAME_SAMPLES,
            trim,
            payload: Payload::from(AAC_FRAME.to_vec()),
        })
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
    fn a_timestamp_gap_cannot_extend_a_part_past_its_ceiling() {
        let mut started = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &discarded_events(),
        )
        .expect("mux starts");
        let mut media = Vec::new();
        started
            .muxer
            .push(sample(0, true), &mut media)
            .expect("first sample");
        assert!(matches!(
            started.muxer.push(sample(16_384, true), &mut media),
            Err(crate::mux::MuxError::Part { .. })
        ));
        assert!(media.is_empty(), "the invalid part must never be emitted");
    }

    #[test]
    fn delay_moov_emits_initialization_then_first_chunk() {
        let sink = discarded_events();
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

        // Frozen output bytes own their allocation independently of the writer.
        let retained = chunk.payload.clone();
        drop(started);
        assert!(!retained.is_empty());
    }

    #[test]
    fn a_part_ceiling_smaller_than_a_reorder_group_is_rejected() {
        let sink = discarded_events();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(16_384));
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())
            .expect("fixture presentation validates");
        let mut started = started(
            &input,
            vec![
                PlanBuilder::new(0, timebase, nz::u64!(131_072))
                    .part(nz::u32!(1), nz::u64!(8_192))
                    .build(),
            ],
            &sink,
        );
        let frame = i64::try_from(FRAME).expect("frame fits");
        let mut media = Vec::new();
        started
            .muxer
            .push(sample_with_dts(0, 0, -3 * frame, true), &mut media)
            .expect("first frame");
        started
            .muxer
            .push(sample_with_dts(0, 4 * frame, -2 * frame, false), &mut media)
            .expect("reference frame");
        assert!(matches!(
            started
                .muxer
                .push(sample_with_dts(0, frame, -frame, false), &mut media),
            Err(crate::mux::MuxError::Part { .. })
        ));
    }

    #[test]
    fn late_presentation_samples_cannot_rewrite_published_parts() -> Result<(), crate::mux::MuxError>
    {
        let mut started = start(
            SegmentBoundaryPolicy::Strict,
            Timebase::new(nz::u32!(1), nz::u32!(16_384)),
            &discarded_events(),
        )?;
        let frame = i64::try_from(FRAME).expect("frame fits");
        let mut media = Vec::new();
        for sample in [
            sample_with_dts(0, 0, 0, true),
            sample_with_dts(0, frame, frame, false),
            sample_with_dts(0, 0, 2 * frame, false),
        ] {
            started.muxer.push(sample, &mut media)?;
        }
        let published = media.len();
        let error = started
            .muxer
            .finish(crate::mux::FinishReason::Final, &mut media)
            .expect_err("overlap with published media");
        assert!(matches!(
            error,
            crate::mux::MuxError::Boundary {
                reason: "video presentation overlaps a published part",
                ..
            }
        ));
        assert_eq!(media.len(), published);
        Ok(())
    }

    #[test]
    fn strict_boundaries_complete_zero_based_segments() {
        let sink = discarded_events();
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
        let sink = discarded_events();
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
        let sink = discarded_events();
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
        part_access_units: Option<NonZero<u32>>,
        part_target: NonZero<u64>,
        segment_ticks: u64,
    ) -> (Vec<u64>, u64) {
        let sink = discarded_events();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(timebase)
            .codec(crate::domain::Codec::Aac)
            .codec_extradata(AAC_EXTRADATA.to_vec())
            .build();
        let input = validate(&catalog(vec![audio]), &StreamPolicy::permissive())
            .expect("audio fixture validates");
        let plan = PlanBuilder::new(0, timebase, NonZero::new(segment_ticks).expect("nonzero"))
            .part(part_access_units.unwrap_or(nz::u32!(1)), part_target)
            // A jittery cadence needs room for its longest access unit.
            .boundary_tolerance(access_units.iter().copied().max().unwrap_or(0))
            .build();
        let mut started = started(&input, vec![plan], &sink);

        let mut media = Vec::new();
        let mut pts = 0_i64;
        for duration in access_units {
            let mut sample = audio_sample(pts);
            if let NormalizedMedia::Audio(sample) = &mut sample {
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
                packaged_parts(&access_units, Some(nz::u32!(4)), target, 24 * 1_024);
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
    fn duration_based_parts_preserve_jitter_without_exceeding_hls_bounds() {
        // A large spread defeats fixed AU counts. Vary the phase against the
        // part boundary to exercise different crossing units over many parts.
        let durations: Vec<_> = [120, 720, 360, 600, 240]
            .into_iter()
            .cycle()
            .take(500)
            .collect();
        let (parts, target) = packaged_parts(&durations, None, nz::u64!(4_800), 480_000);
        assert!(parts.len() > 30);
        for duration in parts {
            assert!(duration * 100 >= target * 85);
            assert!(duration <= target);
        }
    }

    #[test]
    fn variable_video_parts_keep_the_encoded_timestamps() -> Result<(), Box<dyn std::error::Error>>
    {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(90_000));
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())?;
        let plan = PlanBuilder::new(0, timebase, nz::u64!(900_000))
            .part(nz::u32!(1), nz::u64!(90_000))
            .build();
        let mut mux = started(&input, vec![plan], &discarded_events());
        let mut media = Vec::new();
        let mut pts = 0;
        let mut expected_pts = Vec::new();
        for duration in [1_800, 6_300, 2_700, 4_500].into_iter().cycle().take(200) {
            let mut sample = sample_for(0, pts, pts == 0);
            if let NormalizedMedia::Video(video) = &mut sample {
                video.duration = duration;
            }
            expected_pts.push(Some(pts));
            mux.muxer.push(sample, &mut media)?;
            pts += i64::try_from(duration)?;
        }
        // All emitted parts are regular; the unflushed tail is still pending.
        for event in &media {
            if let PackagedMedia::Chunk(chunk) = event {
                assert!(chunk.duration >= 76_500 && chunk.duration <= 90_000);
            }
        }
        mux.muxer
            .finish(crate::mux::FinishReason::Final, &mut media)?;
        let demuxed = demux_cmaf(&concat_cmaf_bytes(&media));
        let actual: Vec<_> = demuxed.tracks[0]
            .samples
            .iter()
            .map(|sample| sample.pts)
            .collect();
        assert_eq!(actual, expected_pts);
        Ok(())
    }

    #[test]
    fn duration_cutting_keeps_aac_parts_within_the_selected_ceiling() {
        // The threshold closes regular parts at 21 frames; the tail is shorter.
        let (parts, part_target) = packaged_parts(
            &[AAC_FRAME_SAMPLES; 94],
            Some(nz::u32!(24)),
            nz::u64!(24 * AAC_FRAME_SAMPLES),
            93 * AAC_FRAME_SAMPLES,
        );

        assert_eq!(part_target, 24 * AAC_FRAME_SAMPLES);
        assert_eq!(
            parts,
            vec![
                21 * AAC_FRAME_SAMPLES,
                21 * AAC_FRAME_SAMPLES,
                21 * AAC_FRAME_SAMPLES,
                21 * AAC_FRAME_SAMPLES,
                9 * AAC_FRAME_SAMPLES,
            ]
        );
    }

    #[test]
    fn part_counting_restarts_at_every_segment_boundary() {
        // Five units to a segment against a four-unit part: each segment ends
        // with a short part, and the next segment's first part must be a full
        // four units rather than continuing the previous count.
        let (parts, _) = packaged_parts(
            &[AAC_FRAME_SAMPLES; 15],
            Some(nz::u32!(4)),
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
        let sink = discarded_events();
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
                    .part(nz::u32!(1), nz::u64!(1_024))
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
        let sink = discarded_events();
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
        assert_priming_round_trip(&media);
    }

    fn assert_priming_round_trip(media: &[PackagedMedia]) {
        let bytes = concat_cmaf_bytes(media);
        let demuxed = demux_cmaf(&bytes);
        let sample = demuxed.tracks[0]
            .samples
            .first()
            .expect("primed audio demuxes at least one sample");
        assert_eq!(sample.dts, Some(0));
        assert_eq!(sample.pts, Some(0));
    }

    #[test]
    fn audio_priming_spanning_access_units_keeps_packet_metadata_and_grid_timing() {
        let sink = discarded_events();
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
            "the edit list carries the original whole skip count"
        );
        assert!(matches!(
            media.iter().find(|event| matches!(event, PackagedMedia::Chunk(_))),
            Some(PackagedMedia::Chunk(chunk))
                if chunk.media_start == -64 && chunk.duration == 2_048
        ));
    }

    #[test]
    fn trailing_audio_padding_is_excluded_from_final_delivery_timing() {
        let sink = discarded_events();
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

    /// A track that starts after the shared origin says so in `tfdt`. hls.js
    /// and Shaka time samples from `tfdt` alone, and Chrome's MSE ignores an
    /// empty edit, so an empty edit over a zero `tfdt` plays the track early.
    #[test]
    fn a_later_video_start_is_carried_by_tfdt_not_an_empty_edit() {
        let sink = discarded_events();
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

        let [
            PackagedMedia::Initialization(initialization),
            PackagedMedia::Chunk(chunk),
        ] = media.as_slice()
        else {
            panic!("expected initialization and one chunk, got {media:?}");
        };
        assert_eq!(
            edit_list(&initialization.payload),
            [],
            "video without reordering needs no edit, and a late start is never an empty edit"
        );
        assert_eq!(chunk.media_start, 1_980);
        assert_eq!(chunk.duration, FRAME);
        let demuxed = demux_cmaf(&concat_cmaf_bytes(&media));
        let first = demuxed.tracks[0]
            .samples
            .first()
            .expect("offset video demuxes a sample");
        assert_eq!(
            (first.dts, first.pts),
            (Some(1_980), Some(1_980)),
            "tfdt places the first frame at its shared-clock position"
        );
    }

    /// Late audio needs no edit, even when it declares priming. An edit can
    /// only hide media before presentation zero, so for a late track it would
    /// be a pure shift that hls.js (which skips `elst`) misplaces. The late
    /// start lives in `tfdt`, and the priming frame presents just before the
    /// audible start, as it does in Apple's own segmenters.
    #[test]
    fn a_later_audio_start_needs_no_edit_even_with_priming() {
        const PRIMING: i64 = 1_024;
        // The audible start sits 2048 ticks after the shared origin.
        const LATE: i64 = 2_048;
        let sink = discarded_events();
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(timebase)
            .parameters(MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(1),
                frame_size: Some(nz::u32!(1_024)),
                bit_depth: None,
                timing: AudioTiming {
                    initial_padding_samples: 1_024,
                    ..AudioTiming::default()
                },
            })
            .codec(crate::domain::Codec::Aac)
            .codec_extradata(AAC_EXTRADATA.to_vec())
            .build();
        let input = validate(&catalog(vec![audio]), &StreamPolicy::permissive())
            .expect("audio fixture validates");
        let mut started = started(
            &input,
            vec![
                audio_plan(timebase, 8, 2)
                    .presentation_origin(-LATE)
                    .build(),
            ],
            &sink,
        );
        let mut media = Vec::new();
        for sample in [
            trimmed_audio_sample(
                -PRIMING,
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
                .expect("late primed AAC packages");
        }

        let initialization = media
            .iter()
            .find_map(|event| match event {
                PackagedMedia::Initialization(initialization) => Some(initialization),
                _ => None,
            })
            .expect("delayed initialization is emitted");
        assert_eq!(
            edit_list(&initialization.payload),
            [],
            "a late track carries neither an empty edit nor a priming shift"
        );
        assert!(matches!(
            media.iter().find(|event| matches!(event, PackagedMedia::Chunk(_))),
            Some(PackagedMedia::Chunk(chunk)) if chunk.media_start == LATE
        ));
        let demuxed = demux_cmaf(&concat_cmaf_bytes(&media));
        let samples = &demuxed.tracks[0].samples;
        assert_eq!(
            samples.first().and_then(|primed| primed.pts),
            Some(LATE - PRIMING),
            "the priming frame presents just before the audible start"
        );
        assert_eq!(
            samples.get(1).and_then(|audible| audible.pts),
            Some(LATE),
            "the first audible frame presents at its shared-clock position"
        );
    }

    #[test]
    fn audio_and_video_keep_their_relative_offset_through_packaging() {
        // The one shape no other CMAF test covers: both kinds in one
        // publication, with different timebases and different start times. If a
        // per-track origin adjustment ever creeps back in, the two renditions
        // drift apart here and nowhere else.
        let sink = discarded_events();
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

    #[test]
    fn emitted_initialization_and_chunks_are_demuxable() {
        let sink = discarded_events();
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

        let bytes = concat_cmaf_bytes(&media);
        let demuxed = demux_cmaf(&bytes);
        assert_eq!(demuxed.tracks.len(), 1);
        assert_eq!(demuxed.tracks[0].spec.timescale, 16_384);
        assert_eq!(demuxed.tracks[0].samples.len(), 4);
        assert!(demuxed.tracks[0].samples[0].flags.is_sync);
    }

    /// One ingest path, demuxed and normalized.
    ///
    /// Shared by the MPEG-TS and RTMP packaging tests so each fixture is
    /// probed and normalized exactly once per test.
    struct IngestFixture {
        presentation: crate::media::PresentationPlan,
        timeline: crate::media::TimelineCalibration,
        samples: Vec<NormalizedMedia>,
    }

    async fn ingest_fixture(mut source: impl PacketSource) -> IngestFixture {
        let discovery = source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 1024 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("fixture tracks are discoverable");
        let input = validate(&discovery.tracks, &StreamPolicy::permissive())
            .expect("fixture tracks are admitted");
        let timeline = calibrate(&input).expect("fixture timeline calibrates");
        let mut normalized = PassThroughNormalizerFactory
            .start(&input, &timeline, crate::domain::InputMode::Permissive)
            .expect("fixture normalization starts");

        let mut packets = Vec::new();
        while source
            .fill(&mut packets)
            .await
            .expect("fixture packets demux")
            .is_open()
        {}
        let mut samples = Vec::new();
        for packet in packets {
            normalized
                .normalizer
                .push(packet, &mut samples)
                .expect("fixture packet normalizes");
        }
        normalized
            .normalizer
            .finish(&mut samples)
            .expect("held video timing resolves at end of input");
        IngestFixture {
            presentation: normalized.presentation,
            timeline: normalized.timeline,
            samples,
        }
    }

    async fn mpeg_ts_fixture() -> IngestFixture {
        let process = crate::observe::ProcessMeters::default();
        let session = crate::observe::SessionMeters::new(process);
        let source = MpegTsPacketSource::new(
            Box::new(ReadInput::closed(Cursor::new(
                crate::source::fixtures::h264_adts_aac_mpeg_ts(),
            ))),
            MpegTsConfig::default(),
            InputLimits::permissive(),
            session.source_view(),
        )
        .expect("MPEG-TS source starts");
        ingest_fixture(source).await
    }

    fn rtmp_audio(timestamp: u32, packet_type: u8, payload: &[u8]) -> IngressEvent {
        let mut raw = vec![0xaf, packet_type];
        raw.extend_from_slice(payload);
        IngressEvent::Audio {
            timestamp,
            media: rtmpx::ValidatedMedia::parse_audio(
                bytes::Bytes::from(raw),
                rtmpx::EnhancedValidationMode::Strict,
            )
            .expect("legacy AAC is valid"),
        }
    }

    fn rtmp_video(timestamp: u32, keyframe: bool, payload: &[u8]) -> IngressEvent {
        let mut raw = vec![if keyframe { 0x17 } else { 0x27 }, 0x01, 0x00, 0x00, 0x00];
        raw.extend_from_slice(payload);
        IngressEvent::Video {
            timestamp,
            media: rtmpx::ValidatedMedia::parse_video(
                bytes::Bytes::from(raw),
                rtmpx::EnhancedValidationMode::Strict,
            )
            .expect("legacy AVC sample is valid"),
        }
    }

    fn rtmp_video_config(payload: &[u8]) -> IngressEvent {
        let mut raw = vec![0x17, 0x00, 0x00, 0x00, 0x00];
        raw.extend_from_slice(payload);
        IngressEvent::Video {
            timestamp: 0,
            media: rtmpx::ValidatedMedia::parse_video(
                bytes::Bytes::from(raw),
                rtmpx::EnhancedValidationMode::Strict,
            )
            .expect("legacy AVC config is valid"),
        }
    }

    async fn rtmp_fixture() -> IngestFixture {
        let process = crate::observe::ProcessMeters::default();
        let session = crate::observe::SessionMeters::new(process);
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        writer
            .send(rtmp_video_config(H264_EXTRADATA))
            .await
            .expect("video config queues");
        writer
            .send(rtmp_audio(0, 0, AAC_EXTRADATA))
            .await
            .expect("audio config queues");
        writer
            .send(rtmp_video(0, true, H264_IDR))
            .await
            .expect("IDR queues");
        writer
            .send(rtmp_audio(0, 1, AAC_FRAME))
            .await
            .expect("first AAC queues");
        writer
            .send(rtmp_video(500, false, H264_P))
            .await
            .expect("P-frame queues");
        let second = u32::try_from(AAC_FRAME_SAMPLES * 1_000 / 48_000).expect("timestamp fits");
        writer
            .send(rtmp_audio(second, 1, AAC_FRAME))
            .await
            .expect("second AAC queues");
        writer.finish(InputState::Closed);
        let source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), session.source_view())
                .expect("RTMP source starts");
        ingest_fixture(source).await
    }

    fn packages_as_demuxable_cmaf(fixture: IngestFixture) -> Vec<Vec<u8>> {
        let tracks = fixture
            .presentation
            .tracks()
            .iter()
            .map(|track| {
                let mut track_samples = fixture
                    .samples
                    .iter()
                    .filter(|sample| sample.track_id() == track.id);
                let first = track_samples
                    .next()
                    .expect("each discovered track produced media");
                let longest = track_samples
                    .map(NormalizedMedia::duration)
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
                    NonZero::new(
                        longest
                            * u64::from(
                                crate::media::video_config::properties(
                                    track.codec,
                                    track.codec_extradata.as_bytes(),
                                )
                                .reorder_depth
                                    + 1,
                            ),
                    )
                    .expect("samples have duration"),
                )
                .presentation_origin(
                    fixture
                        .timeline
                        .get(track.id)
                        .expect("normalized track has a timeline")
                        .origin_pts,
                )
                .segmentation_origin(first.pts())
                .build()
            })
            .collect();
        let segmentation = SegmentationPlan::new(&fixture.presentation, tracks)
            .expect("fixture segmentation is valid");
        let sink = discarded_events();
        let mut mux = PassThroughMuxerFactory
            .start(MuxerStartRequest {
                presentation: &fixture.presentation,
                segmentation: &segmentation,
                time_anchor: SystemTime::UNIX_EPOCH,
                events: &sink,
                budget: &crate::domain::PipelineBudget::unlimited(),
            })
            .expect("presentation packages");
        let mut media = Vec::new();
        for sample in fixture.samples {
            mux.muxer
                .push(sample, &mut media)
                .expect("normalized sample packages");
        }
        mux.muxer
            .finish(crate::mux::FinishReason::Final, &mut media)
            .expect("CMAF tails finish");

        let outputs = collect_rendition_bytes(&media);
        for (bytes, expected) in outputs.iter().zip(
            fixture
                .presentation
                .tracks()
                .iter()
                .map(|track| track.codec),
        ) {
            assert!(!bytes.is_empty());
            let demuxed = demux_cmaf(bytes);
            assert_eq!(cmaf_codec(&demuxed.tracks[0].spec.config), expected);
            assert!(!demuxed.tracks[0].samples.is_empty());
        }
        outputs
            .into_iter()
            .filter(|bytes| !bytes.is_empty())
            .collect()
    }

    async fn ts_bytes_fixture(bytes: &[u8]) -> IngestFixture {
        let session = crate::observe::SessionMeters::new(crate::observe::ProcessMeters::default());
        let source = MpegTsPacketSource::new(
            Box::new(ReadInput::closed(Cursor::new(bytes.to_vec()))),
            MpegTsConfig::default(),
            InputLimits::permissive(),
            session.source_view(),
        )
        .expect("TS source");
        ingest_fixture(source).await
    }

    async fn flv_audio_fixture(bytes: &[u8]) -> IngestFixture {
        let session = crate::observe::SessionMeters::new(crate::observe::ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        let mut offset = 13;
        while offset + 11 <= bytes.len() {
            let size = usize::from(bytes[offset + 1]) * 65_536
                + usize::from(bytes[offset + 2]) * 256
                + usize::from(bytes[offset + 3]);
            let timestamp = u32::from_be_bytes([
                bytes[offset + 7],
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
            ]);
            let payload = &bytes[offset + 11..offset + 11 + size];
            if bytes[offset] == 8 {
                writer
                    .send(IngressEvent::Audio {
                        timestamp,
                        media: rtmpx::ValidatedMedia::parse_audio(
                            bytes::Bytes::copy_from_slice(payload),
                            rtmpx::EnhancedValidationMode::Strict,
                        )
                        .expect("AAC FLV tag"),
                    })
                    .await
                    .expect("tag queues");
            }
            offset += 11 + size + 4;
        }
        writer.finish(InputState::Closed);
        let source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), session.source_view())
                .expect("RTMP source");
        ingest_fixture(source).await
    }

    #[tokio::test]
    async fn real_he_aac_and_hev2_keep_output_rate_and_frame_size() {
        for bytes in [
            &include_bytes!("../../../tests/apple_hls/fixtures/he_aac.flv")[..],
            &include_bytes!("../../../tests/apple_hls/fixtures/hev2_aac.flv")[..],
        ] {
            let fixture = flv_audio_fixture(bytes).await;
            let MediaParameters::Audio {
                sample_rate,
                channels,
                frame_size,
                ..
            } = fixture.presentation.tracks()[0].parameters
            else {
                panic!("audio")
            };
            assert_eq!(
                (sample_rate.get(), channels.get(), frame_size.unwrap().get()),
                (48_000, 2, 2_048)
            );
            assert!(
                fixture
                    .samples
                    .iter()
                    .all(|sample| sample.duration() == 2_048)
            );
            packages_as_demuxable_cmaf(fixture);
        }
    }

    #[tokio::test]
    async fn h264_and_hevc_preserve_colour_aspect_and_reorder_depth() {
        for bytes in [
            &include_bytes!("../../../tests/apple_hls/fixtures/h264_colour.ts")[..],
            &include_bytes!("../../../tests/apple_hls/fixtures/hevc_hdr.ts")[..],
        ] {
            let fixture = ts_bytes_fixture(bytes).await;
            let MediaParameters::Video { video_delay, .. } =
                fixture.presentation.tracks()[0].parameters
            else {
                panic!("video")
            };
            assert!(video_delay > 0);
            let hevc = fixture.presentation.tracks()[0].codec == crate::domain::Codec::Hevc;
            let outputs = packages_as_demuxable_cmaf(fixture);
            if hevc {
                let offset = outputs[0]
                    .windows(4)
                    .position(|v| v == b"clli")
                    .expect("content light box");
                assert_eq!(&outputs[0][offset + 4..offset + 8], &[3, 232, 1, 144]);
                let offset = outputs[0]
                    .windows(4)
                    .position(|v| v == b"mdcv")
                    .expect("mastering display box");
                assert_eq!(
                    &outputs[0][offset + 4..offset + 8],
                    &[0x33, 0xc2, 0x86, 0xc4]
                );
            }
            for (kind, expected) in [
                (b"pasp", vec![0, 0, 0, 4, 0, 0, 0, 3]),
                (b"colr", vec![b'n', b'c', b'l', b'x', 0, 9, 0, 16, 0, 9, 0]),
            ] {
                let offset = outputs[0]
                    .windows(4)
                    .position(|value| value == kind)
                    .expect("visual property box");
                assert_eq!(
                    &outputs[0][offset + 4..offset + 4 + expected.len()],
                    expected
                );
            }
        }
    }

    #[tokio::test]
    async fn fractional_video_preserves_sample_timing_across_parts()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = ts_bytes_fixture(include_bytes!(
            "../../../tests/apple_hls/fixtures/h264_2398_aac.ts"
        ))
        .await;
        let track = fixture
            .presentation
            .tracks()
            .iter()
            .find(|track| track.kind() == MediaKind::Video)
            .expect("video")
            .clone();
        let samples: Vec<_> = fixture
            .samples
            .into_iter()
            .filter(|sample| sample.track_id() == track.id)
            .collect();
        let input = crate::media::fixtures::presentation(vec![track.clone()]);
        let admitted = crate::segment::run_preroll(
            &mut crate::segment::fixtures::SampleBatches::new(vec![samples.clone()]),
            crate::segment::PrerollRequest {
                presentation: &input,
                timeline: &crate::media::fixtures::calibrated([(
                    track.id.0,
                    track.timebase,
                    samples[0].pts(),
                )]),
                limits: crate::segment::PrerollLimits::permissive(),
                budget: &crate::domain::PipelineBudget::unlimited(),
                policy: crate::segment::SegmentationPolicy::latency_first(
                    Duration::from_secs(2),
                    Duration::from_millis(500),
                ),
            },
            &discarded_events(),
        )
        .await?;
        let mut mux = started(
            &input,
            admitted.segmentation.iter().copied().collect(),
            &discarded_events(),
        );
        let mut media = Vec::new();
        for sample in &samples {
            mux.muxer.push(sample.clone(), &mut media)?;
        }
        mux.muxer
            .finish(crate::mux::FinishReason::Final, &mut media)?;
        let bytes = concat_cmaf_bytes(&media);
        let demuxed = demux_cmaf(&bytes);
        let output = &demuxed.tracks[0].samples;
        assert_eq!(output.len(), samples.len());
        let origin = super::sample_dts(&samples[0]);
        for (actual, expected) in output.iter().zip(&samples) {
            assert_eq!(actual.duration.map(u64::from), Some(expected.duration()));
            assert_eq!(actual.dts, Some(super::sample_dts(expected) - origin));
            assert_eq!(actual.pts, Some(expected.pts() - origin));
        }
        let init = media
            .iter()
            .find_map(|event| match event {
                PackagedMedia::Initialization(init) => Some(init.payload.as_bytes()),
                _ => None,
            })
            .expect("initialization");
        let mut previous_end = None;
        for chunk in media.iter().filter_map(|event| match event {
            PackagedMedia::Chunk(chunk) => Some(chunk),
            _ => None,
        }) {
            let bytes = [init, chunk.payload.as_bytes()].concat();
            let demuxed = demux_cmaf(&bytes);
            let samples = &demuxed.tracks[0].samples;
            let start = samples
                .iter()
                .map(|sample| sample.pts.expect("PTS"))
                .min()
                .expect("samples");
            let end = samples
                .iter()
                .map(|sample| {
                    sample.pts.expect("PTS") + i64::from(sample.duration.expect("duration"))
                })
                .max()
                .expect("samples");
            if let Some(previous) = previous_end {
                assert!(start >= previous - 1, "part presentation ranges overlap");
            }
            previous_end = Some(end);
        }
        Ok(())
    }

    #[tokio::test]
    async fn long_av1_fixture_has_eighty_timed_video_frames() {
        let fixture = ts_bytes_fixture(include_bytes!(
            "../../../tests/apple_hls/fixtures/av1_long.ts"
        ))
        .await;
        assert_eq!(fixture.presentation.tracks().len(), 1);
        assert_eq!(
            fixture.presentation.tracks()[0].codec,
            crate::domain::Codec::Av1
        );
        assert_eq!(fixture.presentation.tracks()[0].kind(), MediaKind::Video);
        assert_eq!(fixture.samples.len(), 80);
        assert!(
            fixture
                .samples
                .iter()
                .all(|sample| sample.duration() == 9000)
        );
        assert!(
            fixture
                .samples
                .windows(2)
                .all(|pair| pair[1].pts() - pair[0].pts() == 9000)
        );
    }

    #[tokio::test]
    async fn av1g_and_opus_ts_package_as_cmaf() {
        for bytes in [
            &include_bytes!("../../../tests/apple_hls/fixtures/av1.ts")[..],
            &include_bytes!("../../../tests/apple_hls/fixtures/av1_long.ts")[..],
            &include_bytes!("../../../tests/apple_hls/fixtures/opus.ts")[..],
        ] {
            let fixture = ts_bytes_fixture(bytes).await;
            assert!(!fixture.samples.is_empty());
            let opus = fixture.presentation.tracks()[0].codec == crate::domain::Codec::Opus;
            let outputs = packages_as_demuxable_cmaf(fixture);
            if opus {
                let media = demux_cmaf(&outputs[0]);
                let samples = &media.tracks[0].samples;
                assert_eq!(samples.last().unwrap().duration, Some(312));
                let duration: u32 = samples.iter().map(|sample| sample.duration.unwrap()).sum();
                assert_eq!(duration - 312, 19_200);
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires ffmpeg executable"]
    async fn independent_decoder_accepts_native_ts_opus_and_av1g()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory =
            std::env::temp_dir().join(format!("rushls-decode-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&directory)?;
        let result = async {
            for (name, bytes) in [
                (
                    "av1",
                    &include_bytes!("../../../tests/apple_hls/fixtures/av1.ts")[..],
                ),
                (
                    "av1_long",
                    &include_bytes!("../../../tests/apple_hls/fixtures/av1_long.ts")[..],
                ),
                (
                    "opus",
                    &include_bytes!("../../../tests/apple_hls/fixtures/opus.ts")[..],
                ),
            ] {
                let output = packages_as_demuxable_cmaf(ts_bytes_fixture(bytes).await);
                let path = directory.join(format!("{name}.mp4"));
                std::fs::write(&path, &output[0])?;
                let decoded = std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-xerror", "-i"])
                    .arg(&path)
                    .args(["-f", "null", "-"])
                    .output()?;
                assert!(
                    decoded.status.success(),
                    "{name}: {}",
                    String::from_utf8_lossy(&decoded.stderr)
                );
            }
            // Compare the audible prefix with the TS decoder, including startup
            // alignment. FFmpeg currently retains the 648 padded samples at the
            // end of fragmented MP4 despite its shortened final trun duration.
            let decoded = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(directory.join("opus.mp4"))
                .args(["-f", "s16le", "-ac", "2", "-"])
                .output()?;
            assert!(decoded.status.success());
            let source = directory.join("opus.ts");
            std::fs::write(
                &source,
                include_bytes!("../../../tests/apple_hls/fixtures/opus.ts"),
            )?;
            let reference = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(source)
                .args(["-f", "s16le", "-ac", "2", "-"])
                .output()?;
            assert!(reference.status.success());
            // This TS decoder exposes all encoded samples; apply the control
            // header's declared trim to obtain the audible reference.
            assert_eq!(reference.stdout.len() / 4, 20_160);
            let audible_reference = &reference.stdout[312 * 4..(20_160 - 648) * 4];
            assert!((19_200..=19_848).contains(&(decoded.stdout.len() / 4)));
            // Float-to-PCM conversion can round by one least-significant bit.
            for (actual, expected) in decoded.stdout[..audible_reference.len()]
                .as_chunks::<2>()
                .0
                .iter()
                .zip(audible_reference.as_chunks::<2>().0)
            {
                let actual = i16::from_le_bytes([actual[0], actual[1]]);
                let expected = i16::from_le_bytes([expected[0], expected[1]]);
                assert!(i32::from(actual).abs_diff(i32::from(expected)) <= 1);
            }
            for (name, bytes) in [
                (
                    "he",
                    &include_bytes!("../../../tests/apple_hls/fixtures/he_aac.flv")[..],
                ),
                (
                    "hev2",
                    &include_bytes!("../../../tests/apple_hls/fixtures/hev2_aac.flv")[..],
                ),
            ] {
                let output = packages_as_demuxable_cmaf(flv_audio_fixture(bytes).await);
                let path = directory.join(format!("{name}.mp4"));
                std::fs::write(&path, &output[0])?;
                let decoded = std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-xerror", "-i"])
                    .arg(&path)
                    .args(["-f", "null", "-"])
                    .output()?;
                assert!(
                    decoded.status.success(),
                    "{name}: {}",
                    String::from_utf8_lossy(&decoded.stderr)
                );
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        std::fs::remove_dir_all(&directory)?;
        result
    }

    #[tokio::test]
    async fn mpeg_ts_normalizes_to_audio_and_video() {
        let fixture = mpeg_ts_fixture().await;
        assert!(
            fixture
                .samples
                .iter()
                .any(|sample| matches!(sample, NormalizedMedia::Video(_)))
                && fixture
                    .samples
                    .iter()
                    .any(|sample| matches!(sample, NormalizedMedia::Audio(_)))
        );
    }

    #[tokio::test]
    async fn normalized_mpeg_ts_packages_as_demuxable_cmaf() {
        packages_as_demuxable_cmaf(mpeg_ts_fixture().await);
    }

    #[tokio::test]
    async fn rtmp_normalizes_to_audio_and_video() {
        let fixture = rtmp_fixture().await;
        assert!(
            fixture
                .samples
                .iter()
                .any(|sample| matches!(sample, NormalizedMedia::Video(_)))
                && fixture
                    .samples
                    .iter()
                    .any(|sample| matches!(sample, NormalizedMedia::Audio(_)))
        );
    }

    #[tokio::test]
    async fn rtmp_opus_preserves_priming_through_normalization_and_cmaf()
    -> Result<(), Box<dyn std::error::Error>> {
        const PACKETS: usize = 201;
        let session = crate::observe::SessionMeters::new(crate::observe::ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        // Stereo OpusHead: 312 samples of pre-skip, 44.1 kHz input metadata.
        // Playback and pre-skip still use 48 kHz.
        let head = b"OpusHead\x01\x02\x38\x01\x44\xac\x00\x00\x00\x00\x00";
        let tags = std::iter::once((0, 0x90, &head[..])).chain(
            (0..u32::try_from(PACKETS)?).map(|index| (index * 20, 0x91, &[0xf8, 0xff, 0xfe][..])),
        );
        for (timestamp, tag, payload) in tags {
            let mut raw = vec![tag];
            raw.extend_from_slice(b"Opus");
            raw.extend_from_slice(payload);
            writer
                .send(IngressEvent::Audio {
                    timestamp,
                    media: rtmpx::ValidatedMedia::parse_audio(
                        bytes::Bytes::from(raw),
                        rtmpx::EnhancedValidationMode::Strict,
                    )?,
                })
                .await?;
        }
        writer.finish(InputState::Closed);
        let source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), session.source_view())?;
        let fixture = ingest_fixture(source).await;
        let track = &fixture.presentation.tracks()[0];
        assert_eq!(track.first_pts, Some(312));
        assert_eq!(track.timebase, Timebase::new(nz::u32!(1), nz::u32!(48_000)));
        assert_eq!(fixture.samples.len(), PACKETS);
        let NormalizedMedia::Audio(first) = &fixture.samples[0] else {
            panic!("audio sample")
        };
        assert_eq!(
            (first.pts, first.duration, first.trim.leading_samples),
            (0, 960, 312)
        );
        let mut cursor = crate::media::PresentedTimingCursor::for_track(track);
        let presented = cursor.next(&fixture.samples[0])?;
        assert_eq!((presented.start, presented.duration), (312, 648));

        assert_opus_part_cadence(&fixture)?;
        let mut output =
            super::output::CmafOutput::open(track, crate::domain::PipelineBudget::unlimited())
                .expect("Opus output opens");
        for sample in &fixture.samples {
            let charge = output.reserve(sample.payload_len())?;
            output.write(sample, sample.pts() - 312, sample.pts() - 312, charge);
        }
        let init = output.flush_fragment().expect("Opus init");
        assert_eq!(edit_list(&init), [(0, 312)]);
        let media = output.flush_fragment().expect("Opus media");
        let mut bytes = init.as_bytes().to_vec();
        bytes.extend_from_slice(media.as_bytes());
        let demuxed = demux_cmaf(&bytes);
        let transmux::CodecConfig::Opus {
            config,
            sample_rate,
            ..
        } = &demuxed.tracks[0].spec.config
        else {
            panic!("Opus config")
        };
        assert_eq!(*sample_rate, 48_000);
        assert_eq!(
            (
                config.version,
                config.output_channel_count,
                config.pre_skip,
                config.input_sample_rate
            ),
            (0, 2, 312, 44_100)
        );
        assert_eq!(demuxed.tracks[0].samples.len(), PACKETS);
        assert_eq!(demuxed.tracks[0].samples[0].duration, Some(960));
        // The roll `sbgp` is framed into `traf` by hand, so the mdat offset
        // must account for it or every packet would be read shifted.
        for (demuxed, sample) in demuxed.tracks[0].samples.iter().zip(&fixture.samples) {
            let NormalizedMedia::Audio(sample) = sample else {
                panic!("audio sample")
            };
            assert_eq!(demuxed.data.as_ref(), sample.payload.as_bytes());
        }
        let mut remaining = media.as_bytes();
        let mut groups = 0;
        while !remaining.is_empty() {
            let (atom, size) = transmux::parse_box(remaining)?;
            if atom.header.box_type.is(b"moof") {
                for child in transmux::box_iter(atom.body) {
                    let (child, _) = child?;
                    if child.header.box_type.is(b"traf") {
                        for grandchild in transmux::box_iter(child.body) {
                            groups += usize::from(grandchild?.0.header.box_type.is(b"sbgp"));
                        }
                    }
                }
            }
            remaining = &remaining[size..];
        }
        assert!(groups > 0, "Opus fragments carry roll groups");
        Ok(())
    }

    fn assert_opus_part_cadence(fixture: &IngestFixture) -> Result<(), Box<dyn std::error::Error>> {
        let track = &fixture.presentation.tracks()[0];
        // Exercise the real planner as well as the codec output. A pre-skip
        // larger than 15% of one packet must not reject a steady packet grid,
        // even when the requested part holds only one packet.
        for desired_part_duration in [Duration::from_millis(20), Duration::from_millis(200)] {
            let mut observer = crate::segment::CadenceObserver::new(
                &fixture.presentation,
                &fixture.timeline,
                crate::segment::SegmentationPolicy {
                    maximum_part_duration: Duration::from_secs(2),
                    early_boundary: Duration::ZERO,
                    late_boundary: Duration::ZERO,
                    desired_segment_duration: Duration::from_secs(2),
                    desired_part_duration,
                    maximum_segment_duration: Duration::from_secs(2),
                },
            )?;
            for sample in &fixture.samples {
                observer.observe(sample)?;
            }
            let plans = observer.plan()?.expect("steady Opus cadence is ready");
            let target = plans[0].part_duration.get();
            assert_eq!(
                target,
                track.timebase.duration_to_ticks(desired_part_duration)
            );
            let mut mux = started(&fixture.presentation, plans, &discarded_events());
            let mut packaged = Vec::new();
            for sample in &fixture.samples {
                mux.muxer.push(sample.clone(), &mut packaged)?;
            }
            mux.muxer
                .finish(crate::mux::FinishReason::Final, &mut packaged)?;
            let chunks: Vec<_> = packaged
                .iter()
                .filter_map(|event| match event {
                    PackagedMedia::Chunk(chunk) => Some(chunk),
                    _ => None,
                })
                .collect();
            assert!(chunks.len() > 2);
            assert!(chunks[0].duration <= target);
            for chunk in &chunks[..chunks.len() - 1] {
                assert!(chunk.duration <= target);
                assert!(
                    chunk.independent
                        || u128::from(chunk.duration) * 100 >= u128::from(target) * 85
                );
            }
            let demuxed = demux_cmaf(&concat_cmaf_bytes(&packaged));
            assert_eq!(demuxed.tracks[0].samples.len(), fixture.samples.len());
        }
        Ok(())
    }

    #[tokio::test]
    async fn normalized_rtmp_packages_as_demuxable_cmaf() {
        packages_as_demuxable_cmaf(rtmp_fixture().await);
    }

    #[test]
    fn rebases_pts_and_preserves_negative_dts_at_the_shared_origin() {
        let sink = discarded_events();
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
    fn video_parts_regain_independence_at_idr_after_gap() -> Result<(), Box<dyn std::error::Error>>
    {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(16_384));
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())?;
        let mut mux = started(
            &input,
            vec![
                PlanBuilder::new(0, timebase, nz::u64!(65_536))
                    .part(nz::u32!(2), nz::u64!(16_384))
                    .build(),
            ],
            &discarded_events(),
        );
        let frame_ticks = i64::try_from(FRAME)?;
        let mut out = Vec::new();
        mux.muxer.push(sample(0, true), &mut out)?;
        mux.muxer.push(
            NormalizedMedia::Gap(crate::media::MissingInterval {
                track_id: TrackId(0),
                media_kind: MediaKind::Video,
                start: i64::try_from(FRAME)?,
                end: i64::try_from(2 * FRAME)?,
                timebase,
            }),
            &mut out,
        )?;
        for frame in 2..8 {
            mux.muxer
                .push(sample(frame * i64::try_from(FRAME)?, frame == 4), &mut out)?;
        }
        mux.muxer
            .finish(crate::mux::FinishReason::Final, &mut out)?;
        let parts: Vec<_> = out
            .iter()
            .filter_map(|m| match m {
                PackagedMedia::Chunk(c) => Some(c),
                _ => None,
            })
            .collect();
        let resumed = parts
            .iter()
            .find(|p| p.media_start == 2 * frame_ticks)
            .expect("resumed part");
        assert!(!resumed.independent);
        assert!(
            parts
                .iter()
                .any(|p| p.media_start >= 4 * frame_ticks && p.independent)
        );
        Ok(())
    }

    #[test]
    fn a_part_containing_a_later_idr_is_marked_independent()
    -> Result<(), Box<dyn std::error::Error>> {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(16_384));
        let input = validate(&catalog(vec![track(timebase)]), &StreamPolicy::permissive())?;
        let mut started = started(
            &input,
            vec![
                PlanBuilder::new(0, timebase, nz::u64!(65_536))
                    .part(nz::u32!(2), nz::u64!(16_384))
                    .build(),
            ],
            &discarded_events(),
        );
        let mut media = Vec::new();
        for frame in 0..4 {
            started.muxer.push(
                sample_for(0, frame * i64::try_from(FRAME)?, frame == 0 || frame == 3),
                &mut media,
            )?;
        }
        started
            .muxer
            .finish(crate::mux::FinishReason::Final, &mut media)?;
        let parts: Vec<_> = media
            .iter()
            .filter_map(|event| match event {
                PackagedMedia::Chunk(chunk) => Some(chunk),
                _ => None,
            })
            .collect();
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|part| part.independent));
        assert!(parts.iter().all(|part| part.duration == 2 * FRAME));
        Ok(())
    }

    #[test]
    fn finish_flushes_final_and_interrupted_tails_but_not_superseded_tails() {
        let sink = discarded_events();
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
        let sink = discarded_events();
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
            started.muxer.push(sample(16_385, false), &mut media),
            Err(crate::mux::MuxError::BoundaryWindow { .. })
        ));
    }

    #[test]
    fn extension_mode_warns_and_keeps_parts_flowing_until_a_keyframe() {
        let (sink, recorder) = RecordedEvents::sink();
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
            recorder.events().as_slice(),
            [SessionEvent::SegmentationExtended {
                track: TrackId(0),
                ..
            }]
        ));
    }

    #[test]
    fn an_exhausted_extension_budget_fails_instead_of_overrunning_the_target() {
        let sink = discarded_events();
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
                Err(crate::mux::MuxError::BoundaryWindow { .. })
            ),
            "past the budget the publication fails: its parts are already \
             fetchable, so there is no later point at which an overrun could be \
             contained"
        );
    }

    #[test]
    fn accepts_webvtt_and_preserves_the_track_timebase() {
        let sink = discarded_events();
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
        let started = PassThroughMuxerFactory
            .start(MuxerStartRequest {
                presentation: &subtitle_input,
                segmentation: &segmentation,
                time_anchor: SystemTime::UNIX_EPOCH,
                events: &sink,
                budget: &crate::domain::PipelineBudget::unlimited(),
            })
            .expect("mixed CMAF and WebVTT presentation starts");
        assert_eq!(
            started.presentation.renditions[1].config.segment_format,
            crate::mux::MediaSegmentFormat::WebVtt
        );
        assert_eq!(started.presentation.renditions[1].codecs.as_ref(), "wvtt");

        let millisecond = Timebase::new(nz::u32!(1), nz::u32!(1_000));
        let started = start(SegmentBoundaryPolicy::Strict, millisecond, &sink)
            .expect("native CMAF keeps the track timebase");
        assert_eq!(
            started.presentation.renditions[0].config.timebase,
            millisecond
        );
    }
    /// Packaging experiment only: video normalization still uses its existing policy.
    /// Replace a normalized P picture with an exact interval, without synthesizing
    /// media or changing any surviving packet's duration or timestamps.
    #[tokio::test]
    #[ignore = "requires ffmpeg and RUSHLS_GAP_FIXTURES for external browser tests"]
    async fn ip_video_gap_playback_fixture() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = encoded_ip_video_fixture().await?;
        assert_eq!(fixture.samples.len(), 200);
        let track = &fixture.presentation.tracks()[0];
        let origin = fixture.samples[0].pts();
        let rate = u64::from(track.timebase.den().get());
        for omit in [false, true] {
            let plan = PlanBuilder::new(
                track.id.0,
                track.timebase,
                NonZero::new(rate * 2).ok_or("rate")?,
            )
            .part(nz::u32!(1), NonZero::new(rate / 5).ok_or("rate")?)
            .presentation_origin(origin)
            .segmentation_origin(origin)
            .build();
            let sink = discarded_events();
            let mut mux = started(&fixture.presentation, vec![plan], &sink);
            let mut media = Vec::new();
            for (index, sample) in fixture.samples.iter().enumerate() {
                let NormalizedMedia::Video(video) = sample else {
                    panic!("video fixture")
                };
                assert_eq!(video.pts, video.dts, "no presentation reordering");
                assert_eq!(video.random_access, index % 50 == 0);
                let item = if omit && index == 126 {
                    assert!(!video.random_access);
                    NormalizedMedia::Gap(crate::media::MissingInterval {
                        track_id: video.track_id,
                        media_kind: MediaKind::Video,
                        start: video.pts,
                        end: video.pts + i64::try_from(video.duration)?,
                        timebase: track.timebase,
                    })
                } else {
                    sample.clone()
                };
                mux.muxer.push(item, &mut media)?;
            }
            mux.muxer
                .finish(crate::mux::FinishReason::Final, &mut media)?;
            let gaps: Vec<_> = media
                .iter()
                .filter_map(|item| match item {
                    PackagedMedia::Gap(gap) => Some(gap),
                    _ => None,
                })
                .collect();
            assert_eq!(gaps.len(), usize::from(omit));
            if omit {
                assert_eq!(gaps[0].media_start, i64::try_from(rate * 126 / 25)?);
                assert_eq!(gaps[0].duration, rate / 25);
            }
            let demuxed = demux_cmaf(&concat_cmaf_bytes(&media));
            let output = &demuxed.tracks[0].samples;
            assert_eq!(output.len(), 200 - usize::from(omit));
            let expected = fixture
                .samples
                .iter()
                .enumerate()
                .filter(|(index, _)| !omit || *index != 126)
                .map(|(_, sample)| sample);
            for (actual, expected) in output.iter().zip(expected) {
                assert_eq!(actual.duration.map(u64::from), Some(expected.duration()));
                assert_eq!(actual.dts, Some(super::sample_dts(expected) - origin));
                assert_eq!(actual.pts, Some(expected.pts() - origin));
            }
            export_gap_fixture(&mux.presentation, &media, track, omit)?;
        }
        Ok(())
    }

    async fn encoded_ip_video_fixture() -> Result<IngestFixture, Box<dyn std::error::Error>> {
        let encoded = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=25:duration=8",
                "-c:v",
                "libx264",
                "-preset",
                "medium",
                "-g",
                "50",
                "-bf",
                "0",
                "-x264-params",
                "scenecut=0:ref=1",
                "-an",
                "-f",
                "mpegts",
                "-",
            ])
            .output()?;
        assert!(
            encoded.status.success(),
            "{}",
            String::from_utf8_lossy(&encoded.stderr)
        );
        Ok(ts_bytes_fixture(&encoded.stdout).await)
    }

    fn gap_fixture_as_cmaf(
        fixture: &IngestFixture,
        rate: u32,
    ) -> Result<Vec<PackagedMedia>, Box<dyn std::error::Error>> {
        if std::env::var_os("RUSHLS_GAP_FIXTURES").is_some() {
            audio_fixture_as_cmaf(fixture, rate, false)?;
        }
        audio_fixture_as_cmaf(fixture, rate, true)
    }

    fn audio_fixture_as_cmaf(
        fixture: &IngestFixture,
        rate: u32,
        omit_packets: bool,
    ) -> Result<Vec<PackagedMedia>, Box<dyn std::error::Error>> {
        let mut normalized = PassThroughNormalizerFactory.start(
            &fixture.presentation,
            &fixture.timeline,
            crate::domain::InputMode::Permissive,
        )?;
        let mut samples = Vec::new();
        for (index, sample) in fixture.samples.iter().enumerate() {
            if omit_packets && (10..16).contains(&index) {
                continue;
            }
            let NormalizedMedia::Audio(audio) = sample else {
                panic!("audio fixture");
            };
            normalized.normalizer.push(
                crate::source::Packet {
                    track_id: audio.track_id,
                    pts: Some(audio.pts),
                    dts: Some(audio.pts),
                    duration: Some(i64::try_from(audio.duration)?),
                    random_access: false,
                    audio_trim: audio.trim,
                    webvtt: crate::domain::WebVttCueMetadata::default(),
                    subtitle_position: None,
                    payload: audio.payload.clone(),
                },
                &mut samples,
            )?;
        }
        normalized.normalizer.finish(&mut samples)?;
        let notices = normalized.normalizer.take_notices();
        assert_eq!(notices.len(), usize::from(omit_packets));
        assert_eq!(
            samples.len(),
            fixture.samples.len() - usize::from(omit_packets) * 5
        );
        let track = &normalized.presentation.tracks()[0];
        let plan = PlanBuilder::new(
            track.id.0,
            track.timebase,
            NonZero::new(
                (u64::from(rate) * 2).div_ceil(samples[0].duration()) * samples[0].duration(),
            )
            .ok_or("rate")?,
        )
        .part(
            nz::u32!(1),
            NonZero::new(u64::from(rate) / 5).ok_or("rate")?,
        )
        .presentation_origin(fixture.timeline.get(track.id).ok_or("timeline")?.origin_pts)
        .segmentation_origin(samples[0].pts())
        .build();
        let sink = discarded_events();
        let mut mux = started(&normalized.presentation, vec![plan], &sink);
        let mut media = Vec::new();
        for sample in samples {
            mux.muxer.push(sample, &mut media)?;
        }
        mux.muxer
            .finish(crate::mux::FinishReason::Final, &mut media)?;
        assert_eq!(
            media
                .iter()
                .filter(|item| matches!(item, PackagedMedia::Initialization(_)))
                .count(),
            1
        );
        let chunks: Vec<_> = media
            .iter()
            .filter_map(|item| {
                if let PackagedMedia::Chunk(chunk) = item {
                    Some(chunk)
                } else {
                    None
                }
            })
            .collect();
        assert!(chunks.len() > 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.duration <= u64::from(rate) / 5)
        );
        let mut cursor = chunks[0].media_start;
        for item in &media {
            let (start, duration) = match item {
                PackagedMedia::Chunk(chunk) => (chunk.media_start, chunk.duration),
                PackagedMedia::Gap(gap) => (gap.media_start, gap.duration),
                _ => continue,
            };
            assert_eq!(start, cursor);
            cursor = cursor
                .checked_add_unsigned(duration)
                .ok_or("interval overflow")?;
        }
        export_gap_fixture(&mux.presentation, &media, track, omit_packets)?;
        Ok(media)
    }

    fn export_gap_fixture(
        presentation: &std::sync::Arc<crate::mux::PackagedPresentation>,
        media: &[PackagedMedia],
        track: &crate::domain::DiscoveredTrack,
        omit_packets: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use crate::delivery::{
            hls::{
                project::{PlaylistDelta, PlaylistPolicy, media::media_playlist},
                uri::PlaylistUris,
            },
            store::{SegmentBody, StoredSegmentKind, StreamStore},
        };
        let Some(root) = std::env::var_os("RUSHLS_GAP_FIXTURES") else {
            return Ok(());
        };
        let mut asc = String::new();
        // FLAC initialization can contain long metadata blocks. The first
        // 32 bytes identify these fixed fixtures without exceeding NAME_MAX.
        for byte in track.codec_extradata.as_bytes().iter().take(32) {
            std::fmt::Write::write_fmt(&mut asc, format_args!("{byte:02x}"))?;
        }
        // Controls use the exact same encoded packets and initialization.
        let suffix = if omit_packets { "" } else { "-control" };
        let directory = std::path::PathBuf::from(root).join(format!(
            "{:?}-{}-{asc}{suffix}",
            track.codec,
            track.timebase.den()
        ));
        std::fs::create_dir_all(&directory)?;
        let store = StreamStore::default();
        let lease = store.lease(crate::domain::StreamId::new("gap-fixture"), presentation)?;
        for item in media {
            assert!(lease.write(item.clone())?);
        }
        assert!(lease.end());
        let stream = lease.live().snapshot();
        let rendition = lease
            .live()
            .rendition(crate::domain::RenditionId(0))
            .ok_or("rendition")?;
        let uris = PlaylistUris::default();
        let names = uris.within(
            crate::domain::RenditionId(0),
            super::MediaSegmentFormat::Cmaf,
        );
        let playlist = media_playlist(
            &stream,
            &rendition,
            crate::delivery::hls::project::presentation_server_control(
                &stream,
                crate::delivery::hls::project::DeliveryTimingPolicy::default(),
            ),
            &PlaylistPolicy::default(),
            &uris,
            PlaylistDelta::Full,
        )?;
        std::fs::write(directory.join("index.m3u8"), &playlist)?;
        let full = playlist
            .lines()
            .filter(|line| {
                !line.starts_with("#EXT-X-PART") && !line.starts_with("#EXT-X-SERVER-CONTROL")
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(directory.join("full.m3u8"), full)?;
        let codec = track.rfc6381_codec().ok_or("fixture codec")?;
        for (name, child) in [
            ("master.m3u8", "index.m3u8"),
            ("master-full.m3u8", "full.m3u8"),
        ] {
            std::fs::write(
                directory.join(name),
                format!(
                    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=256000,CODECS=\"{codec}\"\n{child}\n"
                ),
            )?;
        }
        let mut uri = String::new();
        let write = |name: &str, bytes: &[u8]| -> std::io::Result<()> {
            let path = directory.join(name);
            std::fs::create_dir_all(path.parent().expect("fixture parent"))?;
            std::fs::write(path, bytes)
        };
        for init in rendition.initializations.iter() {
            write(
                names
                    .initialization(init.id, &mut uri)
                    .ok_or("initialization URI")?,
                init.payload.as_bytes(),
            )?;
        }
        for segment in rendition.segments.iter() {
            if let StoredSegmentKind::Media(SegmentBody::Chunked(parts)) = &segment.kind {
                let mut bytes = Vec::new();
                for part in parts.iter() {
                    write(names.part(part.id, &mut uri), part.payload.as_bytes())?;
                    bytes.extend_from_slice(part.payload.as_bytes());
                }
                write(names.segment(segment.id, &mut uri), &bytes)?;
            }
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires macOS ffmpeg aac_at encoder and AAC decoder"]
    async fn he_aac_gaps_decode_and_resume_without_replacement_packets()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory =
            std::env::temp_dir().join(format!("rushls-he-gap-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&directory)?;
        let result = async {
            for rate in [44_100_u32, 48_000] {
                for (profile, channels, bitrate) in [(4, 1, 24000), (4, 2, 48000), (28, 2, 24000)] {
                    let encoded = std::process::Command::new("ffmpeg")
                        .args(["-v", "error", "-f", "lavfi", "-i"])
                        .arg(format!("sine=frequency=440:sample_rate={rate}:duration=3"))
                        .args([
                            "-ac",
                            &channels.to_string(),
                            "-c:a",
                            "aac_at",
                            "-profile:a",
                            &profile.to_string(),
                            "-b:a",
                            &bitrate.to_string(),
                            "-f",
                            "flv",
                            "-",
                        ])
                        .output()?;
                    assert!(
                        encoded.status.success(),
                        "{}",
                        String::from_utf8_lossy(&encoded.stderr)
                    );
                    let fixture = flv_audio_fixture(&encoded.stdout).await;
                    let media = gap_fixture_as_cmaf(&fixture, rate)?;
                    let bytes = concat_cmaf_bytes(&media);
                    let init = media
                        .iter()
                        .find_map(|item| match item {
                            PackagedMedia::Initialization(init) => Some(init.payload.clone()),
                            _ => None,
                        })
                        .ok_or("initialization")?;
                    let gap_end = media
                        .iter()
                        .find_map(|item| match item {
                            PackagedMedia::Gap(gap) => {
                                Some(gap.media_start + i64::try_from(gap.duration).ok()?)
                            }
                            _ => None,
                        })
                        .ok_or("gap")?;
                    let mut suffix = init.as_bytes().to_vec();
                    for item in &media {
                        if let PackagedMedia::Chunk(chunk) = item
                            && chunk.media_start >= gap_end
                        {
                            suffix.extend_from_slice(chunk.payload.as_bytes());
                        }
                    }
                    for (name, bytes) in [("continuous", bytes), ("fresh-resume", suffix)] {
                        let path =
                            directory.join(format!("{rate}-{profile}-{channels}-{name}.mp4"));
                        std::fs::write(&path, bytes)?;
                        let decoded = std::process::Command::new("ffmpeg")
                            .args(["-v", "error", "-xerror", "-i"])
                            .arg(&path)
                            .args(["-f", "s16le", "-"])
                            .output()?;
                        assert!(
                            decoded.status.success() && decoded.stderr.is_empty(),
                            "{rate}/{profile}/{channels}/{name}: {}",
                            String::from_utf8_lossy(&decoded.stderr)
                        );
                        assert!(!decoded.stdout.is_empty());
                    }
                }
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        std::fs::remove_dir_all(directory)?;
        result
    }

    fn assert_clean_aac_tail(
        decoded_audio: &[Vec<u8>],
        missing_ticks: u64,
        channels: usize,
        rate: u32,
    ) {
        // Skip ten real AUs after the hole for filter overlap.
        // A valid duration alone can hide persistent decoder damage.
        let tail = 40 * 1024 * channels * 2;
        let resumed_tail =
            tail - usize::try_from(missing_ticks).expect("fixture hole fits usize") * channels * 2;
        assert!(decoded_audio[0].len() > tail);
        let mut signal_energy = 0.0;
        let mut error_energy = 0.0;
        for (original, repaired) in decoded_audio[0][tail..]
            .as_chunks::<2>()
            .0
            .iter()
            .zip(decoded_audio[1][resumed_tail..].as_chunks::<2>().0)
        {
            let original = f64::from(i16::from_le_bytes(*original));
            let repaired = f64::from(i16::from_le_bytes(*repaired));
            signal_energy += original * original;
            error_energy += (original - repaired).powi(2);
        }
        // PNS uses decoder-local noise state, so dropping real
        // packets need not produce bit-identical PCM. Bound the
        // residual error to 1% RMS of this audible tone instead.
        assert!(signal_energy > 0.0);
        assert!(
            error_energy <= signal_energy * 0.0001,
            "{rate}/{channels}: AAC tail relative RMS error {}",
            (error_energy / signal_energy).sqrt()
        );
    }

    #[tokio::test]
    #[ignore = "requires ffmpeg executable with AAC and libopus encoders"]
    async fn gapped_audio_decodes_through_cmaf_for_each_supported_configuration()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory =
            std::env::temp_dir().join(format!("rushls-repair-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&directory)?;
        let result = async {
            for (codec, rate) in [("aac", 44_100u32), ("aac", 48_000), ("libopus", 48_000)] {
                for channels in [1usize, 2] {
                    let encoded = std::process::Command::new("ffmpeg")
                        .args(["-v", "error", "-f", "lavfi", "-i"])
                        .arg(format!(
                            "sine=frequency=440:sample_rate={rate}:duration=1.5"
                        ))
                        .args([
                            "-ac",
                            &channels.to_string(),
                            "-c:a",
                            codec,
                            "-f",
                            "mpegts",
                            "-",
                        ])
                        .output()?;
                    assert!(
                        encoded.status.success(),
                        "{}",
                        String::from_utf8_lossy(&encoded.stderr)
                    );
                    let fixture = ts_bytes_fixture(&encoded.stdout).await;
                    let baseline = packages_as_demuxable_cmaf(IngestFixture {
                        presentation: fixture.presentation.clone(),
                        timeline: fixture.timeline.clone(),
                        samples: fixture.samples.clone(),
                    });
                    let media = gap_fixture_as_cmaf(&fixture, rate)?;
                    let repaired = concat_cmaf_bytes(&media);
                    let missing_ticks: u64 = media
                        .iter()
                        .filter_map(|item| match item {
                            PackagedMedia::Gap(gap) => Some(gap.duration),
                            _ => None,
                        })
                        .sum();
                    let output = demux_cmaf(&repaired);
                    let original = demux_cmaf(&baseline[0]);
                    assert_eq!(
                        output.tracks[0].samples.len(),
                        original.tracks[0].samples.len() - 6
                    );
                    for (actual, expected) in output.tracks[0].samples.iter().zip(
                        original.tracks[0]
                            .samples
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| !(10..16).contains(index))
                            .map(|(_, sample)| sample),
                    ) {
                        assert_eq!(actual.duration, expected.duration);
                    }
                    let mut decoded_audio = Vec::new();
                    for (name, bytes) in [
                        ("baseline", baseline[0].as_slice()),
                        ("repaired", repaired.as_slice()),
                    ] {
                        let path = directory.join(format!("{codec}-{rate}-{channels}-{name}.mp4"));
                        std::fs::write(&path, bytes)?;
                        let decoded = std::process::Command::new("ffmpeg")
                            .args(["-v", "error", "-xerror", "-i"])
                            .arg(path)
                            .args(["-f", "s16le", "-"])
                            .output()?;
                        assert!(
                            decoded.status.success(),
                            "{codec}/{rate}/{channels}: {}",
                            String::from_utf8_lossy(&decoded.stderr)
                        );
                        // AAC SBR failures can produce PCM and exit successfully,
                        // even with -xerror. Diagnostics are also a release gate.
                        assert!(
                            decoded.stderr.is_empty(),
                            "{codec}/{rate}/{channels}: {}",
                            String::from_utf8_lossy(&decoded.stderr)
                        );
                        assert!(!decoded.stdout.is_empty());
                        decoded_audio.push(decoded.stdout);
                    }
                    assert_eq!(
                        decoded_audio[0].len(),
                        decoded_audio[1].len() + usize::try_from(missing_ticks)? * channels * 2,
                        "raw decoding contains only available samples"
                    );
                    if codec == "aac" {
                        assert_clean_aac_tail(&decoded_audio, missing_ticks, channels, rate);
                    }
                }
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        std::fs::remove_dir_all(directory)?;
        result
    }
    async fn flac_fixture(bytes: &[u8]) -> IngestFixture {
        use crate::media::fixtures::{flac_event, flac_parts};
        let (header, frames) = flac_parts(bytes);
        let session = crate::observe::SessionMeters::new(crate::observe::ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(128 * 1024));
        writer
            .send(flac_event(0, 0, header))
            .await
            .expect("config queues");
        writer
            .send(flac_event(0, 1, frames))
            .await
            .expect("packed frames queue");
        writer.finish(InputState::Closed);
        ingest_fixture(
            RtmpPacketSource::new(reader, InputLimits::permissive(), session.source_view())
                .expect("source"),
        )
        .await
    }

    #[tokio::test]
    async fn flac_rtmp_frames_package_with_exact_sample_clock()
    -> Result<(), Box<dyn std::error::Error>> {
        for (bytes, rate) in [
            (crate::media::fixtures::FLAC_MONO, 44100),
            (crate::media::fixtures::FLAC_STEREO, 48000),
        ] {
            let fixture = flac_fixture(bytes).await;
            assert_eq!(
                fixture.presentation.tracks()[0].codec,
                crate::domain::Codec::Flac
            );
            assert_eq!(
                fixture
                    .samples
                    .iter()
                    .map(NormalizedMedia::duration)
                    .sum::<u64>(),
                rate * 71 / 100
            );
            let output = packages_as_demuxable_cmaf(fixture);
            let demuxed = demux_cmaf(&output[0]);
            assert!(matches!(
                demuxed.tracks[0].spec.config,
                transmux::CodecConfig::Flac { .. }
            ));
            assert_eq!(
                demuxed.tracks[0]
                    .samples
                    .iter()
                    .map(|s| u64::from(s.duration.expect("duration")))
                    .sum::<u64>(),
                rate * 71 / 100
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ffmpeg FLAC decoder"]
    async fn flac_mp4_decodes_losslessly_and_resumes_after_gaps()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!("rushls-flac-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&directory)?;
        let result = async {
            for (bytes, rate, channels) in [
                (crate::media::fixtures::FLAC_MONO, 44100, 1),
                (crate::media::fixtures::FLAC_STEREO, 48000, 2),
            ] {
                let source = directory.join("source.flac");
                std::fs::write(&source, bytes)?;
                let decode =
                    |path: &std::path::Path| -> Result<Vec<u8>, Box<dyn std::error::Error>> {
                        let output = std::process::Command::new("ffmpeg")
                            .args(["-v", "error", "-xerror", "-i"])
                            .arg(path)
                            .args(["-f", "s32le", "-acodec", "pcm_s32le", "-"])
                            .output()?;
                        assert!(
                            output.status.success(),
                            "{}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                        Ok(output.stdout)
                    };
                let reference = decode(&source)?;
                let fixture = flac_fixture(bytes).await;
                let output = packages_as_demuxable_cmaf(fixture);
                let path = directory.join("output.mp4");
                std::fs::write(&path, &output[0])?;
                assert_eq!(decode(&path)?, reference);
                let fixture = flac_fixture(bytes).await;
                let media = audio_fixture_as_cmaf(&fixture, rate, true)?;
                assert!(media.iter().any(|m| matches!(m, PackagedMedia::Gap(_))));
                let outputs = collect_rendition_bytes(&media);
                std::fs::write(&path, &outputs[0])?;
                let decoded = decode(&path)?;
                let start = 10 * 1024 * channels * 4;
                let end = 16 * 1024 * channels * 4;
                let mut expected = reference[..start].to_vec();
                expected.extend_from_slice(&reference[end..]);
                assert_eq!(decoded, expected);
                let init = media
                    .iter()
                    .find_map(|item| match item {
                        PackagedMedia::Initialization(init) => Some(init.payload.as_bytes()),
                        _ => None,
                    })
                    .ok_or("FLAC initialization")?;
                // Every part, including the first after a gap, must decode from
                // a fresh decoder without preceding FLAC frames.
                for item in &media {
                    if let PackagedMedia::Chunk(chunk) = item {
                        let mut bytes = init.to_vec();
                        bytes.extend_from_slice(chunk.payload.as_bytes());
                        std::fs::write(&path, bytes)?;
                        let start = usize::try_from(chunk.media_start)? * channels * 4;
                        let end = start + usize::try_from(chunk.duration)? * channels * 4;
                        assert_eq!(decode(&path)?, reference[start..end]);
                    }
                }
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        std::fs::remove_dir_all(directory)?;
        result
    }
}

#[cfg(test)]
mod boundary_accounting_tests {
    use super::*;
    #[test]
    fn an_overshoot_is_rejected_before_the_fragment_is_published() -> Result<(), MuxError> {
        let track = crate::domain::fixtures::TrackBuilder::new(0, MediaKind::Video)
            .codec_extradata(crate::mux::fixtures::H264_EXTRADATA)
            .build();
        let plan =
            crate::segment::fixtures::PlanBuilder::new(0, track.timebase, nz::u64!(90_000)).build();
        let events = crate::mux::fixtures::discarded_events();
        let (_, mut writer) = build_track(
            PackagingRenditionId(0),
            &track,
            plan,
            Duration::ZERO,
            events,
            crate::domain::PipelineBudget::unlimited(),
        )?;
        let mut out = Vec::new();
        writer.push(
            crate::media::fixtures::video_sample(0, 90_000, true, 1),
            &mut out,
        )?;
        assert!(matches!(
            writer.cut(89_900, &mut out),
            Err(MuxError::Boundary {
                observed: 100,
                maximum: 0,
                ..
            })
        ));
        assert!(out.is_empty());
        Ok(())
    }
}

#[cfg(test)]
mod gap_tests {
    use super::*;
    use crate::{
        domain::{Timebase, fixtures::TrackBuilder},
        media::{
            MissingInterval,
            fixtures::{audio_sample, presentation, video_sample},
        },
        mux::{
            Muxer,
            coordinator::Coordinator,
            fixtures::{H264_EXTRADATA, discarded_events},
        },
        segment::{SegmentationPlan, fixtures::PlanBuilder},
    };

    fn assert_gap_coverage(
        out: &[PackagedMedia],
        missing_frames: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut audio_end = 0;
        let mut absent = 0;
        for item in out {
            match item {
                PackagedMedia::Chunk(chunk) if chunk.rendition_id.0 == 0 => {
                    assert_eq!(chunk.media_start, audio_end);
                    assert!(chunk.duration <= 9600);
                    audio_end += i64::try_from(chunk.duration)?;
                }
                PackagedMedia::Gap(gap) => {
                    assert_eq!(gap.media_start, audio_end);
                    assert_eq!(gap.parts.iter().sum::<u64>(), gap.duration);
                    assert!(
                        gap.parts
                            .iter()
                            .all(|duration| *duration > 0 && *duration <= 9600)
                    );
                    absent += gap.duration;
                    audio_end += i64::try_from(gap.duration)?;
                }
                PackagedMedia::SegmentCompleted(segment) if segment.rendition_id.0 == 1 => {
                    assert_eq!(segment.duration, 98_304);
                    assert_eq!(segment.media_start % 98_304, 0);
                }
                _ => {}
            }
        }
        assert_eq!(absent, u64::try_from(missing_frames * 1024)?);
        assert_eq!(audio_end, 288 * 1024);
        assert_eq!(out.iter().filter(|item| matches!(item, PackagedMedia::Initialization(init) if init.rendition_id.0 == 0)).count(), 1);
        Ok(())
    }

    #[test]
    fn local_audio_gaps_preserve_global_boundaries_and_exact_coverage()
    -> Result<(), Box<dyn std::error::Error>> {
        for with_video in [false, true] {
            for (first_missing, missing_frames) in [(1, 1), (9, 1), (90, 20), (96, 1)] {
                let clock = Timebase::new(nz::u32!(1), nz::u32!(48_000));
                let mut tracks = vec![
                    TrackBuilder::new(0, MediaKind::Audio)
                        .timebase(clock)
                        .codec_extradata(&[0x11, 0x90][..])
                        .build(),
                ];
                if with_video {
                    tracks.push(
                        TrackBuilder::new(1, MediaKind::Video)
                            .timebase(clock)
                            .codec_extradata(H264_EXTRADATA)
                            .build(),
                    );
                }
                let input = presentation(tracks);
                let plans: Vec<_> = input
                    .tracks()
                    .iter()
                    .map(|track| {
                        PlanBuilder::new(track.id.0, clock, nz::u64!(98_304))
                            .part(nz::u32!(1), nz::u64!(9_600))
                            .build()
                    })
                    .collect();
                let plan = SegmentationPlan::new(&input, plans.clone())?;
                let events = discarded_events();
                let writers = input
                    .tracks()
                    .iter()
                    .zip(plans)
                    .map(|(track, plan)| {
                        build_track(
                            PackagingRenditionId(track.id.0),
                            track,
                            plan,
                            Duration::ZERO,
                            events.clone(),
                            crate::domain::PipelineBudget::unlimited(),
                        )
                        .map(|(_, writer)| writer)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let mut mux = Coordinator::new(writers, &input, &plan, events)?;
                let mut out = Vec::new();
                for index in 0..288_i64 {
                    if with_video && index % 4 == 0 {
                        // The payload is opaque to timing; reuse the valid H.264 fixture access unit.
                        let mut sample = video_sample(index * 1024, 4096, index % 96 == 0, 1);
                        if let NormalizedMedia::Video(video) = &mut sample {
                            video.track_id = TrackId(1);
                        }
                        mux.push(sample, &mut out)?;
                    }
                    if index == first_missing {
                        mux.push(
                            NormalizedMedia::Gap(MissingInterval {
                                track_id: TrackId(0),
                                media_kind: MediaKind::Audio,
                                start: index * 1024,
                                end: (index + missing_frames) * 1024,
                                timebase: clock,
                            }),
                            &mut out,
                        )?;
                    }
                    if !(first_missing..first_missing + missing_frames).contains(&index) {
                        mux.push(audio_sample(0, index * 1024, 1024), &mut out)?;
                    }
                }
                mux.finish(FinishReason::Final, &mut out)?;
                assert_gap_coverage(&out, missing_frames)?;
            }
        }
        Ok(())
    }
}
