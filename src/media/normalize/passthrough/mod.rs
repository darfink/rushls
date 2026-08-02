//! Track-local pass-through timestamp normalization.

use crate::{
    domain::{
        Appender, Codec, DiscoveredTrack, MediaParameters, TickDuration, TickTimestamp, Timebase,
        TimebaseProjection, TrackId,
    },
    media::{
        MediaNormalizer, NormalizeError, NormalizedSample, NormalizerFactory, PresentationPlan,
        StartedNormalizer, TimelineCalibration, TrackTimeline,
    },
    source::Packet,
};

mod audio;
mod subtitle;
mod video;

use audio::AudioNormalizer;
use subtitle::SubtitleNormalizer;
use video::VideoNormalizer;

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
            let projection = TimebaseProjection::new(track.timebase, output_timebase);

            let mut projected = track.clone();
            projected.timebase = output_timebase;
            projected.first_pts = track
                .first_pts
                .map(|pts| project_timestamp(projection, pts, track.id, "first PTS"))
                .transpose()?;
            tracks.push(projected);
            projected_timelines.push(TrackTimeline {
                track_id: track.id,
                timebase: output_timebase,
                origin_pts: project_timestamp(
                    projection,
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
            MediaParameters::Audio { .. } => {
                Ok(Self::Audio(AudioNormalizer::new(track, output_timebase)?))
            }
            MediaParameters::Subtitle => Ok(Self::Subtitle(SubtitleNormalizer::new(
                track,
                output_timebase,
            ))),
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
            Self::Audio(track) => track.track_id(),
            Self::Subtitle(track) => track.track_id(),
            Self::Video(track) => track.track_id(),
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
            Self::Audio(track) => {
                track.finish(out);
                Ok(())
            }
            Self::Subtitle(_) => Ok(()),
            Self::Video(track) => track.finish(out),
        }
    }
}

fn project_timestamp(
    projection: TimebaseProjection,
    value: TickTimestamp,
    track_id: TrackId,
    field: &'static str,
) -> Result<TickTimestamp, NormalizeError> {
    projection.timestamp(value).ok_or_else(|| {
        processing(format!(
            "{track_id} {field} overflows while changing timebase"
        ))
    })
}

fn project_interval(
    projection: TimebaseProjection,
    start: TickTimestamp,
    duration: TickDuration,
    track_id: TrackId,
) -> Result<(TickTimestamp, TickDuration), NormalizeError> {
    projection.interval(start, duration).ok_or_else(|| {
        processing(format!(
            "{track_id} access-unit interval cannot be represented in the output timebase"
        ))
    })
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
mod tests;
