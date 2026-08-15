//! Track-local WebVTT segmentation.
//!
//! Reading the cues themselves belongs to [`cue`], which keeps every accepted
//! input format's rules in one place; this module only decides where segment
//! boundaries fall and renders the windows between them.

use std::{cmp::Ordering, collections::VecDeque, num::NonZero, sync::Arc};

mod cue;

use cue::{CueAction, CueContent, CueDialect};

use crate::{
    domain::{
        Appender, DiscoveredTrack, MediaInstant, MediaKind, Payload, TickDuration, TickTimestamp,
        Timebase, TimebaseProjection, TrackId, duration_since,
    },
    media::{NormalizedSample, SubtitleSample},
    observe::{EventSink, SessionEvent},
    segment::TrackSegmentationPlan,
};

use super::{
    FinishReason, InitializationSegment, MediaSegmentFormat, MuxError, PackagedChunk,
    PackagedMedia, PackagedRendition, PackagedSegment, PackagedSegmentCompletion,
    PackagingRenditionId, PackagingSegmentId, RenditionConfig, RenditionKey, RenditionMedia,
    TrackPackager,
};

// These cap synchronous work caused by one corrupt cue. They are deliberately
// independent from input byte limits: a tiny payload can still carry a
// duration or timestamp that would materialize billions of segments.
const MAX_CUE_BYTES: usize = 256 * 1024;
const MAX_CUE_SEGMENTS: u64 = 64;
const MAX_EMPTY_SEGMENTS_PER_CUE: u64 = 4_096;
// The heartbeat advances by whatever a sibling's timestamp jumped, so a corrupt
// or wildly out-of-range sample on *another* track lands here.
const MAX_HEARTBEAT_SEGMENTS_PER_TICK: u64 = 4_096;
// A part index is a `u32`, and a segment whose part grid does not fit one is a
// planning error rather than something to discover mid-publication.
const MAX_PARTS_PER_SEGMENT: u64 = 4_096;

/// An unchanged open-ended display state this old is worth reporting.
///
/// This is deliberately not a presentation timeout. FLV text is replacement
/// state, so only the publisher can distinguish a deliberately long caption
/// from a lost clear. The muxer reports the condition once and keeps rendering.
const LONG_LIVED_TEXT_STATE: TickDuration = 30 * 90_000;

const INITIALIZATION: &[u8] = b"WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0\n\n";

#[derive(Clone, Debug)]
struct Cue {
    /// Presentation-relative span, kept alongside the rendered milliseconds so
    /// a part can select the cues overlapping its own interval without
    /// reparsing what was already formatted.
    start: TickTimestamp,
    end: TickTimestamp,
    identifier: Option<Arc<str>>,
    settings: Option<Arc<str>>,
    text: Arc<str>,
}

#[derive(Clone, Debug)]
struct ActiveCue {
    sample: SubtitleSample,
    start: TickTimestamp,
    content: CueContent,
    long_lived_reported: bool,
}

#[derive(Debug)]
struct Window {
    id: u64,
    start: TickTimestamp,
    cues: Vec<Arc<Cue>>,
    /// How many of this window's parts have already been published.
    ///
    /// Always zero when parts are disabled. A published part cannot be revised,
    /// so this is also the boundary a late cue is measured against.
    sealed_parts: u32,
}

/// The part cadence a subtitle rendition publishes on, if any.
///
/// Parts are worth cutting only when they subdivide the segment: an equal
/// target would advertise a part grid carrying exactly one part per segment,
/// which is strictly more playlist for the same media. Decided in one place so
/// the advertised `chunk_target` and the muxer's behaviour cannot disagree.
fn part_target(plan: &TrackSegmentationPlan) -> Option<NonZero<TickDuration>> {
    (plan.part_duration < plan.segment_duration).then_some(plan.part_duration)
}

pub(super) fn build_track(
    rendition_id: PackagingRenditionId,
    track: &DiscoveredTrack,
    plan: TrackSegmentationPlan,
    events: EventSink,
) -> Result<(PackagedRendition, Box<dyn TrackPackager>), MuxError> {
    if track.kind() != MediaKind::Subtitle {
        return Err(invalid("WebVTT output requires a subtitle track"));
    }
    let dialect = CueDialect::for_codec(track.codec).ok_or_else(|| {
        invalid(format!(
            "{} subtitle codec {:?} cannot be converted to WebVTT",
            track.id, track.codec
        ))
    })?;
    if plan.track_id != track.id || plan.timebase != track.timebase {
        return Err(invalid(format!(
            "segmentation plan does not describe {}",
            track.id
        )));
    }
    if plan.timebase != Timebase::hz90k() {
        return Err(invalid(format!(
            "{} WebVTT input is not normalized to the 90 kHz output clock",
            track.id
        )));
    }

    let rendition = packaged_rendition(rendition_id, track, &plan);
    let packager = WebVttTrack::new(rendition_id, track, dialect, plan, events)?;
    Ok((rendition, Box::new(packager)))
}

fn packaged_rendition(
    rendition_id: PackagingRenditionId,
    track: &DiscoveredTrack,
    plan: &TrackSegmentationPlan,
) -> PackagedRendition {
    let fallback_name = format!("Subtitle {}", rendition_id.0 + 1);
    let first_segment_duration = duration_since(
        plan.first_segment_boundary_pts,
        plan.segmentation_origin_pts,
    )
    .and_then(NonZero::new)
    .unwrap_or(plan.segment_duration);
    PackagedRendition {
        packaging_rendition_id: rendition_id,
        key: RenditionKey::for_source(track),
        source_tracks: Arc::from([track.id]),
        config: RenditionConfig {
            timebase: plan.timebase,
            segment_target: plan.segment_duration,
            // Cue windows are cut on the planned grid regardless of cue
            // arrival. The first may be longer when subtitles begin before
            // their recurring cadence.
            maximum_segment_duration: first_segment_duration.max(plan.segment_duration),
            chunk_target: part_target(plan),
            segment_format: MediaSegmentFormat::WebVtt,
        },
        media: RenditionMedia::Subtitle,
        codecs: Arc::from("wvtt"),
        name: Arc::from(track.title.as_deref().unwrap_or(&fallback_name)),
        language: track.language.as_deref().map(Arc::from),
        is_default: false,
        declared_bandwidth: None,
    }
}

struct WebVttTrack {
    rendition_id: PackagingRenditionId,
    track_id: TrackId,
    dialect: CueDialect,
    plan: TrackSegmentationPlan,
    events: EventSink,
    origin: TickTimestamp,
    first_boundary: TickTimestamp,
    segment_ticks: TickDuration,
    /// Part cadence, or `None` when this rendition publishes whole segments.
    part_ticks: Option<TickDuration>,
    windows: VecDeque<Window>,
    next_window_id: u64,
    last_cue_start: Option<TickTimestamp>,
    maximum_cue_end: Option<TickTimestamp>,
    /// The publisher-owned display state for an open-ended text dialect.
    ///
    /// It is rendered provisionally at every seal boundary, then resolved into
    /// canonical intervals when a replacement or clear arrives. Consequently a
    /// part never waits for the future merely to know what is on screen now.
    active: Option<ActiveCue>,
    /// How far the presentation has advanced, as reported by sibling tracks.
    ///
    /// Only ever moves forward: siblings interleave, so a later sample can
    /// carry an earlier instant, and windows that were already emitted cannot
    /// be reopened.
    clock: Option<MediaInstant>,
    initialized: bool,
    finished: bool,
}

impl WebVttTrack {
    fn new(
        rendition_id: PackagingRenditionId,
        track: &DiscoveredTrack,
        dialect: CueDialect,
        plan: TrackSegmentationPlan,
        events: EventSink,
    ) -> Result<Self, MuxError> {
        let origin = plan
            .segmentation_origin_pts
            .checked_sub(plan.presentation_origin_pts)
            .ok_or_else(|| invalid("WebVTT segmentation origin rebasing overflowed"))?;
        let first_boundary = plan
            .first_segment_boundary_pts
            .checked_sub(plan.presentation_origin_pts)
            .ok_or_else(|| invalid("WebVTT first boundary rebasing overflowed"))?;
        if first_boundary <= origin {
            return Err(invalid("WebVTT first segment boundary is invalid"));
        }
        let part_ticks = part_target(&plan).map(NonZero::get);
        if let Some(part_ticks) = part_ticks {
            // The longest window is the first, which may run past the recurring
            // cadence; checking it bounds every later one too.
            let longest = duration_since(first_boundary, origin)
                .unwrap_or(plan.segment_duration.get())
                .max(plan.segment_duration.get());
            if longest.div_ceil(part_ticks) > MAX_PARTS_PER_SEGMENT {
                return Err(invalid(format!(
                    "{} subtitle part cadence divides a segment into more than {MAX_PARTS_PER_SEGMENT} parts",
                    track.id
                )));
            }
        }

        Ok(Self {
            rendition_id,
            track_id: track.id,
            dialect,
            plan,
            events,
            origin,
            first_boundary,
            segment_ticks: plan.segment_duration.get(),
            part_ticks,
            windows: VecDeque::from([Window {
                id: 0,
                start: origin,
                cues: Vec::new(),
                sealed_parts: 0,
            }]),
            next_window_id: 1,
            last_cue_start: None,
            maximum_cue_end: None,
            active: None,
            clock: None,
            initialized: false,
            finished: false,
        })
    }

    /// Places one of this track's own timestamps on the presentation timeline.
    ///
    /// Internal timestamps are already rebased onto the shared presentation
    /// origin, so the origin here is zero. Comparing instants rather than
    /// rescaling a sibling's clock into this track's ticks is what keeps
    /// sealing exact: requantizing would round segment boundaries, which is
    /// precisely what segmentation depends on not happening.
    fn instant(&self, ticks: TickTimestamp) -> MediaInstant {
        MediaInstant::new(self.plan.timebase, ticks, 0)
    }

    /// The presentation instant one window's content runs out at.
    fn window_end(&self, window: &Window) -> Result<MediaInstant, MuxError> {
        let duration = self.window_duration(window.id)?;
        window
            .start
            .checked_add_unsigned(duration)
            .map(|end| self.instant(end))
            .ok_or_else(|| mux_error("WebVTT window end overflowed"))
    }

    fn prepare_cue(&self, sample: &SubtitleSample) -> Result<PreparedCue, MuxError> {
        if sample.track_id != self.track_id {
            return Err(mux_error(format!(
                "{} received a cue for {}",
                self.track_id, sample.track_id
            )));
        }
        if CueDialect::for_codec(sample.codec) != Some(self.dialect) {
            return Err(mux_error(format!(
                "{} changed subtitle codec while muxing",
                self.track_id
            )));
        }
        let retained = sample
            .payload
            .len()
            .checked_add(sample.webvtt.retained_bytes())
            .ok_or_else(|| mux_error("subtitle cue byte accounting overflowed"))?;
        if retained > MAX_CUE_BYTES {
            return Err(mux_error(format!(
                "{} subtitle cue is {retained} bytes, above the {MAX_CUE_BYTES}-byte safety limit",
                self.track_id
            )));
        }
        if self
            .last_cue_start
            .is_some_and(|previous| sample.pts < previous)
        {
            return Err(mux_error(format!(
                "{} supplied decreasing subtitle PTS",
                self.track_id
            )));
        }

        let start = sample
            .pts
            .checked_sub(self.plan.presentation_origin_pts)
            .ok_or_else(|| mux_error("subtitle PTS rebasing overflowed"))?;
        let end = start
            .checked_add_unsigned(sample.duration)
            .ok_or_else(|| mux_error("subtitle cue end overflowed"))?;
        if start < self.origin {
            return Err(mux_error(format!(
                "{} cue starts before its locked segmentation origin",
                self.track_id
            )));
        }

        let (first_index, last_index) = self.cue_segment_span(start, end)?;
        // Verify every required window start before emitting initialization or
        // modifying the queue, keeping a failed push transactional.
        self.window_start(last_index)?;

        Ok(PreparedCue {
            start,
            end,
            first_index,
            last_index,
            cue: Arc::new(self.render_cue(sample, start)?),
        })
    }

    fn cue_segment_span(
        &self,
        start: TickTimestamp,
        end: TickTimestamp,
    ) -> Result<(u64, u64), MuxError> {
        let first_index = self.segment_index(start)?;
        let last_index = self.segment_index(
            end.checked_sub(1)
                .ok_or_else(|| mux_error("subtitle cue has no presentation duration"))?,
        )?;
        last_index
            .checked_add(1)
            .ok_or_else(|| mux_error("WebVTT segment ID overflowed"))?;
        let span = last_index
            .checked_sub(first_index)
            .and_then(|distance| distance.checked_add(1))
            .ok_or_else(|| mux_error("subtitle cue segment span overflowed"))?;
        if span > MAX_CUE_SEGMENTS {
            return Err(mux_error(format!(
                "{} subtitle cue overlaps {span} segments, above the {MAX_CUE_SEGMENTS}-segment safety limit",
                self.track_id
            )));
        }
        let current = self
            .windows
            .front()
            .expect("unfinished WebVTT muxer always has a current window")
            .id;
        let empty_advance = first_index.saturating_sub(current);
        if empty_advance > MAX_EMPTY_SEGMENTS_PER_CUE {
            return Err(mux_error(format!(
                "{} subtitle cue would advance {empty_advance} segments, above the {MAX_EMPTY_SEGMENTS_PER_CUE}-segment safety limit",
                self.track_id
            )));
        }
        Ok((first_index, last_index))
    }

    fn render_cue(&self, sample: &SubtitleSample, start: TickTimestamp) -> Result<Cue, MuxError> {
        let CueAction::Show(content) = self.dialect.read(sample)? else {
            // A clear never reaches rendering: `push` consumes it to end the
            // held cue and holds nothing in its place, so there is no cue to
            // render. Reaching here would mean that ordering was broken.
            return Err(mux_error(format!(
                "{} attempted to render a clear as a cue",
                self.track_id
            )));
        };
        let rendered_bytes = content
            .text
            .len()
            .checked_add(content.metadata.retained_bytes())
            .ok_or_else(|| mux_error("rendered subtitle cue byte accounting overflowed"))?;
        if rendered_bytes > MAX_CUE_BYTES {
            return Err(mux_error(format!(
                "{} rendered subtitle cue is {rendered_bytes} bytes, above the {MAX_CUE_BYTES}-byte safety limit",
                self.track_id
            )));
        }
        Ok(Cue {
            start,
            end: start
                .checked_add_unsigned(sample.duration)
                .ok_or_else(|| mux_error("subtitle cue end overflowed while rendering"))?,
            identifier: content.metadata.identifier,
            settings: content.metadata.settings,
            text: content.text,
        })
    }

    fn segment_index(&self, timestamp: TickTimestamp) -> Result<u64, MuxError> {
        if timestamp < self.first_boundary {
            return Ok(0);
        }
        let offset = timestamp
            .checked_sub(self.first_boundary)
            .and_then(|offset| u64::try_from(offset).ok())
            .ok_or_else(|| mux_error("subtitle timestamp precedes segmentation origin"))?;
        (offset / self.segment_ticks)
            .checked_add(1)
            .ok_or_else(|| mux_error("WebVTT segment ID overflowed"))
    }

    fn window_start(&self, id: u64) -> Result<TickTimestamp, MuxError> {
        if id == 0 {
            return Ok(self.origin);
        }
        let offset = u128::from(id - 1)
            .checked_mul(u128::from(self.segment_ticks))
            .and_then(|offset| i128::try_from(offset).ok())
            .ok_or_else(|| mux_error("WebVTT segment offset overflowed"))?;
        i128::from(self.first_boundary)
            .checked_add(offset)
            .and_then(|start| TickTimestamp::try_from(start).ok())
            .ok_or_else(|| mux_error("WebVTT segment start overflowed"))
    }

    fn window_duration(&self, id: u64) -> Result<TickDuration, MuxError> {
        if id == 0 {
            return duration_since(self.first_boundary, self.origin)
                .ok_or_else(|| mux_error("WebVTT first window duration is invalid"));
        }
        Ok(self.segment_ticks)
    }

    fn ensure_through(&mut self, id: u64) -> Result<(), MuxError> {
        while self.windows.back().is_none_or(|window| window.id < id) {
            let next = self.next_window_id;
            self.windows.push_back(Window {
                id: next,
                start: self.window_start(next)?,
                cues: Vec::new(),
                sealed_parts: 0,
            });
            self.next_window_id = next
                .checked_add(1)
                .ok_or_else(|| mux_error("WebVTT segment ID overflowed"))?;
        }
        Ok(())
    }

    /// Emits every window whose content is entirely behind `now`.
    ///
    /// Only pops from the front, so callers must have materialized a window
    /// that outlives `now` first — otherwise this drains the queue and strands
    /// the invariant that an unfinished muxer always has a current window.
    fn seal_before(&mut self, now: MediaInstant, out: &mut dyn Appender<PackagedMedia>) {
        if self.part_ticks.is_some() {
            self.seal_parts_before(now, out);
            return;
        }
        while self.windows.front().is_some_and(|window| {
            self.window_end(window)
                .ok()
                .and_then(|end| end.compare(now))
                .is_some_and(|ordering| ordering != Ordering::Greater)
        }) {
            let window = self.windows.pop_front().expect("front was inspected above");
            let duration = self
                .window_duration(window.id)
                .expect("queued WebVTT windows have valid timing");
            self.emit(&window, duration, out);
        }
    }

    /// As [`Self::seal_before`], one part at a time.
    ///
    /// Works on an owned window so a part can be published and recorded in the
    /// same step; a window with parts still open goes back on the front. The
    /// queue cannot drain here for the same reason it cannot in whole-segment
    /// mode: `ensure_current_at` has already materialized a window whose
    /// content outlives `now`, and its final part therefore never seals.
    fn seal_parts_before(&mut self, now: MediaInstant, out: &mut dyn Appender<PackagedMedia>) {
        while let Some(mut window) = self.windows.pop_front() {
            let Ok(total) = self.window_duration(window.id) else {
                self.windows.push_front(window);
                return;
            };
            let count = self.part_count(total);
            while window.sealed_parts < count {
                let Some((start, duration)) = self.part_span(&window, total, window.sealed_parts)
                else {
                    break;
                };
                let end = start.saturating_add_unsigned(duration);
                if self
                    .instant(end)
                    .compare(now)
                    .is_none_or(|ordering| ordering == Ordering::Greater)
                {
                    break;
                }
                self.emit_part(&window, window.sealed_parts, start, duration, out);
                window.sealed_parts += 1;
            }
            if window.sealed_parts < count {
                self.windows.push_front(window);
                return;
            }
            self.complete(&window, total, out);
        }
    }

    /// Publishes everything left of `window` up to `total`, then closes it.
    ///
    /// Used only on the drain path, where there is no clock left to wait for.
    /// Remaining parts still follow the grid so none can exceed `PART-TARGET`;
    /// only the last is short, which HLS allows.
    fn seal_remaining_parts(
        &self,
        window: &mut Window,
        total: TickDuration,
        out: &mut dyn Appender<PackagedMedia>,
    ) {
        let count = self.part_count(total);
        while window.sealed_parts < count {
            let Some((start, duration)) = self.part_span(window, total, window.sealed_parts) else {
                break;
            };
            self.emit_part(window, window.sealed_parts, start, duration, out);
            window.sealed_parts += 1;
        }
        self.complete(window, total, out);
    }

    /// Materializes windows until one of them still has content ahead of `now`.
    ///
    /// Expressed as a walk rather than an index computed from `now` because the
    /// two are not interchangeable: deriving an index would mean rescaling a
    /// sibling's clock into this track's ticks, and a rounding error that
    /// created one window too few would let [`Self::seal_before`] empty the
    /// queue.
    fn ensure_current_at(&mut self, now: MediaInstant) -> Result<(), MuxError> {
        let mut created = 0u64;
        while self
            .windows
            .back()
            .map(|window| self.window_end(window))
            .transpose()?
            .is_none_or(|end| {
                end.compare(now)
                    .is_some_and(|ordering| ordering != Ordering::Greater)
            })
        {
            if created >= MAX_HEARTBEAT_SEGMENTS_PER_TICK {
                return Err(mux_error(format!(
                    "{} presentation clock advanced past {MAX_HEARTBEAT_SEGMENTS_PER_TICK} subtitle segments in one step",
                    self.track_id
                )));
            }
            let next = self.next_window_id;
            self.windows.push_back(Window {
                id: next,
                start: self.window_start(next)?,
                cues: Vec::new(),
                sealed_parts: 0,
            });
            self.next_window_id = next
                .checked_add(1)
                .ok_or_else(|| mux_error("WebVTT segment ID overflowed"))?;
            created += 1;
        }
        Ok(())
    }

    fn emit(&self, window: &Window, duration: TickDuration, out: &mut dyn Appender<PackagedMedia>) {
        let end = window.start.saturating_add_unsigned(duration);
        out.push(PackagedMedia::Segment(PackagedSegment {
            rendition_id: self.rendition_id,
            packaging_segment_id: PackagingSegmentId(window.id),
            media_start: window.start,
            duration,
            independent: true,
            payload: Payload::from(self.render_range(
                window,
                window.start,
                end,
                self.dialect == CueDialect::Text,
            )),
        }));
    }

    /// Publishes one part, carrying every cue on screen during its interval.
    ///
    /// That overlap rule is what makes `independent` honest: a client joining
    /// here is handed the captions it should already be displaying, not only
    /// the ones that happen to begin inside this part. The cost is that a cue
    /// spanning several parts is repeated in each, so the parent segment —
    /// which delivery serves as the concatenation of its parts — carries the
    /// same repetition players already absorb across overlapping segments.
    fn emit_part(
        &self,
        window: &Window,
        index: u32,
        start: TickTimestamp,
        duration: TickDuration,
        out: &mut dyn Appender<PackagedMedia>,
    ) {
        let end = start.saturating_add_unsigned(duration);
        out.push(PackagedMedia::Chunk(PackagedChunk {
            rendition_id: self.rendition_id,
            packaging_segment_id: PackagingSegmentId(window.id),
            chunk_index: index,
            media_start: start,
            duration,
            independent: true,
            payload: Payload::from(self.render_range(window, start, end, true)),
        }));
    }

    fn complete(
        &self,
        window: &Window,
        duration: TickDuration,
        out: &mut dyn Appender<PackagedMedia>,
    ) {
        let end = window.start.saturating_add_unsigned(duration);
        out.push(PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
            rendition_id: self.rendition_id,
            packaging_segment_id: PackagingSegmentId(window.id),
            media_start: window.start,
            duration,
            payload: Some(Payload::from(self.render_range(
                window,
                window.start,
                end,
                self.dialect == CueDialect::Text,
            ))),
        }));
    }

    /// Renders the display state intersecting `[start, end)` as independent
    /// WebVTT cue slices.
    ///
    /// Clipping every interval to the resource boundary makes adjacent parts
    /// tile exactly. In particular, an unresolved active state can be emitted
    /// now without inventing a future end that a later clear could not retract.
    fn render_range(
        &self,
        window: &Window,
        start: TickTimestamp,
        end: TickTimestamp,
        clip_resolved: bool,
    ) -> Vec<u8> {
        let milliseconds = TimebaseProjection::new(
            self.plan.timebase,
            Timebase::new(nz::u32!(1), nz::u32!(1_000)),
        );
        let mut body = String::new();
        for cue in window
            .cues
            .iter()
            .filter(|cue| cue.start < end && cue.end > start)
        {
            render_cue_slice(
                &mut body,
                milliseconds,
                if clip_resolved {
                    cue.start.max(start)
                } else {
                    cue.start
                },
                if clip_resolved {
                    cue.end.min(end)
                } else {
                    cue.end
                },
                cue.identifier.as_deref(),
                cue.settings.as_deref(),
                &cue.text,
            );
        }
        if let Some(active) = &self.active
            && active.start < end
        {
            render_cue_slice(
                &mut body,
                milliseconds,
                active.start.max(start),
                end,
                active.content.metadata.identifier.as_deref(),
                active.content.metadata.settings.as_deref(),
                &active.content.text,
            );
        }
        body.into_bytes()
    }

    /// Where one part of `window` begins and how long it runs.
    ///
    /// The last part of a window takes whatever remains, which HLS exempts from
    /// the `PART-TARGET` floor precisely so a grid need not divide evenly.
    fn part_span(
        &self,
        window: &Window,
        total: TickDuration,
        index: u32,
    ) -> Option<(TickTimestamp, TickDuration)> {
        let part_ticks = self.part_ticks?;
        let offset = u64::from(index).checked_mul(part_ticks)?;
        let duration = total.checked_sub(offset).filter(|left| *left > 0)?;
        Some((
            window.start.checked_add_unsigned(offset)?,
            duration.min(part_ticks),
        ))
    }

    fn part_count(&self, total: TickDuration) -> u32 {
        self.part_ticks
            .map(|ticks| total.div_ceil(ticks))
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or(0)
    }

    fn initialize(&mut self, out: &mut dyn Appender<PackagedMedia>) {
        if self.initialized {
            return;
        }
        out.push(PackagedMedia::Initialization(InitializationSegment {
            rendition_id: self.rendition_id,
            version: 0,
            payload: Payload::from(INITIALIZATION),
        }));
        self.initialized = true;
    }

    /// Places one cue whose span is known into every window that carries it.
    ///
    /// Separate from [`TrackPackager::push`] because an open-ended cue is
    /// placed whenever its end becomes known, which is rarely the moment it
    /// arrived.
    fn place_cue(
        &mut self,
        sample: &SubtitleSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        let prepared = self.prepare_cue(sample)?;

        self.initialize(out);
        self.ensure_through(prepared.last_index)?;
        self.seal_before(self.instant(prepared.start), out);
        let mut placed = false;
        for window in &mut self.windows {
            if window.id < prepared.first_index || window.id > prepared.last_index {
                continue;
            }
            // Storing the cue is not the same as publishing it: parts render by
            // overlap at seal time, so a cue whose whole span lies in parts that
            // already went out reaches nobody, even though its window is open.
            let revisable = self
                .part_ticks
                .and_then(|ticks| u64::from(window.sealed_parts).checked_mul(ticks))
                .and_then(|offset| window.start.checked_add_unsigned(offset))
                .unwrap_or(window.start);
            window.cues.push(Arc::clone(&prepared.cue));
            placed |= prepared.end > revisable;
        }
        // The heartbeat seals on the presentation clock, so a cue can arrive
        // after every window that could have carried it was already published.
        // Dropping it is the only option left — the segments are out — but a
        // publisher whose subtitles consistently run late should be visible.
        if !placed {
            self.events.emit(SessionEvent::SubtitleCueTooLate {
                track: self.track_id,
                late_by: self
                    .clock
                    .and_then(|now| now.elapsed_since(self.instant(prepared.end)))
                    .unwrap_or_default(),
            });
        }
        self.last_cue_start = Some(prepared.start);
        self.maximum_cue_end = Some(
            self.maximum_cue_end
                .map_or(prepared.end, |end| end.max(prepared.end)),
        );
        Ok(())
    }

    /// Resolves the active replacement state at a presentation-relative end.
    fn close_active_at(
        &mut self,
        end: TickTimestamp,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        let Some(duration) = end
            .checked_sub(active.start)
            .and_then(|span| TickDuration::try_from(span).ok().filter(|span| *span > 0))
        else {
            return Ok(());
        };
        let mut sample = active.sample;
        sample.duration = duration;
        // Identical replacement messages advance input ordering without
        // changing the active state's original start. Resolving that older
        // start is not a timestamp rewind, so validate it against itself and
        // restore the latest observed state-change position afterwards.
        let latest_update = self.last_cue_start;
        self.last_cue_start = Some(sample.pts);
        let result = self.place_cue(&sample, out);
        self.last_cue_start = latest_update.or(self.last_cue_start);
        result
    }

    fn report_long_lived_state(&mut self, now: MediaInstant) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.long_lived_reported {
            return;
        }
        let start = active.start;
        let Some(age) = now.elapsed_since(self.instant(start)) else {
            return;
        };
        if age < self.plan.timebase.ticks_to_duration(LONG_LIVED_TEXT_STATE) {
            return;
        }
        if let Some(active) = self.active.as_mut() {
            active.long_lived_reported = true;
        }
        self.events.emit(SessionEvent::SubtitleStateLongLived {
            track: self.track_id,
            started_at: self
                .plan
                .timebase
                .ticks_to_duration(u64::try_from(start.max(0)).unwrap_or(u64::MAX)),
            age,
        });
    }

    /// Report a replacement-state transition whose intended start is already
    /// behind the first byte range that can still be changed. The state still
    /// applies to future parts; already published parts are never revised.
    fn report_late_transition(&self, start: TickTimestamp) {
        let Some(window) = self.windows.front() else {
            return;
        };
        let revisable = self
            .part_ticks
            .and_then(|ticks| u64::from(window.sealed_parts).checked_mul(ticks))
            .and_then(|offset| window.start.checked_add_unsigned(offset))
            .unwrap_or(window.start);
        if start >= revisable {
            return;
        }
        self.events.emit(SessionEvent::SubtitleCueTooLate {
            track: self.track_id,
            late_by: self
                .clock
                .and_then(|now| now.elapsed_since(self.instant(start)))
                .unwrap_or_default(),
        });
    }
}

struct PreparedCue {
    start: TickTimestamp,
    end: TickTimestamp,
    first_index: u64,
    last_index: u64,
    cue: Arc<Cue>,
}

impl TrackPackager for WebVttTrack {
    fn track_id(&self) -> TrackId {
        self.track_id
    }

    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.finished {
            return Err(mux_error("cannot push WebVTT media after finish"));
        }
        let NormalizedSample::Subtitle(sample) = sample else {
            return Err(mux_error(format!(
                "{} received a non-subtitle sample",
                self.track_id
            )));
        };
        // Held only when the cue actually lacks an end. A codec that may omit
        // one can still supply it, and a supplied span is the publisher's
        // statement about its own cue: overriding it with a successor's start
        // would discard information this node does not have a better source
        // for. Keyed on the sample rather than the dialect so the muxer and the
        // normalizer cannot disagree about which cues are open-ended.
        if sample.codec.is_open_ended() && sample.duration == 0 {
            // Checked before anything is resolved. A held cue is the immediate
            // predecessor and has not reached `last_cue_start` yet, so leaving
            // it out would let a rewound cue end its predecessor in the past —
            // which resolves to an empty span and silently drops it instead of
            // failing the publisher that rewound.
            let previous = self
                .active
                .as_ref()
                .map(|active| active.sample.pts)
                .into_iter()
                .chain(self.last_cue_start)
                .max();
            if previous.is_some_and(|previous| sample.pts < previous) {
                return Err(mux_error(format!(
                    "{} supplied decreasing subtitle PTS",
                    self.track_id
                )));
            }
            // Validated before it is held, so a malformed cue fails on the push
            // that delivered it rather than at whatever unrelated moment later
            // resolves it.
            let action = self.dialect.read(&sample)?;
            // Timing is checked here for the same reason. `prepare_cue` runs
            // only when the cue is placed, so without this a cue starting
            // before the locked origin would fail on an unrelated later push —
            // or, at finish, with nothing left to attribute it to.
            if sample
                .pts
                .checked_sub(self.plan.presentation_origin_pts)
                .is_none_or(|start| start < self.origin)
            {
                return Err(mux_error(format!(
                    "{} cue starts before its locked segmentation origin",
                    self.track_id
                )));
            }
            let start = sample
                .pts
                .checked_sub(self.plan.presentation_origin_pts)
                .ok_or_else(|| mux_error("subtitle PTS rebasing overflowed"))?;
            if matches!(action, CueAction::Clear) {
                self.close_active_at(start, out)?;
                // The clear has done its whole job by ending the held cue.
                // Holding nothing in its place is what makes the display go
                // empty, and it is why a clear publishes no cue of its own.
                //
                // `last_cue_start` is advanced so a later cue is still checked
                // against the clear's position: the clear is a point on this
                // track's timeline, and a cue arriving before it would be out
                // of order even though nothing was displayed.
                self.last_cue_start = Some(sample.pts);
                return Ok(());
            }
            let CueAction::Show(content) = action else {
                unreachable!("clear was handled above")
            };
            self.report_late_transition(start);
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.content == content)
            {
                // Replacement state did not change. Keeping the original start
                // coalesces publisher restatements and, importantly, does not
                // let them hide a missing clear from the long-lived warning.
                self.last_cue_start = Some(sample.pts);
                return Ok(());
            }
            self.close_active_at(start, out)?;
            self.last_cue_start = Some(sample.pts);
            self.active = Some(ActiveCue {
                sample,
                start,
                content,
                long_lived_reported: false,
            });
            return Ok(());
        }
        self.place_cue(&sample, out)
    }

    fn tick(
        &mut self,
        now: MediaInstant,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.finished {
            return Ok(());
        }
        if self
            .clock
            .is_some_and(|previous| now.compare(previous) != Some(Ordering::Greater))
        {
            return Ok(());
        }
        self.clock = Some(now);

        self.ensure_current_at(now)?;
        // Before any sealing, so the header precedes the first heartbeat
        // segment. This is also what makes the rendition servable from the
        // start of the presentation rather than from its first cue.
        self.initialize(out);
        self.report_long_lived_state(now);
        self.seal_before(now, out);
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
        // The presentation end is the only honest end for an active replacement
        // state. A timeout would override the publisher; the latest sibling
        // clock instead closes exactly where the media itself stopped.
        if reason != FinishReason::Superseded
            && let (Some(active), Some(clock)) = (self.active.as_ref(), self.clock)
            && let Some(elapsed) = clock.elapsed_since(self.instant(active.start))
        {
            let end = active
                .start
                .saturating_add_unsigned(self.plan.timebase.duration_to_ticks(elapsed));
            self.close_active_at(end, out)?;
        }
        self.active = None;
        self.finished = true;
        if reason == FinishReason::Superseded {
            self.windows.clear();
            return Ok(());
        }
        let Some(end) = self.maximum_cue_end else {
            self.windows.clear();
            return Ok(());
        };

        while let Some(mut window) = self.windows.pop_front() {
            let total = self
                .window_duration(window.id)
                .expect("queued WebVTT windows have valid timing");
            // Cue content reaching into this window, if any. The heartbeat
            // opens windows on the presentation clock, so a sparse track can
            // leave one sitting entirely past the last cue. That is ordinary,
            // not an error: such a window simply has no cued extent.
            let cued = end
                .checked_sub(window.start)
                .and_then(|duration| u64::try_from(duration).ok())
                .map_or(0, |duration| duration.min(total));
            // Parts already published put a floor under the segment: a
            // completion shorter than the media it closes would describe a
            // segment delivery has already served more of.
            let sealed = window
                .sealed_parts
                .checked_sub(1)
                .and_then(|last| self.part_span(&window, total, last))
                .map_or(0, |(start, duration)| {
                    duration_since(start, window.start).unwrap_or(0) + duration
                });
            // Whichever reaches further wins: published parts are already out
            // and must be closed over, while a window with neither cue content
            // nor published parts has nothing to invent media for.
            let duration = cued.max(sealed);
            if duration == 0 {
                break;
            }
            if self.part_ticks.is_some() {
                self.seal_remaining_parts(&mut window, duration, out);
            } else {
                self.emit(&window, duration, out);
            }
        }
        self.windows.clear();
        Ok(())
    }
}

fn render_cue_slice(
    body: &mut String,
    milliseconds: TimebaseProjection,
    start: TickTimestamp,
    end: TickTimestamp,
    identifier: Option<&str>,
    settings: Option<&str>,
    text: &str,
) {
    let Some(start_ms) = milliseconds
        .timestamp(start)
        .and_then(|value| u64::try_from(value).ok())
    else {
        return;
    };
    let Some(end_ms) = milliseconds
        .timestamp(end)
        .and_then(|value| u64::try_from(value).ok())
    else {
        return;
    };
    if end_ms <= start_ms {
        return;
    }
    if let Some(identifier) = identifier
        && !identifier.is_empty()
    {
        body.push_str(identifier);
        body.push('\n');
    }
    body.push_str(&format_timestamp(start_ms));
    body.push_str(" --> ");
    body.push_str(&format_timestamp(end_ms));
    if let Some(settings) = settings
        && !settings.is_empty()
    {
        body.push(' ');
        body.push_str(settings);
    }
    body.push('\n');
    body.push_str(text);
    body.push_str("\n\n");
}

fn format_timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds % 3_600_000) / 60_000;
    let seconds = (milliseconds % 60_000) / 1_000;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

fn invalid(message: impl Into<Box<str>>) -> MuxError {
    MuxError::InvalidPlan(message.into())
}

fn mux_error(message: impl Into<Box<str>>) -> MuxError {
    MuxError::Mux(message.into())
}

#[cfg(test)]
mod tests {
    use std::str;

    use super::*;
    use crate::{
        domain::{Codec, MediaKind, SubtitlePosition, WebVttCueMetadata, fixtures::TrackBuilder},
        media::SubtitleSample,
        mux::fixtures::{RecordedEvents, discarded_events},
        segment::fixtures::PlanBuilder,
    };

    const SECOND: u64 = 90_000;

    fn track(codec: Codec) -> DiscoveredTrack {
        TrackBuilder::new(0, MediaKind::Subtitle)
            .codec(codec)
            .timebase(Timebase::hz90k())
            .build()
    }

    /// Whole-segment publication: a part target equal to the segment means
    /// there is no grid to subdivide, which is what `part_target` refuses.
    fn plan(segment_seconds: u64) -> TrackSegmentationPlan {
        plan_with_parts(segment_seconds, segment_seconds)
    }

    fn plan_with_parts(segment_seconds: u64, part_seconds: u64) -> TrackSegmentationPlan {
        let duration =
            std::num::NonZero::new(segment_seconds * SECOND).expect("fixture segments are nonzero");
        let part =
            std::num::NonZero::new(part_seconds * SECOND).expect("fixture parts are nonzero");
        PlanBuilder::new(0, Timebase::hz90k(), duration)
            .part(nz::u32!(1), part)
            .build()
    }

    fn sample(codec: Codec, start: i64, duration: u64, text: &[u8]) -> NormalizedSample {
        NormalizedSample::Subtitle(SubtitleSample {
            track_id: TrackId(0),
            codec,
            pts: start,
            duration,
            webvtt: WebVttCueMetadata::default(),
            position: None,
            payload: Payload::from(text.to_vec()),
        })
    }

    fn mux(codec: Codec, segment_seconds: u64) -> WebVttTrack {
        mux_with_events(codec, segment_seconds, discarded_events())
    }

    fn mux_with_events(codec: Codec, segment_seconds: u64, events: EventSink) -> WebVttTrack {
        let track = track(codec);
        let dialect = CueDialect::for_codec(codec).expect("fixture codecs have a dialect");
        WebVttTrack::new(
            PackagingRenditionId(3),
            &track,
            dialect,
            plan(segment_seconds),
            events,
        )
        .expect("fixture mux starts")
    }

    /// A sibling track's clock, `seconds` past the shared presentation origin.
    ///
    /// Deliberately not the 90 kHz output clock: the heartbeat has to work from
    /// whatever timebase the track that carries it happens to use.
    fn sibling(seconds: u64) -> MediaInstant {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        MediaInstant::new(
            timebase,
            i64::try_from(seconds * 48_000).expect("fixture instants fit i64"),
            0,
        )
    }

    fn mux_with_parts(codec: Codec, segment_seconds: u64, part_seconds: u64) -> WebVttTrack {
        parts_mux(codec, segment_seconds, part_seconds, discarded_events())
    }

    fn parts_mux(
        codec: Codec,
        segment_seconds: u64,
        part_seconds: u64,
        events: EventSink,
    ) -> WebVttTrack {
        let track = track(codec);
        let dialect = CueDialect::for_codec(codec).expect("fixture codecs have a dialect");
        WebVttTrack::new(
            PackagingRenditionId(3),
            &track,
            dialect,
            plan_with_parts(segment_seconds, part_seconds),
            events,
        )
        .expect("fixture mux starts")
    }

    fn parts(output: &[PackagedMedia]) -> Vec<&PackagedChunk> {
        output
            .iter()
            .filter_map(|media| match media {
                PackagedMedia::Chunk(chunk) => Some(chunk),
                _ => None,
            })
            .collect()
    }

    fn completions(output: &[PackagedMedia]) -> Vec<&PackagedSegmentCompletion> {
        output
            .iter()
            .filter_map(|media| match media {
                PackagedMedia::SegmentCompleted(completion) => Some(completion),
                _ => None,
            })
            .collect()
    }

    fn segments(output: &[PackagedMedia]) -> Vec<&PackagedSegment> {
        output
            .iter()
            .filter_map(|media| match media {
                PackagedMedia::Segment(segment) => Some(segment),
                _ => None,
            })
            .collect()
    }

    async fn round_trip(
        codec: Codec,
        text: &[u8],
        metadata: WebVttCueMetadata,
    ) -> crate::source::Packet {
        use std::{io::Cursor, time::Duration};

        use crate::{
            observe::{ProcessMeters, SessionMeters},
            source::{
                DiscoveryLimits, InputLimits, InputState, PacketSource,
                avformat::{AvformatConfig, AvformatPacketSource, ReadInput},
            },
        };

        let mut mux = mux(codec, 2);
        let mut output = Vec::new();
        let NormalizedSample::Subtitle(cue) = sample(codec, 0, SECOND, text) else {
            unreachable!()
        };
        let mut cue = cue;
        cue.webvtt = metadata;
        mux.push(NormalizedSample::Subtitle(cue), &mut output)
            .expect("cue is accepted");
        mux.finish(FinishReason::Final, &mut output)
            .expect("tail flushes");

        let mut bytes = Vec::new();
        for media in &output {
            match media {
                PackagedMedia::Initialization(initialization) => {
                    bytes.extend_from_slice(initialization.payload.as_bytes());
                }
                PackagedMedia::Segment(segment) => {
                    bytes.extend_from_slice(segment.payload.as_bytes());
                }
                _ => unreachable!("WebVTT emits only initialization and direct segments"),
            }
        }

        let meters = SessionMeters::new(ProcessMeters::default());
        let mut source = AvformatPacketSource::new(
            Box::new(ReadInput::closed(Cursor::new(bytes))),
            AvformatConfig::default(),
            InputLimits::permissive(),
            meters.source_view(),
        )
        .expect("round-trip source opens");
        source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 64 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect("rendered WebVTT is discovered");
        let mut packets = Vec::new();
        loop {
            let state = source.fill(&mut packets).await.expect("output demuxes");
            if state != InputState::Open {
                break;
            }
        }
        assert_eq!(packets.len(), 1);
        packets.pop().expect("one packet was recovered")
    }

    #[test]
    fn timestamp_formatting_does_not_wrap_after_one_hour() {
        assert_eq!(format_timestamp(3_723_004), "01:02:03.004");
    }

    #[test]
    fn initialization_precedes_segmented_webvtt_with_full_cue_timestamps()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        let NormalizedSample::Subtitle(first) = sample(Codec::WebVtt, 0, 3 * SECOND, b"first")
        else {
            unreachable!()
        };
        let mut first = first;
        first.webvtt = WebVttCueMetadata {
            identifier: Some(Arc::from("cue-one")),
            settings: Some(Arc::from("align:start")),
        };
        mux.push(NormalizedSample::Subtitle(first), &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        assert!(matches!(
            &output[0],
            PackagedMedia::Initialization(initialization)
                if initialization.version == 0
                    && initialization.payload.as_bytes() == INITIALIZATION
        ));
        let segments = segments(&output);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].packaging_segment_id, PackagingSegmentId(0));
        assert_eq!(segments[0].media_start, 0);
        assert_eq!(segments[0].duration, 2 * SECOND);
        assert_eq!(segments[1].duration, SECOND);
        for segment in segments {
            let body = str::from_utf8(segment.payload.as_bytes())?;
            assert!(body.contains("cue-one\n00:00:00.000 --> 00:00:03.000 align:start"));
        }
        Ok(())
    }

    #[test]
    fn long_cue_windows_stay_open_for_later_overlapping_cues()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        mux.push(sample(Codec::WebVtt, 0, 6 * SECOND, b"long"), &mut output)?;
        mux.push(
            sample(
                Codec::WebVtt,
                3 * i64::try_from(SECOND).expect("second fits i64"),
                SECOND,
                b"overlap",
            ),
            &mut output,
        )?;
        mux.finish(FinishReason::Final, &mut output)?;

        let segments = segments(&output);
        assert_eq!(segments.len(), 3);
        let middle = str::from_utf8(segments[1].payload.as_bytes())?;
        assert!(middle.contains("long"));
        assert!(middle.contains("overlap"));
        let last = str::from_utf8(segments[2].payload.as_bytes())?;
        assert!(last.contains("long"));
        assert!(!last.contains("overlap"));
        Ok(())
    }

    #[test]
    fn sparse_cues_emit_empty_windows_without_inventing_cue_text()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        mux.push(sample(Codec::WebVtt, 0, SECOND, b"first"), &mut output)?;
        mux.push(
            sample(
                Codec::WebVtt,
                6 * i64::try_from(SECOND).expect("second fits i64"),
                SECOND,
                b"later",
            ),
            &mut output,
        )?;
        mux.finish(FinishReason::Interrupted, &mut output)?;

        let segments = segments(&output);
        assert_eq!(segments.len(), 4);
        assert!(segments[1].payload.is_empty());
        assert!(segments[2].payload.is_empty());
        assert_eq!(
            segments[3].media_start,
            6 * i64::try_from(SECOND).expect("second fits i64")
        );
        Ok(())
    }

    #[test]
    fn the_presentation_clock_alone_keeps_a_silent_track_on_cadence()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        // No cue is ever pushed: this is the sparse case that used to leave the
        // playlist empty until someone spoke.
        for second in 1..=7 {
            mux.tick(sibling(second), &mut output)?;
        }

        assert!(matches!(
            &output[0],
            PackagedMedia::Initialization(initialization)
                if initialization.payload.as_bytes() == INITIALIZATION
        ));
        let segments = segments(&output);
        assert_eq!(segments.len(), 3);
        for (index, segment) in segments.iter().enumerate() {
            assert!(segment.payload.is_empty());
            assert_eq!(segment.duration, 2 * SECOND);
            assert_eq!(
                segment.media_start,
                i64::try_from(index as u64 * 2 * SECOND)?,
                "heartbeat segments stay on the planned grid"
            );
        }
        Ok(())
    }

    #[test]
    fn one_large_clock_jump_emits_every_window_it_crossed() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        mux.tick(sibling(7), &mut output)?;

        let segments = segments(&output);
        assert_eq!(segments.len(), 3);
        assert_eq!(
            segments
                .iter()
                .map(|segment| segment.packaging_segment_id)
                .collect::<Vec<_>>(),
            vec![
                PackagingSegmentId(0),
                PackagingSegmentId(1),
                PackagingSegmentId(2)
            ],
            "no window may be skipped, or the timeline would gap"
        );
        // The queue must still hold the window covering the clock, or the next
        // cue would find no current window at all.
        mux.push(
            sample(
                Codec::WebVtt,
                6 * i64::try_from(SECOND)?,
                SECOND,
                b"after the jump",
            ),
            &mut output,
        )?;
        Ok(())
    }

    #[test]
    fn a_clock_that_goes_backwards_seals_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        mux.tick(sibling(5), &mut output)?;
        let sealed = segments(&output).len();
        // Siblings interleave, so an earlier instant arriving later is ordinary.
        mux.tick(sibling(3), &mut output)?;

        assert_eq!(segments(&output).len(), sealed);
        Ok(())
    }

    #[test]
    fn a_cue_the_heartbeat_already_sealed_past_is_reported_and_dropped()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sink, recorder) = RecordedEvents::sink();
        let mut mux = mux_with_events(Codec::WebVtt, 2, sink);
        let mut output = Vec::new();
        mux.tick(sibling(7), &mut output)?;
        let sealed = segments(&output).len();

        mux.push(
            sample(Codec::WebVtt, 0, SECOND, b"far too late"),
            &mut output,
        )?;
        mux.finish(FinishReason::Final, &mut output)?;

        assert!(matches!(
            recorder.events().as_slice(),
            [SessionEvent::SubtitleCueTooLate {
                track: TrackId(0),
                ..
            }]
        ));
        for segment in segments(&output).iter().take(sealed) {
            assert!(
                segment.payload.is_empty(),
                "a published segment cannot be revised to carry a late cue"
            );
        }
        Ok(())
    }

    #[test]
    fn a_cue_straddling_the_sealed_edge_still_reaches_its_open_windows()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sink, recorder) = RecordedEvents::sink();
        let mut mux = mux_with_events(Codec::WebVtt, 2, sink);
        let mut output = Vec::new();
        // Seals windows 0 and 1, leaving window 2 (4s..6s) open.
        mux.tick(sibling(5), &mut output)?;
        // Spans 3s..7s: its first window is gone, its later ones are not.
        mux.push(
            sample(
                Codec::WebVtt,
                3 * i64::try_from(SECOND)?,
                4 * SECOND,
                b"straddles",
            ),
            &mut output,
        )?;
        mux.finish(FinishReason::Final, &mut output)?;

        assert!(
            recorder.events().is_empty(),
            "a partially placed cue is not a dropped cue"
        );
        let segments = segments(&output);
        let carried = segments
            .iter()
            .filter(|segment| {
                str::from_utf8(segment.payload.as_bytes())
                    .is_ok_and(|body| body.contains("straddles"))
            })
            .count();
        assert!(carried > 0);
        Ok(())
    }

    #[test]
    fn every_part_carries_the_cues_on_screen_during_it() -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux_with_parts(Codec::WebVtt, 2, 1);
        let mut output = Vec::new();
        // Spans the whole first window, so it is on screen during both parts.
        mux.push(
            sample(Codec::WebVtt, 0, 2 * SECOND, b"spanning"),
            &mut output,
        )?;
        mux.tick(sibling(4), &mut output)?;

        let parts = parts(&output);
        assert!(parts.len() >= 2);
        for part in parts.iter().take(2) {
            let body = str::from_utf8(part.payload.as_bytes())?;
            assert!(
                body.contains("spanning"),
                "a part that omitted an active cue would make INDEPENDENT=YES a \
                 lie: a client joining there would see no caption"
            );
            assert!(part.independent);
        }
        let completion = completions(&output)
            .first()
            .and_then(|completion| completion.payload.as_ref())
            .expect("WebVTT parent has a canonical standalone body");
        let parent = str::from_utf8(completion.as_bytes())?;
        assert_eq!(parent.matches("spanning").count(), 1);
        Ok(())
    }

    #[test]
    fn an_open_text_state_tiles_every_part_until_clear() -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux_with_parts(Codec::Text, 2, 1);
        let mut output = Vec::new();
        mux.push(text_cue(0, b"and then we had"), &mut output)?;
        mux.tick(sibling(3), &mut output)?;

        let parts = parts(&output);
        assert!(parts.len() >= 2);
        let first = str::from_utf8(parts[0].payload.as_bytes())?;
        let second = str::from_utf8(parts[1].payload.as_bytes())?;
        assert!(first.contains("00:00:00.000 --> 00:00:01.000\nand then we had"));
        assert!(second.contains("00:00:01.000 --> 00:00:02.000\nand then we had"));
        Ok(())
    }

    #[test]
    fn a_clear_inside_a_part_leaves_later_parts_empty() -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux_with_parts(Codec::Text, 2, 1);
        let mut output = Vec::new();
        mux.push(text_cue(0, b"visible"), &mut output)?;
        mux.push(text_cue(SECOND_TICKS + SECOND_TICKS / 2, b""), &mut output)?;
        mux.tick(sibling(4), &mut output)?;

        let parts = parts(&output);
        assert!(parts.len() >= 3);
        assert!(str::from_utf8(parts[0].payload.as_bytes())?.contains("visible"));
        let crossing = str::from_utf8(parts[1].payload.as_bytes())?;
        assert!(crossing.contains("00:00:01.000 --> 00:00:01.500\nvisible"));
        assert!(parts[2].payload.is_empty());
        Ok(())
    }

    #[test]
    fn a_long_lived_text_state_is_reported_once_without_clearing()
    -> Result<(), Box<dyn std::error::Error>> {
        let (events, recorded) = RecordedEvents::sink();
        let mut mux = parts_mux(Codec::Text, 2, 1, events);
        let mut output = Vec::new();
        mux.push(text_cue(0, b"still active"), &mut output)?;
        mux.tick(sibling(31), &mut output)?;
        mux.tick(sibling(32), &mut output)?;

        assert!(parts(&output).iter().any(|part| {
            str::from_utf8(part.payload.as_bytes()).is_ok_and(|body| body.contains("still active"))
        }));
        assert!(matches!(
            recorded.events().as_slice(),
            [SessionEvent::SubtitleStateLongLived {
                track: TrackId(0),
                ..
            }]
        ));
        Ok(())
    }

    #[test]
    fn identical_restatement_advances_transition_ordering() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut mux = mux(Codec::Text, 30);
        let mut output = Vec::new();
        mux.push(text_cue(0, b"same"), &mut output)?;
        mux.push(text_cue(2 * SECOND_TICKS, b"same"), &mut output)?;

        let error = mux
            .push(text_cue(SECOND_TICKS, b""), &mut output)
            .expect_err("clear before the latest state update must be rejected");
        assert!(error.to_string().contains("decreasing subtitle PTS"));
        Ok(())
    }

    #[test]
    fn open_state_starting_behind_sealed_parts_is_reported_late()
    -> Result<(), Box<dyn std::error::Error>> {
        let (events, recorded) = RecordedEvents::sink();
        let mut mux = parts_mux(Codec::Text, 2, 1, events);
        let mut output = Vec::new();
        mux.tick(sibling(2), &mut output)?;
        mux.push(text_cue(0, b"late but active"), &mut output)?;
        mux.tick(sibling(3), &mut output)?;

        assert!(
            recorded
                .events()
                .iter()
                .any(|event| matches!(event, SessionEvent::SubtitleCueTooLate { .. }))
        );
        assert!(parts(&output).iter().any(|part| {
            str::from_utf8(part.payload.as_bytes())
                .is_ok_and(|body| body.contains("late but active"))
        }));
        Ok(())
    }

    #[test]
    fn parts_tile_their_segment_exactly() -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux_with_parts(Codec::WebVtt, 2, 1);
        let mut output = Vec::new();
        mux.tick(sibling(5), &mut output)?;

        let completions = completions(&output);
        assert!(!completions.is_empty());
        for completion in &completions {
            let covered: u64 = parts(&output)
                .iter()
                .filter(|part| part.packaging_segment_id == completion.packaging_segment_id)
                .map(|part| part.duration)
                .sum();
            assert_eq!(
                covered, completion.duration,
                "a parent's duration is the sum of its parts, or delivery would \
                 serve a segment whose bytes and timing disagree"
            );
            let first = parts(&output)
                .into_iter()
                .find(|part| part.packaging_segment_id == completion.packaging_segment_id)
                .expect("a completed segment has parts");
            assert_eq!(first.media_start, completion.media_start);
        }
        Ok(())
    }

    #[test]
    fn a_silent_track_advances_on_the_part_grid() -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux_with_parts(Codec::WebVtt, 2, 1);
        let mut output = Vec::new();
        for second in 1..=4 {
            mux.tick(sibling(second), &mut output)?;
        }

        let parts = parts(&output);
        assert_eq!(parts.len(), 4, "two segments of two one-second parts each");
        for (index, part) in parts.iter().enumerate() {
            assert!(part.payload.is_empty());
            assert_eq!(part.duration, SECOND);
            assert_eq!(part.chunk_index, u32::try_from(index % 2)?);
        }
        assert_eq!(completions(&output).len(), 2);
        Ok(())
    }

    #[test]
    fn a_cue_landing_only_in_published_parts_is_reported_and_dropped()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sink, recorder) = RecordedEvents::sink();
        let mut mux = parts_mux(Codec::WebVtt, 2, 1, sink);
        let mut output = Vec::new();
        // Seals part 0 of window 0 while leaving the window itself open.
        mux.tick(sibling(1), &mut output)?;
        assert_eq!(parts(&output).len(), 1);

        // Confined to the part that already went out.
        mux.push(
            sample(Codec::WebVtt, 0, SECOND / 2, b"too late"),
            &mut output,
        )?;

        assert!(
            matches!(
                recorder.events().as_slice(),
                [SessionEvent::SubtitleCueTooLate { .. }]
            ),
            "an open window is not enough: the parts that could have carried \
             this cue are already published"
        );
        Ok(())
    }

    #[test]
    fn draining_an_untouched_window_publishes_only_the_media_it_has()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux_with_parts(Codec::WebVtt, 2, 1);
        let mut output = Vec::new();
        // Half a part's worth of cue, and no heartbeat: nothing has been sealed,
        // so the drain must not claim a whole part of media exists.
        mux.push(sample(Codec::WebVtt, 0, SECOND / 2, b"short"), &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        let completion = completions(&output)
            .first()
            .copied()
            .expect("a final drain completes its open segment");
        assert_eq!(completion.duration, SECOND / 2);
        let covered: u64 = parts(&output).iter().map(|part| part.duration).sum();
        assert_eq!(covered, completion.duration);
        Ok(())
    }

    #[test]
    fn draining_after_the_heartbeat_outran_the_last_cue_still_closes_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux_with_parts(Codec::WebVtt, 2, 1);
        let mut output = Vec::new();
        // Sparse captions: one early cue, then siblings carry the clock well
        // past it, leaving a later window holding published parts but no cue
        // content at all.
        mux.push(sample(Codec::WebVtt, 0, SECOND / 2, b"early"), &mut output)?;
        mux.tick(sibling(5), &mut output)?;
        let published = parts(&output).len();
        assert!(published > 2, "the heartbeat reached a later window");

        mux.finish(FinishReason::Final, &mut output)?;

        let last = completions(&output)
            .last()
            .copied()
            .expect("the drain completes the window the heartbeat opened");
        let covered: u64 = parts(&output)
            .iter()
            .filter(|part| part.packaging_segment_id == last.packaging_segment_id)
            .map(|part| part.duration)
            .sum();
        assert_eq!(
            covered, last.duration,
            "a window past the last cue still owes delivery exactly the media \
             it already published"
        );
        Ok(())
    }

    #[test]
    fn subrip_is_converted_and_positioned_cues_are_rejected_transactionally()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::SubRip, 2);
        let mut output = Vec::new();
        mux.push(
            sample(
                Codec::SubRip,
                0,
                SECOND,
                b"<b>Hello &amp; <font color=\"red\">world</font></b>",
            ),
            &mut output,
        )?;
        let output_before_error = output.len();
        let NormalizedSample::Subtitle(positioned) = sample(
            Codec::SubRip,
            i64::try_from(SECOND).expect("second fits i64"),
            SECOND,
            b"placed",
        ) else {
            unreachable!()
        };
        let mut positioned = positioned;
        positioned.position = Some(SubtitlePosition {
            x1: 1,
            y1: 2,
            x2: 3,
            y2: 4,
        });
        assert!(
            mux.push(NormalizedSample::Subtitle(positioned), &mut output)
                .is_err()
        );
        assert_eq!(output.len(), output_before_error);
        mux.finish(FinishReason::Final, &mut output)?;
        let body = str::from_utf8(segments(&output)[0].payload.as_bytes())?;
        assert!(body.contains("<b>Hello &amp; world</b>"));
        assert!(!body.contains("<font"));
        Ok(())
    }

    #[test]
    fn superseded_discards_unsealed_tail_and_finish_is_idempotent()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        mux.push(sample(Codec::WebVtt, 0, SECOND, b"tail"), &mut output)?;
        mux.finish(FinishReason::Superseded, &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        assert_eq!(
            output
                .iter()
                .filter(|media| matches!(media, PackagedMedia::Segment(_)))
                .count(),
            0
        );
        Ok(())
    }

    #[test]
    fn out_of_order_and_pathological_cues_fail_before_output() {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        assert!(
            mux.push(
                sample(Codec::WebVtt, 0, 65 * 2 * SECOND, b"too long"),
                &mut output
            )
            .is_err()
        );
        assert!(output.is_empty());

        mux.push(
            sample(
                Codec::WebVtt,
                i64::try_from(SECOND).expect("second fits i64"),
                SECOND,
                b"valid",
            ),
            &mut output,
        )
        .expect("valid cue starts the muxer");
        let before = output.len();
        assert!(
            mux.push(sample(Codec::WebVtt, 0, SECOND, b"stale"), &mut output)
                .is_err()
        );
        assert_eq!(output.len(), before);
    }

    #[tokio::test]
    async fn initialization_plus_segment_round_trips_through_avformat() {
        let packet = round_trip(
            Codec::WebVtt,
            b"round trip",
            WebVttCueMetadata {
                identifier: Some(Arc::from("round-trip")),
                settings: Some(Arc::from("position:25%")),
            },
        )
        .await;

        assert_eq!(packet.pts, Some(0));
        assert_eq!(packet.duration, Some(1_000));
        assert_eq!(packet.payload.as_bytes(), b"round trip");
        assert_eq!(packet.webvtt.identifier.as_deref(), Some("round-trip"));
        assert_eq!(packet.webvtt.settings.as_deref(), Some("position:25%"));
    }

    #[tokio::test]
    async fn converted_subrip_styling_round_trips_as_webvtt() {
        let packet = round_trip(
            Codec::SubRip,
            b"<B>Hello &amp; <i>world</i></B>",
            WebVttCueMetadata::default(),
        )
        .await;

        assert_eq!(packet.pts, Some(0));
        assert_eq!(packet.duration, Some(1_000));
        assert_eq!(
            packet.payload.as_bytes(),
            b"<b>Hello &amp; <i>world</i></b>"
        );
    }

    /// The rendered body of every segment, concatenated.
    fn rendered(output: &[PackagedMedia]) -> String {
        segments(output)
            .iter()
            .map(|segment| String::from_utf8_lossy(segment.payload.as_bytes()).into_owned())
            .collect()
    }

    /// An open-ended cue, as FLV script data delivers one: a start and no end.
    fn text_cue(start: i64, text: &[u8]) -> NormalizedSample {
        sample(Codec::Text, start, 0, text)
    }

    /// One second on the 90 kHz presentation clock, as a cue timestamp.
    const SECOND_TICKS: i64 = 90_000;

    #[test]
    fn an_open_ended_cue_is_held_until_its_successor_supplies_an_end()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::Text, 10);
        let mut output = Vec::new();

        // Nothing can be rendered from the first cue alone: its end is not yet
        // known, and guessing one is what the successor exists to avoid.
        mux.push(text_cue(0, b"first"), &mut output)?;
        assert!(rendered(&output).is_empty());

        mux.push(text_cue(SECOND_TICKS, b"second"), &mut output)?;
        mux.tick(sibling(4), &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        let body = rendered(&output);
        // The first cue ends exactly where the second begins.
        assert!(body.contains("00:00:00.000 --> 00:00:01.000\nfirst\n"));
        // The last cue closes where the presentation itself ends.
        assert!(body.contains("00:00:01.000 --> 00:00:04.000\nsecond\n"));
        Ok(())
    }

    #[test]
    fn an_empty_text_cue_ends_the_held_cue_and_publishes_nothing()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::Text, 30);
        let mut output = Vec::new();

        mux.push(text_cue(0, b"first"), &mut output)?;
        // The publisher's own clear is the only normal display end.
        mux.push(text_cue(SECOND_TICKS, b""), &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        let body = rendered(&output);
        assert!(
            body.contains("00:00:00.000 --> 00:00:01.000\nfirst\n"),
            "the clear must end the held cue where it was sent: {body}"
        );
        // A clear is an instruction, not content: nothing is published for it,
        // and in particular not a cue with an empty body, which would
        // terminate the preceding cue in the rendered WebVTT.
        assert_eq!(
            body.matches("-->").count(),
            1,
            "a clear must not publish a cue of its own: {body}"
        );
        Ok(())
    }

    #[test]
    fn a_clear_ends_state_at_the_publishers_timestamp() -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::Text, 30);
        let mut output = Vec::new();

        mux.push(text_cue(0, b"held"), &mut output)?;
        mux.push(text_cue(SECOND_TICKS / 2, b""), &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        let body = rendered(&output);
        assert!(
            body.contains("00:00:00.000 --> 00:00:00.500\nheld\n"),
            "clear at 500ms must end the cue at 500ms, not at the cap: {body}"
        );
        Ok(())
    }

    #[test]
    fn a_clear_with_nothing_held_is_harmless() -> Result<(), Box<dyn std::error::Error>> {
        // A publisher may clear a display that is already empty — at startup,
        // or after two clears in a row. Nothing is held, so nothing ends, and
        // the stream stays valid.
        let mut mux = mux(Codec::Text, 30);
        let mut output = Vec::new();

        mux.push(text_cue(0, b""), &mut output)?;
        mux.push(text_cue(SECOND_TICKS, b""), &mut output)?;
        mux.push(text_cue(2 * SECOND_TICKS, b"after"), &mut output)?;
        mux.tick(sibling(5), &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        let body = rendered(&output);
        assert!(
            body.contains("00:00:02.000 --> 00:00:05.000\nafter\n"),
            "a cue after redundant clears must still publish: {body}"
        );
        assert_eq!(
            body.matches("-->").count(),
            1,
            "clears published cues: {body}"
        );
        Ok(())
    }

    #[test]
    fn a_cue_arriving_before_a_clear_is_still_out_of_order()
    -> Result<(), Box<dyn std::error::Error>> {
        // A clear is a point on the track's timeline even though it displays
        // nothing, so it has to advance the ordering reference. Otherwise a
        // publisher could rewind past it undetected.
        let mut mux = mux(Codec::Text, 30);
        let mut output = Vec::new();

        mux.push(text_cue(2 * SECOND_TICKS, b""), &mut output)?;
        assert!(
            mux.push(text_cue(SECOND_TICKS, b"rewound"), &mut output)
                .is_err(),
            "a cue before the last clear must be rejected"
        );
        Ok(())
    }

    #[test]
    fn a_gap_without_a_clear_keeps_the_publishers_state_visible()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::Text, 30);
        let mut output = Vec::new();

        // Ten seconds of silence between cues. RushLS cannot know whether that
        // is deliberate, so the successor — not an inferred cap — ends it.
        mux.push(text_cue(0, b"first"), &mut output)?;
        mux.push(text_cue(10 * SECOND_TICKS, b"second"), &mut output)?;
        mux.tick(sibling(13), &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        let body = rendered(&output);
        assert!(body.contains("00:00:00.000 --> 00:00:10.000\nfirst\n"));
        Ok(())
    }

    #[test]
    fn identical_replacements_are_coalesced() -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::Text, 30);
        let mut output = Vec::new();

        // A second apart, well inside the cap. Overlapping spans would stack as
        // two captions on screen at once, which is what resolving the end from
        // the successor exists to prevent.
        for index in 0..4 {
            mux.push(text_cue(index * SECOND_TICKS, b"line"), &mut output)?;
        }
        mux.tick(sibling(4), &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        let body = rendered(&output);
        assert!(body.contains("00:00:00.000 --> 00:00:04.000\nline\n"));
        assert_eq!(body.matches("line").count(), 1);
        Ok(())
    }

    #[test]
    fn a_held_cue_is_published_before_its_window_seals() -> Result<(), Box<dyn std::error::Error>> {
        let (events, recorded) = RecordedEvents::sink();
        let mut mux = mux_with_events(Codec::Text, 2, events);
        let mut output = Vec::new();

        // The cue arrives, then the presentation clock advances without a
        // successor. Every sealed window must still carry the active state.
        mux.push(text_cue(0, b"only"), &mut output)?;
        mux.tick(sibling(6), &mut output)?;

        let body = rendered(&output);
        assert!(body.contains("00:00:00.000 --> 00:00:02.000\nonly\n"));
        // Committed on time, so this is not the drop path.
        assert!(recorded.events().is_empty());
        Ok(())
    }

    #[test]
    fn a_cue_replaced_at_the_same_instant_is_dropped_rather_than_rendered_empty()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::Text, 10);
        let mut output = Vec::new();

        // Two cues stamped identically: the first is replaced before it was
        // ever visible, and a zero-length span is not a cue WebVTT can express.
        mux.push(text_cue(0, b"replaced"), &mut output)?;
        mux.push(text_cue(0, b"winner"), &mut output)?;
        mux.tick(sibling(3), &mut output)?;
        mux.finish(FinishReason::Final, &mut output)?;

        let body = rendered(&output);
        assert!(!body.contains("replaced"));
        assert!(body.contains("winner"));
        Ok(())
    }

    #[test]
    fn a_superseded_publication_drops_its_held_cue() -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::Text, 10);
        let mut output = Vec::new();

        // A successor's media follows immediately, so inventing a trailing cue
        // for the publication being replaced would collide with it.
        mux.push(text_cue(0, b"held"), &mut output)?;
        mux.finish(FinishReason::Superseded, &mut output)?;

        assert!(rendered(&output).is_empty());
        Ok(())
    }

    #[test]
    fn decreasing_text_cue_timestamps_are_still_refused() {
        let mut mux = mux(Codec::Text, 10);
        let mut output = Vec::new();

        mux.push(text_cue(2 * SECOND_TICKS, b"first"), &mut output)
            .expect("the first cue is accepted");
        assert!(
            mux.push(text_cue(SECOND_TICKS, b"rewound"), &mut output)
                .is_err()
        );
    }

    #[test]
    fn a_malformed_text_cue_fails_on_arrival_rather_than_when_it_resolves() {
        let mut mux = mux(Codec::Text, 10);
        let mut output = Vec::new();

        // Held cues are validated when they arrive, so the error names the
        // push that caused it rather than an unrelated later one.
        assert!(mux.push(text_cue(0, b"bad\0cue"), &mut output).is_err());
    }

    #[test]
    fn a_text_cue_that_carries_its_own_end_is_placed_immediately()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut mux = mux(Codec::Text, 10);
        let mut output = Vec::new();

        // Holding is for cues with no end. One that arrived with a duration has
        // already said when it stops, and resolving it from a successor would
        // silently replace the publisher's own span with a guess.
        mux.push(
            sample(Codec::Text, 0, u64::try_from(SECOND_TICKS)? / 2, b"timed"),
            &mut output,
        )?;
        mux.finish(FinishReason::Final, &mut output)?;

        assert!(rendered(&output).contains("00:00:00.000 --> 00:00:00.500\ntimed\n"));
        Ok(())
    }
}
