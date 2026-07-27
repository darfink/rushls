use std::num::{NonZeroU16, NonZeroU32};

use thiserror::Error;

use super::{Payload, SourceTrackKey, TickTimestamp, Timebase, TrackId};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MediaKind {
    Audio,
    Subtitle,
    Video,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Codec {
    Aac,
    Av1,
    H264,
    Hevc,
    Opus,
    WebVtt,
    Unknown(u32),
}

/// Exact declared video cadence in frames per second.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameRate {
    numerator: NonZeroU32,
    denominator: NonZeroU32,
}

impl FrameRate {
    pub const fn new(numerator: NonZeroU32, denominator: NonZeroU32) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    pub fn numerator(self) -> NonZeroU32 {
        self.numerator
    }

    pub fn denominator(self) -> NonZeroU32 {
        self.denominator
    }

    pub fn exceeds(self, maximum: Self) -> bool {
        u64::from(self.numerator.get()) * u64::from(maximum.denominator.get())
            > u64::from(maximum.numerator.get()) * u64::from(self.denominator.get())
    }
}

/// What a track carries, and the shape it carries it in.
///
/// Discovered before validation, and re-checked while the session runs: an
/// input that changes these mid-stream is reported as
/// [`SourceError::CodecParametersChanged`](crate::source::SourceError), because
/// a resolution or channel-layout change is exactly that.
///
/// [`MediaKind`] is this enum's discriminant, kept separately so track counts,
/// policy limits, and error messages can name a kind without inventing
/// parameters they do not have.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaParameters {
    Video {
        width: NonZeroU32,
        height: NonZeroU32,
        /// May be unavailable or unreliable for some live inputs. Runtime
        /// cadence validation must still protect the pipeline.
        frame_rate: Option<FrameRate>,
        /// Number of delayed frames declared by the codec.
        video_delay: u32,
    },
    Audio {
        sample_rate: NonZeroU32,
        channels: NonZeroU16,
        /// Encoded samples per frame, when fixed and declared.
        frame_size: Option<NonZeroU32>,
        /// Meaningful bits in each decoded sample, when declared.
        bit_depth: Option<NonZeroU16>,
    },
    Subtitle,
}

impl MediaParameters {
    pub fn kind(self) -> MediaKind {
        match self {
            Self::Audio { .. } => MediaKind::Audio,
            Self::Subtitle => MediaKind::Subtitle,
            Self::Video { .. } => MediaKind::Video,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredTrack {
    pub id: TrackId,
    /// Stable source identity when the protocol or carried container has one.
    pub source_key: Option<SourceTrackKey>,
    pub codec: Codec,
    pub parameters: MediaParameters,
    pub timebase: Timebase,
    pub first_pts: Option<TickTimestamp>,
    pub title: Option<String>,
    pub language: Option<String>,
    /// Codec initialization bytes required by downstream muxers.
    pub codec_extradata: Payload,
}

impl DiscoveredTrack {
    pub fn kind(&self) -> MediaKind {
        self.parameters.kind()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackCatalog {
    tracks: Vec<DiscoveredTrack>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TrackCatalogError {
    #[error("track catalog is empty")]
    Empty,
    #[error("track id {0} occurs more than once")]
    DuplicateId(TrackId),
}

impl TrackCatalog {
    pub fn new(tracks: Vec<DiscoveredTrack>) -> Result<Self, TrackCatalogError> {
        if tracks.is_empty() {
            return Err(TrackCatalogError::Empty);
        }

        for (index, track) in tracks.iter().enumerate() {
            if tracks[..index]
                .iter()
                .any(|candidate| candidate.id == track.id)
            {
                return Err(TrackCatalogError::DuplicateId(track.id));
            }
        }

        Ok(Self { tracks })
    }

    pub fn tracks(&self) -> &[DiscoveredTrack] {
        &self.tracks
    }

    pub fn get(&self, track_id: TrackId) -> Option<&DiscoveredTrack> {
        self.tracks.iter().find(|track| track.id == track_id)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &DiscoveredTrack> {
        self.tracks.iter()
    }

    /// Counts tracks of each kind in `(audio, subtitle, video)` order.
    pub fn counts(&self) -> TrackCounts {
        let mut counts = TrackCounts::default();
        for track in &self.tracks {
            match track.kind() {
                MediaKind::Audio => counts.audio += 1,
                MediaKind::Subtitle => counts.subtitle += 1,
                MediaKind::Video => counts.video += 1,
            }
        }
        counts
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TrackCounts {
    pub audio: usize,
    pub subtitle: usize,
    pub video: usize,
}

#[cfg(test)]
mod tests {
    use crate::domain::fixtures::track;

    use super::*;

    #[test]
    fn catalog_rejects_empty_and_duplicated_track_sets() {
        assert_eq!(TrackCatalog::new(Vec::new()), Err(TrackCatalogError::Empty));
        assert_eq!(
            TrackCatalog::new(vec![track(0, MediaKind::Video), track(0, MediaKind::Audio)]),
            Err(TrackCatalogError::DuplicateId(TrackId(0)))
        );
    }

    #[test]
    fn catalog_counts_tracks_by_kind() {
        let catalog = TrackCatalog::new(vec![
            track(0, MediaKind::Video),
            track(1, MediaKind::Audio),
            track(2, MediaKind::Audio),
        ])
        .expect("catalog is valid");

        assert_eq!(
            catalog.counts(),
            TrackCounts {
                audio: 2,
                subtitle: 0,
                video: 1,
            }
        );
    }

    #[test]
    fn frame_rate_comparison_is_exact() {
        let ntsc = FrameRate::new(nz::u32!(30_000), nz::u32!(1_001));
        let thirty = FrameRate::new(nz::u32!(30), nz::u32!(1));

        assert!(!ntsc.exceeds(thirty));
        assert!(thirty.exceeds(ntsc));
    }
}
