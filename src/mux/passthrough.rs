//! Format routing for one-rendition-per-track pass-through packaging.

use std::sync::Arc;

use crate::domain::MediaKind;

use super::{
    CmafMuxerConfig, MuxError, MuxerFactory, MuxerStartRequest, PackagedPresentation,
    PackagingRenditionId, StartedMuxer, TrackPackager, TrackRouter, cmaf, webvtt,
};

/// Builds one pass-through rendition per source track.
///
/// Encoded audio and video are routed to the CMAF packager while textual
/// subtitle codecs are packaged directly as WebVTT.
#[derive(Clone, Copy, Debug, Default)]
pub struct PassThroughMuxerFactory {
    cmaf: CmafMuxerConfig,
}

impl PassThroughMuxerFactory {
    /// Uses `cmaf` for every audio/video rendition created by this factory.
    pub fn new(cmaf: CmafMuxerConfig) -> Self {
        Self { cmaf }
    }
}

impl MuxerFactory for PassThroughMuxerFactory {
    fn start(&self, request: MuxerStartRequest<'_>) -> Result<StartedMuxer, MuxError> {
        let mut renditions = Vec::with_capacity(request.presentation.tracks().len());
        let mut packagers: Vec<Box<dyn TrackPackager>> =
            Vec::with_capacity(request.presentation.tracks().len());

        for (index, track) in request.presentation.tracks().iter().enumerate() {
            let plan = request
                .segmentation
                .get(track.id)
                .copied()
                .ok_or_else(|| invalid(format!("segmentation plan omits {}", track.id)))?;
            let rendition_id = PackagingRenditionId(
                u32::try_from(index)
                    .map_err(|_| invalid("too many tracks for packaging rendition IDs"))?,
            );
            let (rendition, packager) = match track.kind() {
                MediaKind::Audio | MediaKind::Video => {
                    cmaf::build_track(rendition_id, track, plan, self.cmaf, request.events.clone())?
                }
                MediaKind::Subtitle => {
                    webvtt::build_track(rendition_id, track, plan, request.events.clone())?
                }
            };
            renditions.push(rendition);
            packagers.push(packager);
        }

        let presentation = PackagedPresentation::with_default_topology(
            request.time_anchor,
            request.presentation,
            renditions,
        )
        .map_err(|error| invalid(error.to_string()))?;
        Ok(StartedMuxer {
            muxer: Box::new(TrackRouter::new(packagers, request.segmentation)),
            presentation: Arc::new(presentation),
        })
    }
}

fn invalid(message: impl Into<Box<str>>) -> MuxError {
    MuxError::InvalidPlan(message.into())
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::{
        admission::StreamPolicy,
        domain::{
            AudioTrim, Codec, MediaKind, MediaParameters, Payload, SessionId, Timebase, TrackId,
            fixtures::{TrackBuilder, catalog},
        },
        media::{AudioSample, NormalizedSample, SubtitleSample, validate},
        mux::{
            FinishReason, MediaSegmentFormat, MuxerFactory, PackagedMedia,
            fixtures::{AAC_EXTRADATA, AAC_FRAME, H264_EXTRADATA},
        },
        observe::Events,
        segment::{SegmentationPlan, fixtures::PlanBuilder},
    };

    use super::*;

    #[test]
    fn mixed_publication_routes_subrip_to_a_webvtt_rendition()
    -> Result<(), Box<dyn std::error::Error>> {
        let video = TrackBuilder::new(0, MediaKind::Video)
            .codec_extradata(Payload::from(H264_EXTRADATA))
            .build();
        let subtitle = TrackBuilder::new(1, MediaKind::Subtitle)
            .codec(Codec::SubRip)
            .build();
        let input = validate(&catalog(vec![video, subtitle]), &StreamPolicy::permissive())?;
        let segmentation = SegmentationPlan::new(
            &input,
            [0, 1]
                .into_iter()
                .map(|track_id| {
                    PlanBuilder::new(track_id, Timebase::hz90k(), nz::u64!(180_000))
                        .part(nz::u32!(1), nz::u64!(90_000))
                        .build()
                })
                .collect(),
        )?;
        let events = Events::default().scoped(SessionId(nz::u64!(1)));

        let started = PassThroughMuxerFactory::default().start(MuxerStartRequest {
            presentation: &input,
            segmentation: &segmentation,
            time_anchor: SystemTime::UNIX_EPOCH,
            events: &events,
        })?;

        assert_eq!(started.presentation.renditions.len(), 2);
        assert_eq!(
            started.presentation.renditions[0].config.segment_format,
            MediaSegmentFormat::Cmaf
        );
        let subtitle = &started.presentation.renditions[1];
        assert_eq!(subtitle.config.segment_format, MediaSegmentFormat::WebVtt);
        assert_eq!(
            subtitle.config.chunk_target,
            Some(nz::u64!(90_000)),
            "a subtitle rendition publishes on the same part cadence its plan \
             gives every other rendition, rather than opting out of parts"
        );
        assert_eq!(subtitle.codecs.as_ref(), "wvtt");
        assert_eq!(subtitle.source_tracks.as_ref(), &[TrackId(1)]);
        Ok(())
    }

    #[test]
    fn audio_alone_keeps_a_cueless_subtitle_rendition_on_cadence()
    -> Result<(), Box<dyn std::error::Error>> {
        let audio_timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .codec(Codec::Aac)
            .codec_extradata(Payload::from(AAC_EXTRADATA))
            .timebase(audio_timebase)
            .parameters(MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(1),
                frame_size: Some(nz::u32!(1_024)),
                bit_depth: None,
                timing: crate::domain::AudioTiming::default(),
            })
            .build();
        let subtitle = TrackBuilder::new(1, MediaKind::Subtitle)
            .codec(Codec::SubRip)
            .timebase(Timebase::hz90k())
            .build();
        let input = validate(&catalog(vec![audio, subtitle]), &StreamPolicy::permissive())?;
        let segmentation = SegmentationPlan::new(
            &input,
            vec![
                PlanBuilder::new(0, audio_timebase, nz::u64!(48_000))
                    .part(nz::u32!(8), nz::u64!(8_192))
                    .build(),
                // One-second subtitle segments, so a couple of seconds of audio
                // is enough to cross several boundaries.
                PlanBuilder::new(1, Timebase::hz90k(), nz::u64!(90_000))
                    .part(nz::u32!(1), nz::u64!(90_000))
                    .build(),
            ],
        )?;
        let events = Events::default().scoped(SessionId(nz::u64!(3)));
        let mut started = PassThroughMuxerFactory::default().start(MuxerStartRequest {
            presentation: &input,
            segmentation: &segmentation,
            time_anchor: SystemTime::UNIX_EPOCH,
            events: &events,
        })?;
        let mut output = Vec::new();

        // Just past three seconds of audio and not one subtitle cue: the case
        // that used to leave the subtitle playlist stuck at its origin while
        // audio and video stayed fully servable. The heartbeat reads a sample's
        // start rather than its end, so the last frame must begin past 3s for
        // the third subtitle window to be sealed.
        for frame in 0..142 {
            started.muxer.push(
                NormalizedSample::Audio(AudioSample {
                    track_id: TrackId(0),
                    codec: Codec::Aac,
                    pts: frame * 1_024,
                    duration: 1_024,
                    trim: AudioTrim::default(),
                    payload: Payload::from(AAC_FRAME),
                }),
                &mut output,
            )?;
        }

        let subtitle_segments: Vec<_> = output
            .iter()
            .filter_map(|media| match media {
                PackagedMedia::Segment(segment)
                    if segment.rendition_id == PackagingRenditionId(1) =>
                {
                    Some(segment)
                }
                _ => None,
            })
            .collect();
        assert_eq!(subtitle_segments.len(), 3);
        for (index, segment) in subtitle_segments.iter().enumerate() {
            assert!(segment.payload.is_empty());
            assert_eq!(segment.media_start, i64::try_from(index as u64 * 90_000)?);
            assert_eq!(segment.duration, 90_000);
        }
        Ok(())
    }

    #[test]
    fn mixed_cmaf_and_webvtt_preserve_the_publication_relative_offset()
    -> Result<(), Box<dyn std::error::Error>> {
        let audio_timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .codec(Codec::Aac)
            .codec_extradata(Payload::from(AAC_EXTRADATA))
            .timebase(audio_timebase)
            .parameters(MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(1),
                frame_size: Some(nz::u32!(1_024)),
                bit_depth: None,
                timing: crate::domain::AudioTiming::default(),
            })
            .build();
        let subtitle = TrackBuilder::new(1, MediaKind::Subtitle)
            .codec(Codec::SubRip)
            .timebase(Timebase::hz90k())
            .build();
        let input = validate(&catalog(vec![audio, subtitle]), &StreamPolicy::permissive())?;
        let segmentation = SegmentationPlan::new(
            &input,
            vec![
                // Audio priming puts both origins before zero; the boundary
                // still lands one segment after the segmentation origin.
                PlanBuilder::new(0, audio_timebase, nz::u64!(8_192))
                    .part(nz::u32!(2), nz::u64!(2_048))
                    .presentation_origin(-1_056)
                    .segmentation_origin(-1_056)
                    .build(),
                PlanBuilder::new(1, Timebase::hz90k(), nz::u64!(180_000))
                    .part(nz::u32!(1), nz::u64!(90_000))
                    .presentation_origin(-1_980)
                    .build(),
            ],
        )?;
        let events = Events::default().scoped(SessionId(nz::u64!(2)));
        let mut started = PassThroughMuxerFactory::default().start(MuxerStartRequest {
            presentation: &input,
            segmentation: &segmentation,
            time_anchor: SystemTime::UNIX_EPOCH,
            events: &events,
        })?;
        let mut output = Vec::new();

        started.muxer.push(
            NormalizedSample::Audio(AudioSample {
                track_id: TrackId(0),
                codec: Codec::Aac,
                pts: -1_056,
                duration: 1_024,
                trim: AudioTrim::default(),
                payload: Payload::from(AAC_FRAME),
            }),
            &mut output,
        )?;
        started.muxer.push(
            NormalizedSample::Subtitle(SubtitleSample {
                track_id: TrackId(1),
                codec: Codec::SubRip,
                pts: 0,
                duration: 90_000,
                webvtt: crate::domain::WebVttCueMetadata::default(),
                position: None,
                payload: Payload::from(b"later subtitle".as_slice()),
            }),
            &mut output,
        )?;
        started.muxer.finish(FinishReason::Final, &mut output)?;

        let audio_start = output.iter().find_map(|media| match media {
            PackagedMedia::Chunk(chunk) if chunk.rendition_id == PackagingRenditionId(0) => {
                Some(chunk.media_start)
            }
            _ => None,
        });
        // Subtitles carry a part cadence here, so their first published object
        // is a part rather than a whole segment.
        let subtitle_start = output.iter().find_map(|media| match media {
            PackagedMedia::Chunk(chunk) if chunk.rendition_id == PackagingRenditionId(1) => {
                Some(chunk.media_start)
            }
            _ => None,
        });

        assert_eq!(audio_start, Some(0));
        assert_eq!(subtitle_start, Some(1_980));
        Ok(())
    }
}
