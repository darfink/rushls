use thiserror::Error;

use crate::{
    admission::StreamPolicy,
    domain::{
        Codec, DiscoveredTrack, FrameRate, MediaKind, MediaParameters, TrackCatalog, TrackCounts,
        TrackId,
    },
};

/// A discovered presentation that passed policy validation.
///
/// Kept distinct from [`TrackCatalog`] so downstream calibration, segmentation,
/// and muxing cannot accidentally accept unvalidated discovery metadata.
/// Rendition relationships belong here as those capabilities arrive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationPlan {
    tracks: TrackCatalog,
}

impl PresentationPlan {
    pub fn tracks(&self) -> &[DiscoveredTrack] {
        self.tracks.tracks()
    }

    pub fn catalog(&self) -> &TrackCatalog {
        &self.tracks
    }

    pub fn counts(&self) -> TrackCounts {
        self.tracks.counts()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ValidationError {
    #[error("{track_id} uses {codec:?}, which this stream may not publish")]
    UnsupportedCodec { track_id: TrackId, codec: Codec },
    #[error("the stream published {found} {kind:?} tracks but may publish at most {limit}")]
    TrackLimitExceeded {
        kind: MediaKind,
        limit: usize,
        found: usize,
    },
    #[error("the stream published no audio or video track")]
    NoPresentableTrack,
    #[error("{track_id} declares video width {found}, above the permitted maximum {maximum}")]
    VideoWidthExceeded {
        track_id: TrackId,
        found: u32,
        maximum: u32,
    },
    #[error("{track_id} declares video height {found}, above the permitted maximum {maximum}")]
    VideoHeightExceeded {
        track_id: TrackId,
        found: u32,
        maximum: u32,
    },
    #[error("{track_id} declares a frame rate above the permitted maximum")]
    VideoFrameRateExceeded {
        track_id: TrackId,
        found: FrameRate,
        maximum: FrameRate,
    },
    #[error("{track_id} declares audio sample rate {found}, above the permitted maximum {maximum}")]
    AudioSampleRateExceeded {
        track_id: TrackId,
        found: u32,
        maximum: u32,
    },
    #[error("{track_id} declares {found} audio channels, above the permitted maximum {maximum}")]
    AudioChannelsExceeded {
        track_id: TrackId,
        found: u16,
        maximum: u16,
    },
}

/// Accepts a discovered track set for presentation, or explains the rejection.
pub fn validate(
    tracks: &TrackCatalog,
    policy: &StreamPolicy,
) -> Result<PresentationPlan, ValidationError> {
    let counts = tracks.counts();
    check_limit(MediaKind::Audio, counts.audio, policy.maximum_audio_tracks)?;
    check_limit(
        MediaKind::Subtitle,
        counts.subtitle,
        policy.maximum_subtitle_tracks,
    )?;
    check_limit(MediaKind::Video, counts.video, policy.maximum_video_tracks)?;
    if counts.audio == 0 && counts.video == 0 {
        return Err(ValidationError::NoPresentableTrack);
    }

    for track in tracks.tracks() {
        let accepted = match track.kind() {
            MediaKind::Audio => policy.accepted_audio_codecs.contains(&track.codec),
            MediaKind::Video => policy.accepted_video_codecs.contains(&track.codec),
            // Subtitles are passed through as-is; the muxer decides whether it
            // can carry the cue format.
            MediaKind::Subtitle => true,
        };
        if !accepted {
            return Err(ValidationError::UnsupportedCodec {
                track_id: track.id,
                codec: track.codec,
            });
        }
        validate_media(track, policy)?;
    }

    Ok(PresentationPlan {
        tracks: tracks.clone(),
    })
}

fn validate_media(track: &DiscoveredTrack, policy: &StreamPolicy) -> Result<(), ValidationError> {
    match track.parameters {
        MediaParameters::Video {
            width,
            height,
            frame_rate,
            ..
        } => {
            if width > policy.maximum_video_width {
                return Err(ValidationError::VideoWidthExceeded {
                    track_id: track.id,
                    found: width.get(),
                    maximum: policy.maximum_video_width.get(),
                });
            }
            if height > policy.maximum_video_height {
                return Err(ValidationError::VideoHeightExceeded {
                    track_id: track.id,
                    found: height.get(),
                    maximum: policy.maximum_video_height.get(),
                });
            }
            if let Some(frame_rate) = frame_rate
                && frame_rate.exceeds(policy.maximum_video_frame_rate)
            {
                return Err(ValidationError::VideoFrameRateExceeded {
                    track_id: track.id,
                    found: frame_rate,
                    maximum: policy.maximum_video_frame_rate,
                });
            }
        }
        MediaParameters::Audio {
            sample_rate,
            channels,
            ..
        } => {
            if sample_rate > policy.maximum_audio_sample_rate {
                return Err(ValidationError::AudioSampleRateExceeded {
                    track_id: track.id,
                    found: sample_rate.get(),
                    maximum: policy.maximum_audio_sample_rate.get(),
                });
            }
            if channels > policy.maximum_audio_channels {
                return Err(ValidationError::AudioChannelsExceeded {
                    track_id: track.id,
                    found: channels.get(),
                    maximum: policy.maximum_audio_channels.get(),
                });
            }
        }
        MediaParameters::Subtitle => {}
    }
    Ok(())
}

fn check_limit(kind: MediaKind, found: usize, limit: usize) -> Result<(), ValidationError> {
    if found > limit {
        return Err(ValidationError::TrackLimitExceeded { kind, limit, found });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::domain::fixtures::{TrackBuilder, catalog};

    use super::*;

    fn track(id: u32, kind: MediaKind, codec: Codec) -> DiscoveredTrack {
        TrackBuilder::new(id, kind).codec(codec).build()
    }

    fn policy() -> StreamPolicy {
        StreamPolicy {
            takeovers: crate::admission::TakeoverPolicy::Allow,
            ingest_timing: StreamPolicy::permissive().ingest_timing,
            accepted_video_codecs: vec![Codec::H264],
            accepted_audio_codecs: vec![Codec::Aac],
            maximum_audio_tracks: 1,
            maximum_subtitle_tracks: 2,
            maximum_video_tracks: 1,
            maximum_video_width: nz::u32!(3840),
            maximum_video_height: nz::u32!(2160),
            maximum_video_frame_rate: FrameRate::new(nz::u32!(60), nz::u32!(1)),
            maximum_audio_sample_rate: nz::u32!(96_000),
            maximum_audio_channels: nz::u16!(8),
        }
    }

    #[test]
    fn accepts_a_presentation_within_policy() {
        let tracks = catalog(vec![
            track(0, MediaKind::Video, Codec::H264),
            track(1, MediaKind::Audio, Codec::Aac),
        ]);

        let plan = validate(&tracks, &policy()).expect("presentation is accepted");
        assert_eq!(plan.counts().video, 1);
        assert_eq!(plan.counts().audio, 1);
    }

    #[test]
    fn rejects_a_codec_the_stream_may_not_publish() {
        let tracks = catalog(vec![track(0, MediaKind::Video, Codec::Av1)]);

        assert_eq!(
            validate(&tracks, &policy()),
            Err(ValidationError::UnsupportedCodec {
                track_id: TrackId(0),
                codec: Codec::Av1,
            })
        );
    }

    #[test]
    fn rejects_more_tracks_of_a_kind_than_policy_allows() {
        let tracks = catalog(vec![
            track(0, MediaKind::Video, Codec::H264),
            track(1, MediaKind::Audio, Codec::Aac),
            track(2, MediaKind::Audio, Codec::Aac),
        ]);

        assert_eq!(
            validate(&tracks, &policy()),
            Err(ValidationError::TrackLimitExceeded {
                kind: MediaKind::Audio,
                limit: 1,
                found: 2,
            })
        );
    }

    #[test]
    fn rejects_a_subtitle_only_presentation() {
        let tracks = catalog(vec![track(0, MediaKind::Subtitle, Codec::WebVtt)]);

        assert_eq!(
            validate(&tracks, &policy()),
            Err(ValidationError::NoPresentableTrack)
        );
    }

    #[test]
    fn rejects_video_dimensions_or_frame_rate_above_policy() {
        let mut video = track(0, MediaKind::Video, Codec::H264);
        video.parameters = MediaParameters::Video {
            width: nz::u32!(3840),
            height: nz::u32!(2160),
            frame_rate: Some(FrameRate::new(nz::u32!(120), nz::u32!(1))),
            video_delay: 0,
        };
        let mut constrained = policy();
        constrained.maximum_video_width = nz::u32!(1920);

        assert!(matches!(
            validate(&catalog(vec![video.clone()]), &constrained),
            Err(ValidationError::VideoWidthExceeded { .. })
        ));

        constrained.maximum_video_width = nz::u32!(3840);
        constrained.maximum_video_height = nz::u32!(1080);
        assert!(matches!(
            validate(&catalog(vec![video.clone()]), &constrained),
            Err(ValidationError::VideoHeightExceeded { .. })
        ));

        constrained.maximum_video_height = nz::u32!(2160);
        assert!(matches!(
            validate(&catalog(vec![video]), &constrained),
            Err(ValidationError::VideoFrameRateExceeded { .. })
        ));
    }

    #[test]
    fn rejects_audio_sample_rate_or_channels_above_policy() {
        let mut audio = track(0, MediaKind::Audio, Codec::Aac);
        audio.parameters = MediaParameters::Audio {
            sample_rate: nz::u32!(192_000),
            channels: nz::u16!(16),
            frame_size: Some(nz::u32!(1_024)),
            bit_depth: Some(nz::u16!(16)),
        };
        let constrained = policy();

        assert!(matches!(
            validate(&catalog(vec![audio.clone()]), &constrained),
            Err(ValidationError::AudioSampleRateExceeded { .. })
        ));

        let mut channels_only = constrained;
        channels_only.maximum_audio_sample_rate = nz::u32!(192_000);
        assert!(matches!(
            validate(&catalog(vec![audio]), &channels_only),
            Err(ValidationError::AudioChannelsExceeded { .. })
        ));
    }
}
