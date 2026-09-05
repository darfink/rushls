use thiserror::Error;

use crate::{
    admission::{Bounds, FrameBox, StreamPolicy},
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

    /// Rebuilds an already-validated plan after a representation-only track
    /// transformation such as timestamp normalization.
    ///
    /// Callers must preserve track identities, codecs, and media parameters.
    /// Policy validation is intentionally not repeated: changing a timebase
    /// cannot make an admitted codec or resolution inadmissible, while running
    /// policy again here would require carrying session policy into a purely
    /// mechanical media stage.
    pub(crate) fn with_projected_tracks(
        &self,
        tracks: Vec<DiscoveredTrack>,
    ) -> Result<Self, crate::domain::TrackCatalogError> {
        debug_assert_eq!(self.tracks.tracks().len(), tracks.len());
        debug_assert!(
            self.tracks
                .tracks()
                .iter()
                .zip(&tracks)
                .all(|(before, after)| before.id == after.id
                    && before.codec == after.codec
                    && before.parameters == after.parameters)
        );
        Ok(Self {
            tracks: TrackCatalog::new(tracks)?,
        })
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ValidationError {
    #[error("{track_id} uses {codec:?}, which this stream may not publish")]
    UnsupportedCodec { track_id: TrackId, codec: Codec },
    #[error("the stream published {found} {kind:?} tracks, but this stream admits {admitted}")]
    TrackCountRefused {
        kind: MediaKind,
        admitted: String,
        found: usize,
    },
    #[error("the stream published no audio or video track")]
    NoPresentableTrack,
    #[error("{track_id} is {found}, but this stream admits {admitted}")]
    ResolutionRefused {
        track_id: TrackId,
        found: FrameBox,
        admitted: String,
    },
    #[error("{track_id} runs at {found}, but this stream admits {admitted}")]
    FrameRateRefused {
        track_id: TrackId,
        found: FrameRate,
        admitted: String,
    },
    #[error("{track_id} samples at {found}Hz, but this stream admits {admitted}")]
    SampleRateRefused {
        track_id: TrackId,
        found: u32,
        admitted: String,
    },
    #[error("{track_id} carries {found} channels, but this stream admits {admitted}")]
    ChannelsRefused {
        track_id: TrackId,
        found: u16,
        admitted: String,
    },
}

/// Accepts a discovered track set for presentation, or explains the rejection.
pub fn validate(
    tracks: &TrackCatalog,
    policy: &StreamPolicy,
) -> Result<PresentationPlan, ValidationError> {
    let counts = tracks.counts();
    check_count(MediaKind::Audio, counts.audio, &policy.audio.tracks)?;
    check_count(
        MediaKind::Subtitle,
        counts.subtitle,
        &policy.subtitles.tracks,
    )?;
    check_count(MediaKind::Video, counts.video, &policy.video.tracks)?;
    if counts.audio == 0 && counts.video == 0 {
        return Err(ValidationError::NoPresentableTrack);
    }

    for track in tracks.tracks() {
        let accepted = match track.kind() {
            MediaKind::Audio => policy.audio.codecs.admits(track.codec),
            MediaKind::Video => policy.video.codecs.admits(track.codec),
            // Checked like any other kind. Subtitles used to be waved through
            // on the grounds that the muxer would decide — but the muxer runs
            // after pre-roll has already buffered the publisher's media, so
            // "decide later" meant accepting a session that could not be
            // packaged and failing it seconds in rather than at the handshake.
            MediaKind::Subtitle => policy.subtitles.codecs.admits(track.codec),
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
            let found = FrameBox::new(width.get(), height.get());
            if !policy.video.resolution.admits(found) {
                return Err(ValidationError::ResolutionRefused {
                    track_id: track.id,
                    found,
                    admitted: policy.video.resolution.to_string(),
                });
            }
            // A track that declares no rate cannot be judged against one. It
            // is admitted rather than refused: discovery not having observed a
            // cadence is this node's gap, not the publisher's.
            if let Some(frame_rate) = frame_rate
                && !policy.video.frame_rate.admits(&frame_rate)
            {
                return Err(ValidationError::FrameRateRefused {
                    track_id: track.id,
                    found: frame_rate,
                    admitted: policy.video.frame_rate.to_string(),
                });
            }
        }
        MediaParameters::Audio {
            sample_rate,
            channels,
            ..
        } => {
            if !policy.audio.sample_rate.admits(&sample_rate) {
                return Err(ValidationError::SampleRateRefused {
                    track_id: track.id,
                    found: sample_rate.get(),
                    admitted: policy.audio.sample_rate.to_string(),
                });
            }
            if !policy.audio.channels.admits(&channels) {
                return Err(ValidationError::ChannelsRefused {
                    track_id: track.id,
                    found: channels.get(),
                    admitted: policy.audio.channels.to_string(),
                });
            }
        }
        MediaParameters::Subtitle => {}
    }
    Ok(())
}

fn check_count(
    kind: MediaKind,
    found: usize,
    admitted: &Bounds<usize>,
) -> Result<(), ValidationError> {
    if admitted.admits(&found) {
        return Ok(());
    }
    Err(ValidationError::TrackCountRefused {
        kind,
        admitted: admitted.to_string(),
        found,
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        admission::Resolution,
        domain::fixtures::{TrackBuilder, catalog},
    };

    use super::*;

    fn track(id: u32, kind: MediaKind, codec: Codec) -> DiscoveredTrack {
        TrackBuilder::new(id, kind).codec(codec).build()
    }

    fn policy() -> StreamPolicy {
        StreamPolicy {
            takeovers: crate::admission::TakeoverPolicy::Allow,
            ceiling: None,
            floor: None,
            maximum_timestamp_jump: std::time::Duration::from_secs(10),
            video: crate::admission::VideoAccept {
                codecs: crate::admission::Codecs::OneOf(vec![Codec::H264]),
                resolution: crate::admission::Resolution::AtMost(FrameBox::new(3840, 2160)),
                frame_rate: Bounds::at_most(FrameRate::new(nz::u32!(60), nz::u32!(1))),
                tracks: Bounds::at_most(1),
            },
            audio: crate::admission::AudioAccept {
                codecs: crate::admission::Codecs::OneOf(vec![Codec::Aac]),
                sample_rate: Bounds::at_most(nz::u32!(96_000)),
                channels: Bounds::at_most(nz::u16!(8)),
                tracks: Bounds::at_most(1),
            },
            subtitles: crate::admission::SubtitleAccept {
                codecs: crate::admission::Codecs::OneOf(vec![Codec::WebVtt, Codec::SubRip]),
                tracks: Bounds::at_most(2),
            },
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
            Err(ValidationError::TrackCountRefused {
                kind: MediaKind::Audio,
                admitted: "at most 1".to_owned(),
                found: 2,
            })
        );
    }

    #[test]
    fn accepts_every_cue_format_a_muxer_can_present_as_webvtt() {
        // SubRip is admitted even though it is not carried unchanged: HLS
        // needs WebVTT out, and the muxer can convert its demuxed cue text.
        for codec in [Codec::WebVtt, Codec::SubRip] {
            let tracks = catalog(vec![
                track(0, MediaKind::Video, Codec::H264),
                track(1, MediaKind::Subtitle, codec),
            ]);

            let plan = validate(&tracks, &policy())
                .unwrap_or_else(|error| panic!("{codec:?} subtitles are accepted: {error}"));
            assert_eq!(plan.counts().subtitle, 1);
        }
    }

    #[test]
    fn in_band_text_is_admitted_only_where_a_policy_asks_for_it() {
        let tracks = catalog(vec![
            track(0, MediaKind::Video, Codec::H264),
            track(1, MediaKind::Subtitle, Codec::Text),
        ]);

        // The default policy admits `text` (FLV script data is a mainstream
        // caption carriage), so this uses a narrower policy to prove the codec
        // is still gated on admission rather than hard-coded.
        assert_eq!(
            validate(&tracks, &policy()),
            Err(ValidationError::UnsupportedCodec {
                track_id: TrackId(1),
                codec: Codec::Text,
            })
        );

        let mut enabled = policy();
        enabled.subtitles.codecs =
            crate::admission::Codecs::OneOf(vec![Codec::WebVtt, Codec::SubRip, Codec::Text]);
        let plan = validate(&tracks, &enabled).expect("text subtitles are accepted when enabled");
        assert_eq!(plan.counts().subtitle, 1);
    }

    #[test]
    fn mov_text_waits_for_its_dedicated_converter() {
        let tracks = catalog(vec![
            track(0, MediaKind::Video, Codec::H264),
            track(1, MediaKind::Subtitle, Codec::MovText),
        ]);

        assert_eq!(
            validate(&tracks, &policy()),
            Err(ValidationError::UnsupportedCodec {
                track_id: TrackId(1),
                codec: Codec::MovText,
            })
        );
    }

    #[test]
    fn rejects_a_cue_format_no_muxer_can_turn_into_webvtt() {
        // Subtitles used to bypass the codec check entirely, so an
        // unpresentable cue format was admitted and only failed once the muxer
        // had already let the publisher buffer media through pre-roll.
        let tracks = catalog(vec![
            track(0, MediaKind::Video, Codec::H264),
            track(1, MediaKind::Subtitle, Codec::Unknown(94)),
        ]);

        assert_eq!(
            validate(&tracks, &policy()),
            Err(ValidationError::UnsupportedCodec {
                track_id: TrackId(1),
                codec: Codec::Unknown(94),
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
    fn a_resolution_bound_is_a_box_rather_than_two_independent_limits() {
        let mut video = track(0, MediaKind::Video, Codec::H264);
        video.parameters = MediaParameters::Video {
            width: nz::u32!(3840),
            height: nz::u32!(2160),
            frame_rate: Some(FrameRate::new(nz::u32!(120), nz::u32!(1))),
            video_delay: 0,
        };
        let mut constrained = policy();
        constrained.video.resolution = Resolution::AtMost(FrameBox::new(1920, 1080));

        assert!(matches!(
            validate(&catalog(vec![video.clone()]), &constrained),
            Err(ValidationError::ResolutionRefused { .. })
        ));

        constrained.video.resolution = Resolution::AtMost(FrameBox::new(3840, 2160));
        assert!(
            matches!(
                validate(&catalog(vec![video]), &constrained),
                Err(ValidationError::FrameRateRefused { .. })
            ),
            "the frame fits the box, so the rate is what refuses it"
        );
    }

    #[test]
    fn a_portrait_source_fits_a_landscape_box() {
        // Checking width and height against their own limits refused this,
        // which is wrong: a bounding box has no orientation.
        let mut portrait = track(0, MediaKind::Video, Codec::H264);
        portrait.parameters = MediaParameters::Video {
            width: nz::u32!(1080),
            height: nz::u32!(1920),
            frame_rate: None,
            video_delay: 0,
        };
        let mut constrained = policy();
        constrained.video.resolution = Resolution::AtMost(FrameBox::new(1920, 1080));

        assert!(validate(&catalog(vec![portrait]), &constrained).is_ok());
    }

    #[test]
    fn a_frame_rate_bound_may_have_a_floor_as_well_as_a_ceiling() {
        let mut video = track(0, MediaKind::Video, Codec::H264);
        video.parameters = MediaParameters::Video {
            width: nz::u32!(1920),
            height: nz::u32!(1080),
            frame_rate: Some(FrameRate::new(nz::u32!(15), nz::u32!(1))),
            video_delay: 0,
        };
        let mut constrained = policy();
        constrained.video.frame_rate = Bounds::Range {
            min: Some(FrameRate::new(nz::u32!(24), nz::u32!(1))),
            max: Some(FrameRate::new(nz::u32!(60), nz::u32!(1))),
        };

        assert!(
            matches!(
                validate(&catalog(vec![video]), &constrained),
                Err(ValidationError::FrameRateRefused { .. })
            ),
            "minimum bounds are new: the old surface could only cap"
        );
    }

    #[test]
    fn ntsc_rates_compare_as_exact_rationals() {
        // 30000/1001 is not 30, and a predicate that says 30 must not admit
        // it. Comparison stays rational rather than rounding either side.
        let mut video = track(0, MediaKind::Video, Codec::H264);
        video.parameters = MediaParameters::Video {
            width: nz::u32!(1920),
            height: nz::u32!(1080),
            frame_rate: Some(FrameRate::new(nz::u32!(30_000), nz::u32!(1_001))),
            video_delay: 0,
        };
        let mut constrained = policy();
        constrained.video.frame_rate = Bounds::Exact(FrameRate::new(nz::u32!(30), nz::u32!(1)));
        assert!(matches!(
            validate(&catalog(vec![video.clone()]), &constrained),
            Err(ValidationError::FrameRateRefused { .. })
        ));

        // The same rate spelled differently is the same rate.
        constrained.video.frame_rate =
            Bounds::Exact(FrameRate::new(nz::u32!(60_000), nz::u32!(2_002)));
        assert!(validate(&catalog(vec![video]), &constrained).is_ok());
    }

    #[test]
    fn rejects_audio_sample_rate_or_channels_outside_policy() {
        let mut audio = track(0, MediaKind::Audio, Codec::Aac);
        audio.parameters = MediaParameters::Audio {
            sample_rate: nz::u32!(192_000),
            channels: nz::u16!(16),
            frame_size: Some(nz::u32!(1_024)),
            bit_depth: Some(nz::u16!(16)),
            timing: crate::domain::AudioTiming::default(),
        };
        let constrained = policy();

        assert!(matches!(
            validate(&catalog(vec![audio.clone()]), &constrained),
            Err(ValidationError::SampleRateRefused { .. })
        ));

        let mut channels_only = constrained;
        channels_only.audio.sample_rate = Bounds::at_most(nz::u32!(192_000));
        assert!(matches!(
            validate(&catalog(vec![audio]), &channels_only),
            Err(ValidationError::ChannelsRefused { .. })
        ));
    }

    #[test]
    fn a_sample_rate_floor_refuses_a_rate_below_it() {
        let mut audio = track(0, MediaKind::Audio, Codec::Aac);
        audio.parameters = MediaParameters::Audio {
            sample_rate: nz::u32!(32_000),
            channels: nz::u16!(2),
            frame_size: Some(nz::u32!(1_024)),
            bit_depth: Some(nz::u16!(16)),
            timing: crate::domain::AudioTiming::default(),
        };
        let mut constrained = policy();
        constrained.audio.sample_rate = Bounds::Range {
            min: Some(nz::u32!(44_100)),
            max: Some(nz::u32!(48_000)),
        };

        assert!(matches!(
            validate(&catalog(vec![audio]), &constrained),
            Err(ValidationError::SampleRateRefused { .. })
        ));
    }
}
