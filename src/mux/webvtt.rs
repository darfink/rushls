//! Track-local WebVTT segmentation.
//!
//! Reading the cues themselves belongs to [`cue`], which keeps every accepted
//! input format's rules in one place; this module only decides where segment
//! boundaries fall and renders the windows between them.

use std::{cmp::Ordering, collections::VecDeque, num::NonZero, sync::Arc};

mod cue;

use cue::CueDialect;

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
    FinishReason, InitializationSegment, MediaSegmentFormat, MuxError, PackagedMedia,
    PackagedRendition, PackagedSegment, PackagingRenditionId, PackagingSegmentId, RenditionConfig,
    RenditionKey, RenditionMedia, TrackPackager,
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

const INITIALIZATION: &[u8] = b"WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0\n\n";

#[derive(Clone, Debug)]
struct Cue {
    start_ms: u64,
    end_ms: u64,
    identifier: Option<Arc<str>>,
    settings: Option<Arc<str>>,
    text: Arc<str>,
}

#[derive(Debug)]
struct Window {
    id: u64,
    start: TickTimestamp,
    cues: Vec<Arc<Cue>>,
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
            chunk_target: None,
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
    windows: VecDeque<Window>,
    next_window_id: u64,
    last_cue_start: Option<TickTimestamp>,
    maximum_cue_end: Option<TickTimestamp>,
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

        Ok(Self {
            rendition_id,
            track_id: track.id,
            dialect,
            plan,
            events,
            origin,
            first_boundary,
            segment_ticks: plan.segment_duration.get(),
            windows: VecDeque::from([Window {
                id: 0,
                start: origin,
                cues: Vec::new(),
            }]),
            next_window_id: 1,
            last_cue_start: None,
            maximum_cue_end: None,
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
        let content = self.dialect.read(sample)?;
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
        let milliseconds = TimebaseProjection::new(
            self.plan.timebase,
            Timebase::new(nz::u32!(1), nz::u32!(1_000)),
        );
        let (start_ms, duration_ms) =
            milliseconds
                .interval(start, sample.duration)
                .ok_or_else(|| {
                    mux_error("subtitle cue cannot be represented in WebVTT milliseconds")
                })?;
        let start_ms = u64::try_from(start_ms)
            .map_err(|_| mux_error("subtitle cue begins before the publication origin"))?;
        let end_ms = start_ms
            .checked_add(duration_ms)
            .ok_or_else(|| mux_error("WebVTT cue end overflowed"))?;
        Ok(Cue {
            start_ms,
            end_ms,
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
            });
            self.next_window_id = next
                .checked_add(1)
                .ok_or_else(|| mux_error("WebVTT segment ID overflowed"))?;
            created += 1;
        }
        Ok(())
    }

    fn emit(&self, window: &Window, duration: TickDuration, out: &mut dyn Appender<PackagedMedia>) {
        out.push(PackagedMedia::Segment(PackagedSegment {
            rendition_id: self.rendition_id,
            packaging_segment_id: PackagingSegmentId(window.id),
            media_start: window.start,
            duration,
            independent: true,
            payload: Payload::from(render_window(window)),
        }));
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
        let prepared = self.prepare_cue(&sample)?;

        self.initialize(out);
        self.ensure_through(prepared.last_index)?;
        self.seal_before(self.instant(prepared.start), out);
        let mut placed = false;
        for window in &mut self.windows {
            if window.id >= prepared.first_index && window.id <= prepared.last_index {
                window.cues.push(Arc::clone(&prepared.cue));
                placed = true;
            }
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
        self.finished = true;
        if reason == FinishReason::Superseded {
            self.windows.clear();
            return Ok(());
        }
        let Some(end) = self.maximum_cue_end else {
            self.windows.clear();
            return Ok(());
        };

        while let Some(window) = self.windows.pop_front() {
            if window.start >= end {
                break;
            }
            let duration = end
                .checked_sub(window.start)
                .and_then(|duration| u64::try_from(duration).ok())
                .map(|duration| {
                    duration.min(
                        self.window_duration(window.id)
                            .expect("queued WebVTT windows have valid timing"),
                    )
                })
                .filter(|duration| *duration > 0)
                .ok_or_else(|| mux_error("final WebVTT segment duration overflowed"))?;
            self.emit(&window, duration, out);
        }
        self.windows.clear();
        Ok(())
    }
}

fn render_window(window: &Window) -> Vec<u8> {
    let mut body = String::new();
    for cue in &window.cues {
        if let Some(identifier) = cue.identifier.as_deref()
            && !identifier.is_empty()
        {
            body.push_str(identifier);
            body.push('\n');
        }
        body.push_str(&format_timestamp(cue.start_ms));
        body.push_str(" --> ");
        body.push_str(&format_timestamp(cue.end_ms));
        if let Some(settings) = cue.settings.as_deref()
            && !settings.is_empty()
        {
            body.push(' ');
            body.push_str(settings);
        }
        body.push('\n');
        body.push_str(&cue.text);
        body.push_str("\n\n");
    }
    body.into_bytes()
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

    fn plan(segment_seconds: u64) -> TrackSegmentationPlan {
        let duration =
            std::num::NonZero::new(segment_seconds * SECOND).expect("fixture segments are nonzero");
        PlanBuilder::new(0, Timebase::hz90k(), duration)
            .part(nz::u32!(1), nz::u64!(90_000))
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
}
