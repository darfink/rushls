//! Track-local WebVTT segmentation and SubRip cue conversion.

use std::{collections::VecDeque, str, sync::Arc};

mod subrip;

use crate::{
    domain::{
        Appender, Codec, DiscoveredTrack, MediaKind, Payload, TickDuration, TickTimestamp,
        Timebase, TimebaseProjection, TrackId, WebVttCueMetadata,
    },
    media::{NormalizedSample, SubtitleSample},
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
) -> Result<(PackagedRendition, Box<dyn TrackPackager>), MuxError> {
    if track.kind() != MediaKind::Subtitle {
        return Err(invalid("WebVTT output requires a subtitle track"));
    }
    if !matches!(track.codec, Codec::WebVtt | Codec::SubRip) {
        return Err(invalid(format!(
            "{} subtitle codec {:?} cannot be converted to WebVTT",
            track.id, track.codec
        )));
    }
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
    let packager = WebVttTrack::new(rendition_id, track, plan)?;
    Ok((rendition, Box::new(packager)))
}

fn packaged_rendition(
    rendition_id: PackagingRenditionId,
    track: &DiscoveredTrack,
    plan: &TrackSegmentationPlan,
) -> PackagedRendition {
    let fallback_name = format!("Subtitle {}", rendition_id.0 + 1);
    PackagedRendition {
        packaging_rendition_id: rendition_id,
        key: RenditionKey::for_source(track),
        source_tracks: Arc::from([track.id]),
        config: RenditionConfig {
            timebase: plan.timebase,
            segment_target: plan.segment_duration,
            // Cue windows are cut on the planned grid regardless of cue
            // arrival, so a WebVTT segment never extends past its target.
            maximum_segment_duration: plan.segment_duration,
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
    codec: Codec,
    plan: TrackSegmentationPlan,
    origin: TickTimestamp,
    segment_ticks: TickDuration,
    windows: VecDeque<Window>,
    next_window_id: u64,
    last_cue_start: Option<TickTimestamp>,
    maximum_cue_end: Option<TickTimestamp>,
    initialized: bool,
    finished: bool,
}

impl WebVttTrack {
    fn new(
        rendition_id: PackagingRenditionId,
        track: &DiscoveredTrack,
        plan: TrackSegmentationPlan,
    ) -> Result<Self, MuxError> {
        let origin = plan
            .segmentation_origin_pts
            .checked_sub(plan.presentation_origin_pts)
            .ok_or_else(|| invalid("WebVTT segmentation origin rebasing overflowed"))?;
        let first_boundary = plan
            .first_segment_boundary_pts
            .checked_sub(plan.presentation_origin_pts)
            .ok_or_else(|| invalid("WebVTT first boundary rebasing overflowed"))?;
        if origin.checked_add_unsigned(plan.segment_duration.get()) != Some(first_boundary) {
            return Err(invalid("WebVTT first segment boundary is inconsistent"));
        }

        Ok(Self {
            rendition_id,
            track_id: track.id,
            codec: track.codec,
            plan,
            origin,
            segment_ticks: plan.segment_duration.get(),
            windows: VecDeque::from([Window {
                id: 0,
                start: origin,
                cues: Vec::new(),
            }]),
            next_window_id: 1,
            last_cue_start: None,
            maximum_cue_end: None,
            initialized: false,
            finished: false,
        })
    }

    fn prepare_cue(&self, sample: SubtitleSample) -> Result<PreparedCue, MuxError> {
        if sample.track_id != self.track_id {
            return Err(mux_error(format!(
                "{} received a cue for {}",
                self.track_id, sample.track_id
            )));
        }
        if sample.codec != self.codec {
            return Err(mux_error(format!(
                "{} changed subtitle codec while muxing",
                self.track_id
            )));
        }
        if sample.position.is_some() {
            return Err(mux_error(format!(
                "{} carries pixel-positioned subtitle text that WebVTT cannot map without a canvas",
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
        // Verify every required window start before emitting initialization or
        // modifying the queue, keeping a failed push transactional.
        self.window_start(last_index)?;

        let metadata = match self.codec {
            Codec::WebVtt => validate_webvtt_metadata(sample.webvtt)?,
            Codec::SubRip => {
                if sample.webvtt != WebVttCueMetadata::default() {
                    return Err(mux_error(format!(
                        "{} SubRip cue carries WebVTT-only metadata",
                        self.track_id
                    )));
                }
                WebVttCueMetadata::default()
            }
            _ => {
                return Err(mux_error(
                    "unsupported subtitle codec reached WebVTT output",
                ));
            }
        };
        let text = match self.codec {
            Codec::WebVtt => webvtt_text(sample.payload.as_bytes())?,
            Codec::SubRip => subrip::convert(sample.payload.as_bytes())?,
            _ => unreachable!("codec was checked above"),
        };
        let rendered_bytes = text
            .len()
            .checked_add(metadata.retained_bytes())
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

        Ok(PreparedCue {
            start,
            end,
            first_index,
            last_index,
            cue: Arc::new(Cue {
                start_ms,
                end_ms,
                identifier: metadata.identifier,
                settings: metadata.settings,
                text,
            }),
        })
    }

    fn segment_index(&self, timestamp: TickTimestamp) -> Result<u64, MuxError> {
        let offset = timestamp
            .checked_sub(self.origin)
            .and_then(|offset| u64::try_from(offset).ok())
            .ok_or_else(|| mux_error("subtitle timestamp precedes segmentation origin"))?;
        Ok(offset / self.segment_ticks)
    }

    fn window_start(&self, id: u64) -> Result<TickTimestamp, MuxError> {
        let offset = u128::from(id)
            .checked_mul(u128::from(self.segment_ticks))
            .and_then(|offset| i128::try_from(offset).ok())
            .ok_or_else(|| mux_error("WebVTT segment offset overflowed"))?;
        i128::from(self.origin)
            .checked_add(offset)
            .and_then(|start| TickTimestamp::try_from(start).ok())
            .ok_or_else(|| mux_error("WebVTT segment start overflowed"))
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

    fn seal_before(&mut self, timestamp: TickTimestamp, out: &mut dyn Appender<PackagedMedia>) {
        while self.windows.front().is_some_and(|window| {
            window
                .start
                .checked_add_unsigned(self.segment_ticks)
                .is_some_and(|end| end <= timestamp)
        }) {
            let window = self.windows.pop_front().expect("front was inspected above");
            self.emit(window, self.segment_ticks, out);
        }
    }

    fn emit(&self, window: Window, duration: TickDuration, out: &mut dyn Appender<PackagedMedia>) {
        out.push(PackagedMedia::Segment(PackagedSegment {
            rendition_id: self.rendition_id,
            packaging_segment_id: PackagingSegmentId(window.id),
            media_start: window.start,
            duration,
            independent: true,
            payload: Payload::from(render_window(&window)),
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
        let prepared = self.prepare_cue(sample)?;

        self.initialize(out);
        self.ensure_through(prepared.last_index)?;
        self.seal_before(prepared.start, out);
        for window in &mut self.windows {
            if window.id >= prepared.first_index && window.id <= prepared.last_index {
                window.cues.push(Arc::clone(&prepared.cue));
            }
        }
        self.last_cue_start = Some(prepared.start);
        self.maximum_cue_end = Some(
            self.maximum_cue_end
                .map_or(prepared.end, |end| end.max(prepared.end)),
        );
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
                .map(|duration| duration.min(self.segment_ticks))
                .filter(|duration| *duration > 0)
                .ok_or_else(|| mux_error("final WebVTT segment duration overflowed"))?;
            self.emit(window, duration, out);
        }
        self.windows.clear();
        Ok(())
    }
}

fn validate_webvtt_metadata(metadata: WebVttCueMetadata) -> Result<WebVttCueMetadata, MuxError> {
    if metadata
        .identifier
        .as_deref()
        .is_some_and(|value| value.contains(['\0', '\r', '\n']) || value.contains("-->"))
    {
        return Err(mux_error("WebVTT cue identifier is not a single safe line"));
    }
    if let Some(settings) = metadata.settings.as_deref() {
        if settings.contains(['\0', '\r', '\n']) || settings.contains("-->") {
            return Err(mux_error("WebVTT cue settings are not a single safe line"));
        }
        if settings
            .split_ascii_whitespace()
            .any(|setting| setting.starts_with("region:"))
        {
            return Err(mux_error(
                "WebVTT cue regions require a global REGION definition",
            ));
        }
    }
    Ok(metadata)
}

fn webvtt_text(bytes: &[u8]) -> Result<Arc<str>, MuxError> {
    let text = str::from_utf8(bytes).map_err(|_| mux_error("WebVTT cue text is not UTF-8"))?;
    if text.contains('\0') {
        return Err(mux_error("WebVTT cue text contains a NUL byte"));
    }
    let normalized = normalize_newlines(text);
    if normalized.is_empty() || normalized.contains("\n\n") {
        return Err(mux_error(
            "WebVTT cue text is empty or contains a blank line",
        ));
    }
    Ok(Arc::from(normalized))
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
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
    use super::*;
    use crate::{
        domain::{MediaKind, SubtitlePosition, WebVttCueMetadata, fixtures::TrackBuilder},
        media::SubtitleSample,
    };

    const SECOND: u64 = 90_000;

    fn track(codec: Codec) -> DiscoveredTrack {
        TrackBuilder::new(0, MediaKind::Subtitle)
            .codec(codec)
            .timebase(Timebase::hz90k())
            .build()
    }

    fn plan(segment_seconds: u64) -> TrackSegmentationPlan {
        let duration = segment_seconds * SECOND;
        TrackSegmentationPlan {
            track_id: TrackId(0),
            timebase: Timebase::hz90k(),
            presentation_origin_pts: 0,
            segmentation_origin_pts: 0,
            first_segment_boundary_pts: i64::try_from(duration).expect("fixture fits"),
            segment_duration: std::num::NonZero::new(duration).expect("fixture is nonzero"),
            part_duration: nz::u64!(90_000),
        }
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
        WebVttTrack::new(
            PackagingRenditionId(3),
            &track(codec),
            plan(segment_seconds),
        )
        .expect("fixture mux starts")
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
        let mut cue = match sample(codec, 0, SECOND, text) {
            NormalizedSample::Subtitle(sample) => sample,
            _ => unreachable!(),
        };
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
    fn webvtt_metadata_rejects_unsafe_lines_and_undefined_regions() {
        for metadata in [
            WebVttCueMetadata {
                identifier: Some(Arc::from("bad\0identifier")),
                settings: None,
            },
            WebVttCueMetadata {
                identifier: None,
                settings: Some(Arc::from("align:start\nposition:20%")),
            },
            WebVttCueMetadata {
                identifier: None,
                settings: Some(Arc::from("region:captions")),
            },
        ] {
            assert!(validate_webvtt_metadata(metadata).is_err());
        }
    }

    #[test]
    fn timestamp_formatting_does_not_wrap_after_one_hour() {
        assert_eq!(format_timestamp(3_723_004), "01:02:03.004");
    }

    #[test]
    fn initialization_precedes_segmented_webvtt_with_full_cue_timestamps() {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        let mut first = match sample(Codec::WebVtt, 0, 3 * SECOND, b"first") {
            NormalizedSample::Subtitle(sample) => sample,
            _ => unreachable!(),
        };
        first.webvtt = WebVttCueMetadata {
            identifier: Some(Arc::from("cue-one")),
            settings: Some(Arc::from("align:start")),
        };
        mux.push(NormalizedSample::Subtitle(first), &mut output)
            .expect("cue is accepted");
        mux.finish(FinishReason::Final, &mut output)
            .expect("tail flushes");

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
            let body = str::from_utf8(segment.payload.as_bytes()).expect("body is text");
            assert!(body.contains("cue-one\n00:00:00.000 --> 00:00:03.000 align:start"));
        }
    }

    #[test]
    fn long_cue_windows_stay_open_for_later_overlapping_cues() {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        mux.push(sample(Codec::WebVtt, 0, 6 * SECOND, b"long"), &mut output)
            .expect("long cue is accepted");
        mux.push(
            sample(Codec::WebVtt, 3 * SECOND as i64, SECOND, b"overlap"),
            &mut output,
        )
        .expect("overlapping cue is accepted");
        mux.finish(FinishReason::Final, &mut output)
            .expect("windows flush");

        let segments = segments(&output);
        assert_eq!(segments.len(), 3);
        let middle = str::from_utf8(segments[1].payload.as_bytes()).expect("body is text");
        assert!(middle.contains("long"));
        assert!(middle.contains("overlap"));
        let last = str::from_utf8(segments[2].payload.as_bytes()).expect("body is text");
        assert!(last.contains("long"));
        assert!(!last.contains("overlap"));
    }

    #[test]
    fn sparse_cues_emit_empty_windows_without_inventing_cue_text() {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        mux.push(sample(Codec::WebVtt, 0, SECOND, b"first"), &mut output)
            .expect("first cue is accepted");
        mux.push(
            sample(Codec::WebVtt, 6 * SECOND as i64, SECOND, b"later"),
            &mut output,
        )
        .expect("later cue is accepted");
        mux.finish(FinishReason::Interrupted, &mut output)
            .expect("tail flushes");

        let segments = segments(&output);
        assert_eq!(segments.len(), 4);
        assert!(segments[1].payload.is_empty());
        assert!(segments[2].payload.is_empty());
        assert_eq!(segments[3].media_start, 6 * SECOND as i64);
    }

    #[test]
    fn subrip_is_converted_and_positioned_cues_are_rejected_transactionally() {
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
        )
        .expect("ordinary SubRip converts");
        let output_before_error = output.len();
        let mut positioned = match sample(Codec::SubRip, SECOND as i64, SECOND, b"placed") {
            NormalizedSample::Subtitle(sample) => sample,
            _ => unreachable!(),
        };
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
        mux.finish(FinishReason::Final, &mut output)
            .expect("valid cue remains flushable");
        let body = str::from_utf8(segments(&output)[0].payload.as_bytes()).expect("body is text");
        assert!(body.contains("<b>Hello &amp; world</b>"));
        assert!(!body.contains("<font"));
    }

    #[test]
    fn superseded_discards_unsealed_tail_and_finish_is_idempotent() {
        let mut mux = mux(Codec::WebVtt, 2);
        let mut output = Vec::new();
        mux.push(sample(Codec::WebVtt, 0, SECOND, b"tail"), &mut output)
            .expect("cue is accepted");
        mux.finish(FinishReason::Superseded, &mut output)
            .expect("tail is discarded");
        mux.finish(FinishReason::Final, &mut output)
            .expect("second finish is harmless");

        assert_eq!(
            output
                .iter()
                .filter(|media| matches!(media, PackagedMedia::Segment(_)))
                .count(),
            0
        );
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
            sample(Codec::WebVtt, SECOND as i64, SECOND, b"valid"),
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
