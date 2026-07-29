//! Format routing for one-rendition-per-track pass-through packaging.

use std::sync::Arc;

use crate::domain::MediaKind;

use super::{
    CmafMuxerConfig, MuxError, MuxerFactory, MuxerStartRequest, PackagedPresentation,
    PackagingRenditionId, StartedMuxer, TrackPackager, TrackRouter, cmaf, webvtt,
};

/// Builds one pass-through rendition per source track.
///
/// Encoded audio and video are routed to FFmpeg CMAF contexts while textual
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
                MediaKind::Subtitle => webvtt::build_track(rendition_id, track, plan)?,
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
            muxer: Box::new(TrackRouter::new(
                packagers,
                request.segmentation.shortest_part_duration(),
            )),
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
        segment::{SegmentationPlan, TrackSegmentationPlan},
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
            [TrackId(0), TrackId(1)]
                .into_iter()
                .map(|track_id| TrackSegmentationPlan {
                    track_id,
                    timebase: Timebase::hz90k(),
                    presentation_origin_pts: 0,
                    segmentation_origin_pts: 0,
                    first_segment_boundary_pts: 180_000,
                    segment_duration: nz::u64!(180_000),
                    part_access_units: nz::u32!(1),
                    part_duration: nz::u64!(90_000),
                    boundary_tolerance: 0,
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
        assert_eq!(subtitle.config.chunk_target, None);
        assert_eq!(subtitle.codecs.as_ref(), "wvtt");
        assert_eq!(subtitle.source_tracks.as_ref(), &[TrackId(1)]);
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
                timing: Default::default(),
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
                TrackSegmentationPlan {
                    track_id: TrackId(0),
                    timebase: audio_timebase,
                    presentation_origin_pts: -1_056,
                    segmentation_origin_pts: -1_056,
                    first_segment_boundary_pts: 7_136,
                    segment_duration: nz::u64!(8_192),
                    part_access_units: nz::u32!(2),
                    part_duration: nz::u64!(2_048),
                    boundary_tolerance: 0,
                },
                TrackSegmentationPlan {
                    track_id: TrackId(1),
                    timebase: Timebase::hz90k(),
                    presentation_origin_pts: -1_980,
                    segmentation_origin_pts: 0,
                    first_segment_boundary_pts: 180_000,
                    segment_duration: nz::u64!(180_000),
                    part_access_units: nz::u32!(1),
                    part_duration: nz::u64!(90_000),
                    boundary_tolerance: 0,
                },
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
                webvtt: Default::default(),
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
        let subtitle_start = output.iter().find_map(|media| match media {
            PackagedMedia::Segment(segment) if segment.rendition_id == PackagingRenditionId(1) => {
                Some(segment.media_start)
            }
            _ => None,
        });

        assert_eq!(audio_start, Some(0));
        assert_eq!(subtitle_start, Some(1_980));
        Ok(())
    }
}
