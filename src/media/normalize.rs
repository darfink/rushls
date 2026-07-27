use std::collections::VecDeque;

use thiserror::Error;

use crate::{
    domain::{
        Appender, AudioTiming, AudioTrim, Codec, DiscoveredTrack, FrameRate, MediaParameters,
        Payload, RationalTickAccumulator, TickDuration, TickTimestamp, Timebase, TrackId,
    },
    source::Packet,
};

use super::{PresentationPlan, TimelineCalibration};

/// An access unit normalized onto its calibrated track-local timeline.
///
/// PTS, DTS, and duration use the corresponding
/// [`TrackTimeline::timebase`](super::TrackTimeline); they are not implicitly
/// expressed in a global or canonical timebase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NormalizedSample {
    Video(VideoSample),
    Audio(AudioSample),
    Subtitle(SubtitleSample),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoSample {
    pub track_id: TrackId,
    pub codec: Codec,
    pub pts: TickTimestamp,
    pub dts: TickTimestamp,
    pub duration: TickDuration,
    pub random_access: bool,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioSample {
    pub track_id: TrackId,
    pub codec: Codec,
    pub pts: TickTimestamp,
    pub duration: TickDuration,
    pub trim: AudioTrim,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtitleSample {
    pub track_id: TrackId,
    pub codec: Codec,
    pub pts: TickTimestamp,
    pub duration: TickDuration,
    pub payload: Payload,
}

/// The portion of an encoded access unit that belongs on the presentation
/// timeline after codec padding is removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentedTiming {
    pub start: TickTimestamp,
    pub duration: TickDuration,
}

impl PresentedTiming {
    pub fn end(self) -> Option<TickTimestamp> {
        self.start.checked_add_unsigned(self.duration)
    }
}

/// Computes effective presentation ranges in access-unit order.
///
/// FFmpeg attaches the complete leading skip count to one packet, even when
/// that count spans several decoded access units. Keeping the remainder here
/// lets timing consumers suppress those later units without rewriting the
/// packet-local [`AudioTrim`] that the muxer must pass back to FFmpeg.
///
/// The per-track constants are captured once, at construction. That is what
/// lets consumers hold a cursor rather than a whole [`DiscoveredTrack`], and it
/// retires the checks that used to re-establish on every access unit that the
/// supplied track was the cursor's track and carried audio parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentedTimingCursor {
    track_id: TrackId,
    /// Present only for audio: what a trim measured in decoded samples costs
    /// in this track's ticks. Absent for kinds that cannot be trimmed.
    audio: Option<AudioTrimScale>,
    pending_leading_audio_ticks: TickDuration,
}

/// Converts a decoded-sample trim into one track's tick domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AudioTrimScale {
    sample_rate: u32,
    timebase: Timebase,
}

impl AudioTrimScale {
    fn ticks(self, samples: u32) -> Result<TickDuration, SampleTimingError> {
        audio_samples_to_ticks_exact(samples, self.sample_rate, self.timebase)
    }
}

impl PresentedTimingCursor {
    /// Starts presentation accounting for one discovered track.
    pub fn for_track(track: &DiscoveredTrack) -> Self {
        Self {
            track_id: track.id,
            audio: match track.parameters {
                MediaParameters::Audio { sample_rate, .. } => Some(AudioTrimScale {
                    sample_rate: sample_rate.get(),
                    timebase: track.timebase,
                }),
                _ => None,
            },
            pending_leading_audio_ticks: 0,
        }
    }

    /// Returns the next access unit's effective range without changing the
    /// packet trim metadata retained on `sample`.
    ///
    /// Access units must arrive in the order the track produced them: the
    /// leading skip carried between calls is meaningless out of order.
    pub fn next(
        &mut self,
        sample: &NormalizedSample,
    ) -> Result<PresentedTiming, SampleTimingError> {
        if sample.track_id() != self.track_id {
            return Err(SampleTimingError::WrongTrack);
        }
        let (packet_leading, trailing) = match (sample, self.audio) {
            (NormalizedSample::Audio(sample), Some(scale)) => (
                scale.ticks(sample.trim.leading_samples)?,
                scale.ticks(sample.trim.trailing_samples)?,
            ),
            // An audio sample on a track discovered as another kind: its trim
            // has no scale to convert through, so its timing cannot be trusted.
            (NormalizedSample::Audio(_), None) => return Err(SampleTimingError::WrongTrack),
            _ => (0, 0),
        };
        let leading = self
            .pending_leading_audio_ticks
            .checked_add(packet_leading)
            .ok_or(SampleTimingError::AudioTrimOverflow)?;
        let applied_leading = leading.min(sample.duration());
        self.pending_leading_audio_ticks = leading - applied_leading;
        let duration = sample
            .duration()
            .checked_sub(applied_leading)
            .and_then(|duration| duration.checked_sub(trailing))
            .ok_or(SampleTimingError::TrimExceedsDuration)?;
        let start = sample
            .pts()
            .checked_add_unsigned(applied_leading)
            .ok_or(SampleTimingError::TimestampOverflow)?;
        Ok(PresentedTiming { start, duration })
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SampleTimingError {
    #[error("sample belongs to a different track")]
    WrongTrack,
    #[error("audio trim cannot be represented exactly in the track timebase")]
    InexactAudioTrim,
    #[error("accumulated audio trim overflowed")]
    AudioTrimOverflow,
    #[error("audio trim exceeds the encoded access-unit duration")]
    TrimExceedsDuration,
    #[error("presented sample timestamp overflowed")]
    TimestampOverflow,
}

impl NormalizedSample {
    pub fn track_id(&self) -> TrackId {
        match self {
            Self::Video(sample) => sample.track_id,
            Self::Audio(sample) => sample.track_id,
            Self::Subtitle(sample) => sample.track_id,
        }
    }

    pub fn random_access(&self) -> bool {
        match self {
            Self::Video(sample) => sample.random_access,
            // Every audio access unit and subtitle cue is independently enterable.
            Self::Audio(_) | Self::Subtitle(_) => true,
        }
    }

    pub fn pts(&self) -> TickTimestamp {
        match self {
            Self::Video(sample) => sample.pts,
            Self::Audio(sample) => sample.pts,
            Self::Subtitle(sample) => sample.pts,
        }
    }

    pub fn duration(&self) -> TickDuration {
        match self {
            Self::Video(sample) => sample.duration,
            Self::Audio(sample) => sample.duration,
            Self::Subtitle(sample) => sample.duration,
        }
    }

    pub fn payload_len(&self) -> usize {
        match self {
            Self::Video(sample) => sample.payload.len(),
            Self::Audio(sample) => sample.payload.len(),
            Self::Subtitle(sample) => sample.payload.len(),
        }
    }

    /// What holding this sample in a buffer actually costs.
    ///
    /// Charging only [`Self::payload_len`] would let an input with empty or
    /// tiny access units buffer without limit, because the per-sample struct —
    /// several timestamps, a codec, and a [`Payload`] handle — dominates at
    /// that size and would go uncounted. Callers enforcing a byte budget must
    /// use this rather than the payload length alone.
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>() + self.payload_len()
    }
}

fn audio_samples_to_ticks_exact(
    samples: u32,
    sample_rate: u32,
    timebase: crate::domain::Timebase,
) -> Result<TickDuration, SampleTimingError> {
    let numerator = u128::from(samples)
        .checked_mul(u128::from(timebase.den().get()))
        .ok_or(SampleTimingError::InexactAudioTrim)?;
    let denominator = u128::from(sample_rate)
        .checked_mul(u128::from(timebase.num().get()))
        .ok_or(SampleTimingError::InexactAudioTrim)?;
    if !numerator.is_multiple_of(denominator) {
        return Err(SampleTimingError::InexactAudioTrim);
    }
    TickDuration::try_from(numerator / denominator).map_err(|_| SampleTimingError::InexactAudioTrim)
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NormalizeError {
    #[error("cannot normalize the validated presentation: {0}")]
    InvalidPlan(Box<str>),
    #[error("media processing failed: {0}")]
    Processing(Box<str>),
}

/// Turns demultiplexed packets into calibrated access units.
///
/// Samples go to an [`Appender`] rather than into a generic sink type. That
/// single choice is what keeps this trait object-safe, and with it the entire
/// pipeline above: no type parameters propagate out of the hot path, and errors
/// stay flat instead of nesting one stage's failure inside the next one's. The
/// caller's buffer is reused across calls, so appending costs a move rather than
/// an allocation.
///
/// Expansion is bounded by
/// [`InputLimits::maximum_samples_per_batch`](crate::source::InputLimits::maximum_samples_per_batch),
/// measured across a whole batch of packets rather than per push, so holding
/// samples back for reordering and releasing them in a burst is fine.
pub trait MediaNormalizer: Send {
    fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError>;

    /// Flushes any access unit still held for reordering or duration inference.
    ///
    /// Called both when the input ends normally and when a session is cut
    /// short, so it must complete promptly from state already in hand: it is
    /// synchronous precisely so it cannot wait for input that will never come.
    /// Repeated calls must be harmless and produce nothing further.
    fn finish(&mut self, out: &mut dyn Appender<NormalizedSample>) -> Result<(), NormalizeError>;
}

/// A normalizer together with the representation it actually produces.
///
/// Timestamp normalization changes track timebases, so returning only the
/// processor would leave segmentation and muxing holding stale discovery
/// metadata. Startup is the atomic boundary where both become visible.
pub struct StartedNormalizer {
    pub normalizer: Box<dyn MediaNormalizer>,
    pub presentation: PresentationPlan,
    pub timeline: TimelineCalibration,
}

/// Builds a normalizer for one validated, calibrated presentation.
///
/// Takes no meters: volume is counted by the loop that drives the normalizer,
/// once per batch, which is both cheaper and closer to the truth than having
/// each stage report itself.
pub trait NormalizerFactory: Send + Sync {
    fn start(
        &self,
        presentation: &PresentationPlan,
        timeline: &TimelineCalibration,
    ) -> Result<StartedNormalizer, NormalizeError>;
}

/// Builds the production pass-through timing normalizer.
#[derive(Clone, Copy, Debug, Default)]
pub struct PassThroughNormalizerFactory;

impl NormalizerFactory for PassThroughNormalizerFactory {
    fn start(
        &self,
        presentation: &PresentationPlan,
        timeline: &TimelineCalibration,
    ) -> Result<StartedNormalizer, NormalizeError> {
        let mut tracks = Vec::with_capacity(presentation.tracks().len());
        let mut projected_timelines = Vec::with_capacity(presentation.tracks().len());
        let mut normalizers = Vec::with_capacity(presentation.tracks().len());

        for track in presentation.tracks() {
            let input_timeline = timeline
                .get(track.id)
                .ok_or_else(|| invalid_plan(format!("{} has no calibrated timeline", track.id)))?;
            if input_timeline.timebase != track.timebase {
                return Err(invalid_plan(format!(
                    "{} discovery and calibration timebases differ",
                    track.id
                )));
            }
            let output_timebase = normalized_timebase(track)?;
            let rescaler = TimestampRescaler::new(track.timebase, output_timebase);

            let mut projected = track.clone();
            projected.timebase = output_timebase;
            projected.first_pts = track
                .first_pts
                .map(|pts| rescaler.timestamp(pts, track.id, "first PTS"))
                .transpose()?;
            tracks.push(projected);
            projected_timelines.push(super::TrackTimeline {
                track_id: track.id,
                timebase: output_timebase,
                origin_pts: rescaler.timestamp(
                    input_timeline.origin_pts,
                    track.id,
                    "presentation origin",
                )?,
            });
            normalizers.push(TrackNormalizer::new(track, output_timebase)?);
        }

        let presentation = presentation
            .with_projected_tracks(tracks)
            .map_err(|error| invalid_plan(error.to_string()))?;
        let timeline = TimelineCalibration {
            timing_authority: timeline.timing_authority,
            tracks: projected_timelines,
        };
        Ok(StartedNormalizer {
            normalizer: Box::new(PassThroughNormalizer {
                tracks: normalizers,
                finished: false,
            }),
            presentation,
            timeline,
        })
    }
}

fn normalized_timebase(track: &DiscoveredTrack) -> Result<Timebase, NormalizeError> {
    match track.parameters {
        MediaParameters::Video { .. } => Ok(Timebase::hz90k()),
        // HLS WebVTT maps LOCAL cue time onto the 90 kHz MPEG timestamp
        // timeline. Cue text is rounded to milliseconds only when rendered.
        MediaParameters::Subtitle => Ok(Timebase::hz90k()),
        MediaParameters::Audio { sample_rate, .. } => {
            if track.codec == Codec::Opus && sample_rate.get() != 48_000 {
                return Err(invalid_plan(format!(
                    "{} declares Opus at {} Hz rather than its 48000 Hz timestamp clock",
                    track.id, sample_rate
                )));
            }
            Ok(Timebase::new(nz::u32!(1), sample_rate))
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TimestampRescaler {
    input: Timebase,
    output: Timebase,
}

impl TimestampRescaler {
    fn new(input: Timebase, output: Timebase) -> Self {
        Self { input, output }
    }

    fn timestamp(
        self,
        value: TickTimestamp,
        track_id: TrackId,
        field: &'static str,
    ) -> Result<TickTimestamp, NormalizeError> {
        self.input
            .checked_rescale_ticks(value, self.output)
            .ok_or_else(|| {
                processing(format!(
                    "{track_id} {field} overflows while rescaling {:?} to {:?}",
                    self.input, self.output
                ))
            })
    }

    /// Projects both ends of an interval, preventing cumulative drift from
    /// rounding every duration in isolation.
    fn interval(
        self,
        start: TickTimestamp,
        duration: TickDuration,
        track_id: TrackId,
    ) -> Result<(TickTimestamp, TickDuration), NormalizeError> {
        let end = start
            .checked_add_unsigned(duration)
            .ok_or_else(|| processing(format!("{track_id} timestamp interval overflows")))?;
        let projected_start = self.timestamp(start, track_id, "timestamp")?;
        let projected_end = self.timestamp(end, track_id, "timestamp")?;
        let projected_duration = projected_end
            .checked_sub(projected_start)
            .and_then(|duration| TickDuration::try_from(duration).ok())
            .filter(|duration| *duration > 0)
            .ok_or_else(|| {
                processing(format!(
                    "{track_id} access-unit duration disappears in the output timebase"
                ))
            })?;
        Ok((projected_start, projected_duration))
    }

    fn one_input_tick_ceil(self) -> TickDuration {
        let numerator = u128::from(self.input.num().get()) * u128::from(self.output.den().get());
        let denominator = u128::from(self.input.den().get()) * u128::from(self.output.num().get());
        let ticks = numerator / denominator + u128::from(!numerator.is_multiple_of(denominator));
        TickDuration::try_from(ticks).unwrap_or(TickDuration::MAX)
    }
}

struct PassThroughNormalizer {
    tracks: Vec<TrackNormalizer>,
    finished: bool,
}

impl MediaNormalizer for PassThroughNormalizer {
    fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        if self.finished {
            return Err(processing("a packet arrived after normalization finished"));
        }
        let track = self
            .tracks
            .iter_mut()
            .find(|track| track.track_id() == packet.track_id)
            .ok_or_else(|| processing(format!("packet names unknown {}", packet.track_id)))?;
        track.push(packet, out)
    }

    fn finish(&mut self, out: &mut dyn Appender<NormalizedSample>) -> Result<(), NormalizeError> {
        if self.finished {
            return Ok(());
        }
        // Marking the aggregate first makes a retry harmless even if one track
        // reports a bad tail; earlier tracks may already have appended output.
        self.finished = true;
        for track in &mut self.tracks {
            track.finish(out)?;
        }
        Ok(())
    }
}

enum TrackNormalizer {
    Audio(AudioNormalizer),
    Subtitle(SubtitleNormalizer),
    Video(Box<VideoNormalizer>),
}

impl TrackNormalizer {
    fn new(track: &DiscoveredTrack, output_timebase: Timebase) -> Result<Self, NormalizeError> {
        match track.parameters {
            MediaParameters::Audio {
                sample_rate,
                frame_size,
                timing,
                ..
            } => Ok(Self::Audio(AudioNormalizer {
                track_id: track.id,
                codec: track.codec,
                rescaler: TimestampRescaler::new(track.timebase, output_timebase),
                audible_start: track.first_pts.ok_or_else(|| {
                    invalid_plan(format!("{} has no audible start timestamp", track.id))
                })?,
                frame_size: frame_size.map(|size| u64::from(size.get())),
                timing,
                next_pts: None,
                first: true,
                sample_rate: sample_rate.get(),
            })),
            MediaParameters::Subtitle => Ok(Self::Subtitle(SubtitleNormalizer {
                track_id: track.id,
                codec: track.codec,
                rescaler: TimestampRescaler::new(track.timebase, output_timebase),
            })),
            MediaParameters::Video {
                frame_rate,
                video_delay,
                ..
            } => Ok(Self::Video(Box::new(VideoNormalizer::new(
                track,
                output_timebase,
                frame_rate,
                video_delay,
            )?))),
        }
    }

    fn track_id(&self) -> TrackId {
        match self {
            Self::Audio(track) => track.track_id,
            Self::Subtitle(track) => track.track_id,
            Self::Video(track) => track.track_id,
        }
    }

    fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        match self {
            Self::Audio(track) => track.push(packet, out),
            Self::Subtitle(track) => track.push(packet, out),
            Self::Video(track) => track.push(packet, out),
        }
    }

    fn finish(&mut self, out: &mut dyn Appender<NormalizedSample>) -> Result<(), NormalizeError> {
        match self {
            Self::Audio(_) | Self::Subtitle(_) => Ok(()),
            Self::Video(track) => track.finish(out),
        }
    }
}

struct AudioNormalizer {
    track_id: TrackId,
    codec: Codec,
    rescaler: TimestampRescaler,
    audible_start: TickTimestamp,
    frame_size: Option<TickDuration>,
    timing: AudioTiming,
    next_pts: Option<TickTimestamp>,
    first: bool,
    sample_rate: u32,
}

impl AudioNormalizer {
    fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        let duration = self.duration(&packet)?;
        let supplied_pts = packet
            .pts
            .map(|pts| self.rescaler.timestamp(pts, self.track_id, "audio PTS"))
            .transpose()?;
        let pts = if self.first {
            let audible_start =
                self.rescaler
                    .timestamp(self.audible_start, self.track_id, "audible start")?;
            let leading = if packet.audio_trim.leading_samples > 0 {
                TickDuration::from(packet.audio_trim.leading_samples)
            } else {
                TickDuration::from(self.timing.initial_padding_samples)
            };
            let encoded_start = audible_start
                .checked_sub_unsigned(leading)
                .ok_or_else(|| processing(format!("{} audio priming overflows", self.track_id)))?;
            if let Some(supplied) = supplied_pts {
                self.ensure_near(supplied, encoded_start, "first audio PTS")?;
            }
            self.first = false;
            encoded_start
        } else {
            let expected = self.next_pts.ok_or_else(|| {
                processing(format!(
                    "{} audio clock has no next timestamp",
                    self.track_id
                ))
            })?;
            if let Some(supplied) = supplied_pts {
                self.ensure_near(supplied, expected, "audio PTS")?;
            }
            expected
        };
        if let Some(dts) = packet.dts {
            let dts = self.rescaler.timestamp(dts, self.track_id, "audio DTS")?;
            self.ensure_near(dts, pts, "audio DTS")?;
        }
        self.next_pts = Some(
            pts.checked_add_unsigned(duration)
                .ok_or_else(|| processing(format!("{} audio clock overflows", self.track_id)))?,
        );
        out.push(NormalizedSample::Audio(AudioSample {
            track_id: self.track_id,
            codec: self.codec,
            pts,
            duration,
            trim: packet.audio_trim,
            payload: packet.payload,
        }));
        Ok(())
    }

    fn duration(&self, packet: &Packet) -> Result<TickDuration, NormalizeError> {
        if packet.duration.is_some_and(|duration| duration < 0) {
            return Err(processing(format!(
                "{} audio duration is negative",
                self.track_id
            )));
        }
        let supplied = packet
            .duration
            .filter(|duration| *duration > 0)
            .map(|duration| {
                let duration = TickDuration::try_from(duration).map_err(|_| {
                    processing(format!("{} audio duration is invalid", self.track_id))
                })?;
                let start = packet.pts.unwrap_or(0);
                self.rescaler
                    .interval(start, duration, self.track_id)
                    .map(|(_, duration)| duration)
            })
            .transpose()?;
        if let Some(frame_size) = self.frame_size {
            if let Some(supplied) = supplied {
                self.ensure_duration_near(supplied, frame_size, "audio packet duration")?;
            }
            return Ok(frame_size);
        }
        supplied.ok_or_else(|| {
            processing(format!(
                "{} has neither a fixed audio frame size nor a packet duration",
                self.track_id
            ))
        })
    }

    fn ensure_near(
        &self,
        actual: TickTimestamp,
        expected: TickTimestamp,
        field: &'static str,
    ) -> Result<(), NormalizeError> {
        let distance = i128::from(actual)
            .checked_sub(i128::from(expected))
            .map(i128::unsigned_abs)
            .unwrap_or(u128::MAX);
        let tolerance = u128::from(self.rescaler.one_input_tick_ceil());
        if distance > tolerance {
            return Err(processing(format!(
                "{} {field} discontinuity: expected {expected}, got {actual}",
                self.track_id
            )));
        }
        Ok(())
    }

    fn ensure_duration_near(
        &self,
        actual: TickDuration,
        expected: TickDuration,
        field: &'static str,
    ) -> Result<(), NormalizeError> {
        let distance = actual.abs_diff(expected);
        if distance > self.rescaler.one_input_tick_ceil() {
            return Err(processing(format!(
                "{} {field} disagrees with its {} Hz sample clock",
                self.track_id, self.sample_rate
            )));
        }
        Ok(())
    }
}

struct SubtitleNormalizer {
    track_id: TrackId,
    codec: Codec,
    rescaler: TimestampRescaler,
}

impl SubtitleNormalizer {
    fn push(
        &self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        if !packet.audio_trim.is_empty() {
            return Err(processing(format!(
                "{} subtitle cue carries audio trim metadata",
                self.track_id
            )));
        }
        let pts = packet
            .pts
            .ok_or_else(|| processing(format!("{} subtitle cue has no PTS", self.track_id)))?;
        let duration = positive_duration(packet.duration, self.track_id, "subtitle duration")?;
        let (pts, duration) = self.rescaler.interval(pts, duration, self.track_id)?;
        out.push(NormalizedSample::Subtitle(SubtitleSample {
            track_id: self.track_id,
            codec: self.codec,
            pts,
            duration,
            payload: packet.payload,
        }));
        Ok(())
    }
}

const MAXIMUM_VIDEO_REORDER_DEPTH: u32 = 64;

struct VideoNormalizer {
    track_id: TrackId,
    codec: Codec,
    rescaler: TimestampRescaler,
    declared_clock: Option<RationalTickAccumulator>,
    video_delay: usize,
    held: Option<Packet>,
    pending_clock: VecDeque<TimedVideoPacket>,
    next_dts: Option<TickTimestamp>,
    last_duration: Option<TickDuration>,
    finished: bool,
}

struct TimedVideoPacket {
    packet: Packet,
    pts: TickTimestamp,
    duration: TickDuration,
}

impl VideoNormalizer {
    fn new(
        track: &DiscoveredTrack,
        output_timebase: Timebase,
        frame_rate: Option<FrameRate>,
        video_delay: u32,
    ) -> Result<Self, NormalizeError> {
        if video_delay > MAXIMUM_VIDEO_REORDER_DEPTH {
            return Err(invalid_plan(format!(
                "{} declares video reorder depth {video_delay}, above {MAXIMUM_VIDEO_REORDER_DEPTH}",
                track.id
            )));
        }
        Ok(Self {
            track_id: track.id,
            codec: track.codec,
            rescaler: TimestampRescaler::new(track.timebase, output_timebase),
            declared_clock: frame_rate
                .map(|rate| video_cadence(output_timebase, rate, track.id))
                .transpose()?,
            video_delay: video_delay as usize,
            held: None,
            pending_clock: VecDeque::with_capacity(video_delay as usize + 1),
            next_dts: None,
            last_duration: None,
            finished: false,
        })
    }

    fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        if self.finished {
            return Err(processing(format!(
                "{} video packet arrived after finish",
                self.track_id
            )));
        }
        if packet.pts.is_none() {
            return Err(processing(format!(
                "{} video packet has no PTS",
                self.track_id
            )));
        }
        if !packet.audio_trim.is_empty() {
            return Err(processing(format!(
                "{} video packet carries audio trim metadata",
                self.track_id
            )));
        }
        if let Some(previous) = self.held.take() {
            let duration = self.resolve_duration(&previous, Some(&packet))?;
            self.accept_timed(previous, duration, out)?;
        }
        self.held = Some(packet);
        Ok(())
    }

    fn finish(&mut self, out: &mut dyn Appender<NormalizedSample>) -> Result<(), NormalizeError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if let Some(packet) = self.held.take() {
            let duration = self.resolve_duration(&packet, None)?;
            self.accept_timed(packet, duration, out)?;
        }
        if !self.pending_clock.is_empty() {
            self.start_missing_clock(out)?;
        }
        Ok(())
    }

    fn resolve_duration(
        &mut self,
        packet: &Packet,
        next: Option<&Packet>,
    ) -> Result<TickDuration, NormalizeError> {
        if packet.duration.is_some_and(|duration| duration < 0) {
            return Err(processing(format!(
                "{} video duration is negative",
                self.track_id
            )));
        }
        // Advance the declared cadence for every access unit, including units
        // whose observed duration wins. If a later unit needs the fallback,
        // its fractional phase still corresponds to its real frame index.
        let declared = self
            .declared_clock
            .as_mut()
            .map(|clock| {
                clock
                    .advance(1)
                    .filter(|duration| *duration > 0)
                    .ok_or_else(|| {
                        processing(format!(
                            "{} frame duration cannot be represented",
                            self.track_id
                        ))
                    })
            })
            .transpose()?;
        let source_interval = packet
            .duration
            .filter(|duration| *duration > 0)
            .map(|duration| {
                TickDuration::try_from(duration)
                    .map_err(|_| processing(format!("{} video duration is invalid", self.track_id)))
            })
            .transpose()?
            .map(|duration| (packet.dts.or(packet.pts).unwrap_or(0), duration))
            .or_else(|| {
                next.and_then(|next| match (packet.dts, next.dts) {
                    (Some(current), Some(next)) => next
                        .checked_sub(current)
                        .and_then(|duration| TickDuration::try_from(duration).ok())
                        .filter(|duration| *duration > 0)
                        .map(|duration| (current, duration)),
                    _ => None,
                })
            })
            .or_else(|| {
                (self.video_delay == 0).then_some(()).and_then(|()| {
                    next.and_then(|next| {
                        next.pts?
                            .checked_sub(packet.pts?)
                            .and_then(|duration| TickDuration::try_from(duration).ok())
                            .filter(|duration| *duration > 0)
                            .map(|duration| {
                                (packet.pts.expect("video PTS was validated"), duration)
                            })
                    })
                })
            });
        let observed = source_interval
            .map(|(start, duration)| {
                self.rescaler
                    .interval(start, duration, self.track_id)
                    .map(|(_, duration)| duration)
            })
            .transpose()?;
        let duration = observed
            .or(declared)
            .or(self.last_duration)
            .ok_or_else(|| {
                processing(format!(
                    "{} video duration cannot be derived at end of input",
                    self.track_id
                ))
            })?;
        self.last_duration = Some(duration);
        Ok(duration)
    }

    fn accept_timed(
        &mut self,
        packet: Packet,
        duration: TickDuration,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        let pts = self.rescaler.timestamp(
            packet.pts.expect("validated before the packet was held"),
            self.track_id,
            "video PTS",
        )?;
        let dts = packet
            .dts
            .map(|dts| self.rescaler.timestamp(dts, self.track_id, "video DTS"))
            .transpose()?;
        match (dts, self.next_dts) {
            (Some(anchor), Some(expected)) => {
                if anchor != expected {
                    return Err(processing(format!(
                        "{} video DTS discontinuity: expected {expected}, got {anchor}",
                        self.track_id
                    )));
                }
                self.emit(packet, pts, anchor, duration, out)?;
            }
            (Some(anchor), None) => {
                let pending_duration = self
                    .pending_clock
                    .iter()
                    .try_fold(0_u64, |sum, packet| sum.checked_add(packet.duration));
                let mut cursor = anchor
                    .checked_sub_unsigned(pending_duration.ok_or_else(|| {
                        processing(format!(
                            "{} pending video duration overflows",
                            self.track_id
                        ))
                    })?)
                    .ok_or_else(|| {
                        processing(format!("{} synthesized video DTS overflows", self.track_id))
                    })?;
                while let Some(pending) = self.pending_clock.pop_front() {
                    self.emit(pending.packet, pending.pts, cursor, pending.duration, out)?;
                    cursor = self.next_dts.expect("emission establishes the next DTS");
                }
                if cursor != anchor {
                    return Err(processing(format!(
                        "{} explicit video DTS does not follow buffered access units",
                        self.track_id
                    )));
                }
                self.emit(packet, pts, anchor, duration, out)?;
            }
            (None, Some(dts)) => self.emit(packet, pts, dts, duration, out)?,
            (None, None) => {
                self.pending_clock.push_back(TimedVideoPacket {
                    packet,
                    pts,
                    duration,
                });
                if self.pending_clock.len() > self.video_delay {
                    self.start_missing_clock(out)?;
                }
            }
        }
        Ok(())
    }

    fn start_missing_clock(
        &mut self,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        let first_pts = self
            .pending_clock
            .front()
            .map(|packet| packet.pts)
            .ok_or_else(|| processing(format!("{} video clock has no packet", self.track_id)))?;
        let delay = self
            .pending_clock
            .iter()
            .take(self.video_delay)
            .try_fold(0_u64, |sum, packet| sum.checked_add(packet.duration));
        let mut cursor = first_pts
            .checked_sub_unsigned(delay.ok_or_else(|| {
                processing(format!("{} video reorder delay overflows", self.track_id))
            })?)
            .ok_or_else(|| {
                processing(format!("{} synthesized video DTS overflows", self.track_id))
            })?;
        while let Some(pending) = self.pending_clock.pop_front() {
            self.emit(pending.packet, pending.pts, cursor, pending.duration, out)?;
            cursor = self.next_dts.expect("emission establishes the next DTS");
        }
        Ok(())
    }

    fn emit(
        &mut self,
        packet: Packet,
        pts: TickTimestamp,
        dts: TickTimestamp,
        duration: TickDuration,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        let next_dts = dts.checked_add_unsigned(duration).ok_or_else(|| {
            processing(format!("{} synthesized video DTS overflows", self.track_id))
        })?;
        self.next_dts = Some(next_dts);
        out.push(NormalizedSample::Video(VideoSample {
            track_id: self.track_id,
            codec: self.codec,
            pts,
            dts,
            duration,
            random_access: packet.random_access,
            payload: packet.payload,
        }));
        Ok(())
    }
}

fn video_cadence(
    timebase: Timebase,
    frame_rate: FrameRate,
    track_id: TrackId,
) -> Result<RationalTickAccumulator, NormalizeError> {
    let frame_period = Timebase::new(frame_rate.denominator(), frame_rate.numerator());
    let clock = RationalTickAccumulator::requantize_to_ticks(frame_period, timebase)
        .ok_or_else(|| invalid_plan(format!("{track_id} frame cadence overflows")))?;
    let mut probe = clock;
    if probe.advance(1).is_none_or(|duration| duration == 0) {
        return Err(invalid_plan(format!(
            "{track_id} frame duration is not representable in its output timebase"
        )));
    }
    Ok(clock)
}

fn positive_duration(
    duration: Option<i64>,
    track_id: TrackId,
    field: &'static str,
) -> Result<TickDuration, NormalizeError> {
    duration
        .filter(|duration| *duration > 0)
        .and_then(|duration| TickDuration::try_from(duration).ok())
        .ok_or_else(|| processing(format!("{track_id} {field} is missing or not positive")))
}

fn invalid_plan(message: impl Into<Box<str>>) -> NormalizeError {
    NormalizeError::InvalidPlan(message.into())
}

fn processing(message: impl Into<Box<str>>) -> NormalizeError {
    NormalizeError::Processing(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{AudioTiming, MediaKind, MediaParameters, fixtures::TrackBuilder},
        media::{calibrate, fixtures::presentation},
    };

    fn packet(track_id: u32, pts: Option<i64>, dts: Option<i64>, duration: Option<i64>) -> Packet {
        Packet {
            track_id: TrackId(track_id),
            pts,
            dts,
            duration,
            random_access: false,
            audio_trim: AudioTrim::default(),
            payload: Payload::default(),
        }
    }

    fn start(
        tracks: Vec<DiscoveredTrack>,
    ) -> (StartedNormalizer, PresentationPlan, TimelineCalibration) {
        let input = presentation(tracks);
        let timeline = calibrate(&input).expect("input timeline calibrates");
        let started = PassThroughNormalizerFactory
            .start(&input, &timeline)
            .expect("normalization starts");
        (started, input, timeline)
    }

    #[test]
    fn audio_and_subtitles_are_intrinsically_random_access() {
        let audio = NormalizedSample::Audio(AudioSample {
            track_id: TrackId(1),
            codec: Codec::Aac,
            pts: 90_000,
            duration: 1_920,
            trim: AudioTrim::default(),
            payload: Payload::default(),
        });
        let subtitle = NormalizedSample::Subtitle(SubtitleSample {
            track_id: TrackId(2),
            codec: Codec::WebVtt,
            pts: 90_000,
            duration: 90_000,
            payload: Payload::default(),
        });

        assert!(audio.random_access());
        assert!(subtitle.random_access());
    }

    #[test]
    fn video_preserves_its_random_access_marker() {
        let video = NormalizedSample::Video(VideoSample {
            track_id: TrackId(0),
            codec: Codec::H264,
            pts: 90_000,
            dts: 90_000,
            duration: 3_000,
            random_access: false,
            payload: Payload::default(),
        });

        assert!(!video.random_access());
    }

    #[test]
    fn presented_audio_timing_excludes_priming_and_trailing_padding() {
        let track = TrackBuilder::new(1, MediaKind::Audio)
            .timebase(crate::domain::Timebase::new(nz::u32!(1), nz::u32!(48_000)))
            .build();
        let priming = NormalizedSample::Audio(AudioSample {
            track_id: TrackId(1),
            codec: Codec::Aac,
            pts: -1_024,
            duration: 1_024,
            trim: AudioTrim {
                leading_samples: 1_024,
                trailing_samples: 0,
            },
            payload: Payload::default(),
        });
        let tail = NormalizedSample::Audio(AudioSample {
            track_id: TrackId(1),
            codec: Codec::Aac,
            pts: 0,
            duration: 1_024,
            trim: AudioTrim {
                leading_samples: 0,
                trailing_samples: 24,
            },
            payload: Payload::default(),
        });
        let mut timing = PresentedTimingCursor::for_track(&track);

        assert_eq!(
            timing.next(&priming),
            Ok(PresentedTiming {
                start: 0,
                duration: 0,
            })
        );
        assert_eq!(
            timing.next(&tail),
            Ok(PresentedTiming {
                start: 0,
                duration: 1_000,
            })
        );
    }

    #[test]
    fn leading_trim_spans_access_units_without_rewriting_packet_metadata() {
        let track = TrackBuilder::new(1, MediaKind::Audio)
            .timebase(crate::domain::Timebase::new(nz::u32!(1), nz::u32!(48_000)))
            .build();
        let original_trim = AudioTrim {
            leading_samples: 2_112,
            trailing_samples: 0,
        };
        let samples = [
            NormalizedSample::Audio(AudioSample {
                track_id: track.id,
                codec: Codec::Aac,
                pts: -2_112,
                duration: 1_024,
                trim: original_trim,
                payload: Payload::default(),
            }),
            NormalizedSample::Audio(AudioSample {
                track_id: track.id,
                codec: Codec::Aac,
                pts: -1_088,
                duration: 1_024,
                trim: AudioTrim::default(),
                payload: Payload::default(),
            }),
            NormalizedSample::Audio(AudioSample {
                track_id: track.id,
                codec: Codec::Aac,
                pts: -64,
                duration: 1_024,
                trim: AudioTrim::default(),
                payload: Payload::default(),
            }),
        ];
        let mut timing = PresentedTimingCursor::for_track(&track);

        assert_eq!(
            timing.next(&samples[0]),
            Ok(PresentedTiming {
                start: -1_088,
                duration: 0,
            })
        );
        assert_eq!(
            timing.next(&samples[1]),
            Ok(PresentedTiming {
                start: -64,
                duration: 0,
            })
        );
        assert_eq!(
            timing.next(&samples[2]),
            Ok(PresentedTiming {
                start: 0,
                duration: 960,
            })
        );
        assert_eq!(
            match &samples[0] {
                NormalizedSample::Audio(sample) => sample.trim,
                _ => unreachable!("fixture is audio"),
            },
            original_trim,
            "presentation accounting must not rewrite FFmpeg packet side data"
        );
    }

    #[test]
    fn startup_projects_track_metadata_and_the_shared_origin_together() {
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
            .first_pts(Some(-22))
            .build();
        let video = TrackBuilder::new(1, MediaKind::Video)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(10_000)))
            .first_pts(Some(0))
            .build();

        let (started, _, _) = start(vec![audio, video]);
        let audio = started
            .presentation
            .catalog()
            .get(TrackId(0))
            .expect("audio track exists");
        let video = started
            .presentation
            .catalog()
            .get(TrackId(1))
            .expect("video track exists");

        assert_eq!(
            (audio.timebase, audio.first_pts),
            (Timebase::new(nz::u32!(1), nz::u32!(48_000)), Some(-1_056))
        );
        assert_eq!(
            (video.timebase, video.first_pts),
            (Timebase::hz90k(), Some(0))
        );
        assert_eq!(
            started
                .timeline
                .get(TrackId(0))
                .map(|track| track.origin_pts),
            Some(-1_056)
        );
        assert_eq!(
            started
                .timeline
                .get(TrackId(1))
                .map(|track| track.origin_pts),
            Some(-1_980)
        );
    }

    #[test]
    fn audio_clock_preserves_exact_priming_despite_coarse_container_timestamps() {
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
            .first_pts(Some(0))
            .parameters(MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(2),
                frame_size: Some(nz::u32!(1_024)),
                bit_depth: None,
                timing: AudioTiming {
                    initial_padding_samples: 1_024,
                    ..AudioTiming::default()
                },
            })
            .build();
        let (mut started, _, _) = start(vec![audio]);
        let mut first = packet(0, Some(-21), Some(-21), Some(21));
        first.audio_trim.leading_samples = 1_024;
        let second = packet(0, Some(0), Some(0), Some(21));
        let mut output = Vec::new();

        started
            .normalizer
            .push(first, &mut output)
            .expect("priming packet normalizes");
        started
            .normalizer
            .push(second, &mut output)
            .expect("audible packet normalizes");

        let samples: Vec<_> = output
            .iter()
            .map(|sample| match sample {
                NormalizedSample::Audio(sample) => {
                    (sample.pts, sample.duration, sample.trim.leading_samples)
                }
                _ => unreachable!("audio input only produces audio"),
            })
            .collect();
        assert_eq!(samples, [(-1_024, 1_024, 1_024), (0, 1_024, 0)]);
    }

    #[test]
    fn audio_clock_rejects_a_real_timestamp_discontinuity() {
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
            .parameters(MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(2),
                frame_size: Some(nz::u32!(1_024)),
                bit_depth: None,
                timing: AudioTiming::default(),
            })
            .build();
        let (mut started, _, _) = start(vec![audio]);
        let mut output = Vec::new();
        started
            .normalizer
            .push(packet(0, Some(0), Some(0), Some(21)), &mut output)
            .expect("first packet anchors the clock");

        let error = started
            .normalizer
            .push(packet(0, Some(100), Some(100), Some(21)), &mut output)
            .expect_err("a 100 ms jump is not timestamp quantization");
        assert!(error.to_string().contains("discontinuity"));
    }

    #[test]
    fn video_derives_variable_durations_and_synthesizes_missing_dts() {
        let video = TrackBuilder::new(0, MediaKind::Video)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(10_000)))
            .parameters(MediaParameters::Video {
                width: nz::u32!(1920),
                height: nz::u32!(1080),
                frame_rate: None,
                video_delay: 0,
            })
            .build();
        let (mut started, _, _) = start(vec![video]);
        let mut output = Vec::new();
        for pts in [0, 333, 667] {
            started
                .normalizer
                .push(packet(0, Some(pts), None, None), &mut output)
                .expect("video packet is accepted");
        }
        started
            .normalizer
            .finish(&mut output)
            .expect("video tail flushes");

        let timing: Vec<_> = output
            .iter()
            .map(|sample| match sample {
                NormalizedSample::Video(sample) => (sample.pts, sample.dts, sample.duration),
                _ => unreachable!("video input only produces video"),
            })
            .collect();
        assert_eq!(
            timing,
            [(0, 0, 2_997), (2_997, 2_997, 3_006), (6_003, 6_003, 3_006)]
        );
    }

    #[test]
    fn reordered_video_uses_declared_delay_to_synthesize_negative_dts() {
        let video = TrackBuilder::new(0, MediaKind::Video)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
            .parameters(MediaParameters::Video {
                width: nz::u32!(1920),
                height: nz::u32!(1080),
                frame_rate: Some(FrameRate::new(nz::u32!(25), nz::u32!(1))),
                video_delay: 2,
            })
            .build();
        let (mut started, _, _) = start(vec![video]);
        let mut output = Vec::new();
        for pts in [0, 80, 40, 120] {
            started
                .normalizer
                .push(packet(0, Some(pts), None, None), &mut output)
                .expect("reordered packet is accepted");
        }
        started
            .normalizer
            .finish(&mut output)
            .expect("reordered tail flushes");

        let timing: Vec<_> = output
            .iter()
            .map(|sample| match sample {
                NormalizedSample::Video(sample) => (sample.pts, sample.dts, sample.duration),
                _ => unreachable!("video input only produces video"),
            })
            .collect();
        assert_eq!(
            timing,
            [
                (0, -7_200, 3_600),
                (7_200, -3_600, 3_600),
                (3_600, 0, 3_600),
                (10_800, 3_600, 3_600)
            ]
        );
    }

    #[test]
    fn fractional_declared_video_cadence_does_not_accumulate_rounding_drift() {
        let video = TrackBuilder::new(0, MediaKind::Video)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
            .parameters(MediaParameters::Video {
                width: nz::u32!(1920),
                height: nz::u32!(1080),
                frame_rate: Some(FrameRate::new(nz::u32!(24_000), nz::u32!(1_001))),
                video_delay: 2,
            })
            .build();
        let (mut started, _, _) = start(vec![video]);
        let mut output = Vec::new();
        for pts in [0, 83, 42, 125, 208, 167, 250, 292] {
            started
                .normalizer
                .push(packet(0, Some(pts), None, None), &mut output)
                .expect("fractional-cadence packet is accepted");
        }
        started
            .normalizer
            .finish(&mut output)
            .expect("fractional-cadence tail flushes");

        let video: Vec<_> = output
            .iter()
            .map(|sample| match sample {
                NormalizedSample::Video(sample) => sample,
                _ => unreachable!("video input only produces video"),
            })
            .collect();
        assert_eq!(
            video
                .iter()
                .map(|sample| sample.duration)
                .sum::<TickDuration>(),
            30_030,
            "eight 24000/1001 frames are exactly 30030 ticks at 90 kHz"
        );
        assert!(
            video
                .windows(2)
                .all(|pair| pair[0].dts.checked_add_unsigned(pair[0].duration)
                    == Some(pair[1].dts))
        );
    }

    #[test]
    fn subtitles_reuse_interval_rescaling_for_start_and_duration() {
        let subtitle = TrackBuilder::new(0, MediaKind::Subtitle)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(10_000)))
            .build();
        // Validation requires a presentable audio or video track; the
        // normalizer still routes each track independently once admitted.
        let video = TrackBuilder::new(1, MediaKind::Video).build();
        let (mut started, _, _) = start(vec![subtitle, video]);
        let mut output = Vec::new();
        assert_eq!(
            started
                .presentation
                .catalog()
                .get(TrackId(0))
                .map(|track| track.timebase),
            Some(Timebase::hz90k())
        );

        started
            .normalizer
            .push(packet(0, Some(12_345), None, Some(6_789)), &mut output)
            .expect("subtitle cue normalizes");

        assert_eq!(
            output,
            [NormalizedSample::Subtitle(SubtitleSample {
                track_id: TrackId(0),
                codec: Codec::WebVtt,
                pts: 111_105,
                duration: 61_101,
                payload: Payload::default(),
            })]
        );
    }

    #[test]
    fn finish_is_idempotent_after_releasing_a_held_video_packet() {
        let (mut started, _, _) = start(vec![TrackBuilder::new(0, MediaKind::Video).build()]);
        let mut output = Vec::new();
        started
            .normalizer
            .push(packet(0, Some(0), None, Some(3_000)), &mut output)
            .expect("packet is held");

        started
            .normalizer
            .finish(&mut output)
            .expect("first finish flushes");
        started
            .normalizer
            .finish(&mut output)
            .expect("second finish is harmless");

        assert_eq!(output.len(), 1);
    }
}
