use std::{
    collections::{HashMap, HashSet},
    num::{NonZeroU16, NonZeroU32},
    sync::Arc,
    time::SystemTime,
};

use derive_more::Display;
use thiserror::Error;

use crate::{
    domain::{DiscoveredTrack, FrameRate, MediaKind, TrackId},
    media::PresentationPlan,
    observe::EventSink,
};

use super::{MediaSegmentFormat, RenditionConfig};

/// Identifies one output within a muxer publication.
///
/// Delivery maps this local ID to its own durable rendition identity before
/// accepting media. It may therefore restart or be reordered on reconnect.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("packaging-rendition/{_0}")]
pub struct PackagingRenditionId(pub u32);

/// Stable semantic identity assigned by the muxer to one packaged output.
///
/// Exact keys permit delivery to preserve a media-playlist URI and numbering
/// across reconnects. The value is opaque so a pass-through muxer may derive it
/// from a source identity while a transcoder may derive it from a ladder slot.
#[derive(Clone, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenditionKey(pub Arc<str>);

impl RenditionKey {
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    /// Derives the pass-through output key from the best source identity.
    ///
    /// The media kind prevents protocols that number audio and video tracks in
    /// separate namespaces from colliding. When no durable source key exists,
    /// the local track ID still gives deterministic output within a
    /// publication, but reconnect continuity is intentionally not promised.
    pub fn for_source(track: &DiscoveredTrack) -> Self {
        match &track.source_key {
            Some(key) => Self::new(format!("source/{:?}/{}", track.kind(), key.0)),
            None => Self::new(format!("publication/{:?}/{}", track.kind(), track.id.0)),
        }
    }
}

/// Stable identity for one set of alternative renditions.
///
/// Groups are immutable topology, not retained delivery state. Their keys make
/// equivalent reconnect descriptors compare equal and give later manifest
/// projections deterministic protocol-level group names.
#[derive(Clone, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenditionGroupKey(pub Arc<str>);

impl RenditionGroupKey {
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoRange {
    Sdr,
    Hlg,
    Pq,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenditionMedia {
    Video {
        width: NonZeroU32,
        height: NonZeroU32,
        frame_rate: Option<FrameRate>,
        video_range: Option<VideoRange>,
    },
    Audio {
        sample_rate: NonZeroU32,
        channels: NonZeroU16,
    },
    Subtitle,
}

impl RenditionMedia {
    /// The attributes only an audio rendition has, as a pair because a playlist
    /// that advertises one of them advertises both.
    pub fn audio(&self) -> Option<(NonZeroU32, NonZeroU16)> {
        match self {
            Self::Audio {
                sample_rate,
                channels,
            } => Some((*sample_rate, *channels)),
            Self::Subtitle | Self::Video { .. } => None,
        }
    }

    pub fn kind(&self) -> MediaKind {
        match self {
            Self::Audio { .. } => MediaKind::Audio,
            Self::Subtitle => MediaKind::Subtitle,
            Self::Video { .. } => MediaKind::Video,
        }
    }
}

/// One independently packaged output and future media-playlist projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackagedRendition {
    pub packaging_rendition_id: PackagingRenditionId,
    pub key: RenditionKey,
    /// Input tracks consumed to create this output. A transcoded rendition may
    /// still name its source while advertising muxer-authored output metadata.
    pub source_tracks: Arc<[TrackId]>,
    pub config: RenditionConfig,
    pub media: RenditionMedia,
    /// Final RFC 6381 codec list for the bytes emitted by the muxer.
    pub codecs: Arc<str>,
    pub name: Arc<str>,
    pub language: Option<Arc<str>>,
    pub is_default: bool,
    /// Initial advertisement before completed media yields measured bitrate.
    pub declared_bandwidth: Option<u64>,
}

impl PackagedRendition {
    /// Whether retained delivery state can safely continue under this output.
    ///
    /// Exact semantic identity is necessary but not sufficient: changing the
    /// media kind, codec family, or container under one playlist URI is poorly
    /// supported even when separated by a discontinuity.
    ///
    /// Packaging cadence is included for the same reason, one level down. A
    /// reconnect that repartitions the timeline is still the same *stream*, but
    /// it is no longer the same series of segments, and delivery formats that
    /// commit to a cadence for a playlist's lifetime cannot absorb the change
    /// under the existing identity. What such a format then does about it —
    /// retire the old output, start a new one — is its own decision; this only
    /// reports that the two are not continuations of each other.
    pub fn compatible_with(&self, other: &Self) -> bool {
        self.key == other.key
            && self.media.kind() == other.media.kind()
            && self.codecs == other.codecs
            && self.config.segment_format == other.config.segment_format
            && self.config.timebase == other.config.timebase
            && self.config.segment_target == other.config.segment_target
            && self.config.maximum_segment_duration == other.config.maximum_segment_duration
            && self.config.chunk_target == other.config.chunk_target
    }
}

/// Alternative outputs of one media kind and role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenditionGroup {
    pub key: RenditionGroupKey,
    pub media_kind: MediaKind,
    pub renditions: Arc<[PackagingRenditionId]>,
}

/// Groups the player may combine into one presentation.
///
/// Multiple combinations express compatibility restrictions without embedding
/// HLS-specific AUDIO, VIDEO, or SUBTITLES attributes in the mux contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayableCombination {
    pub groups: Arc<[RenditionGroupKey]>,
}

/// One caption service carried inside the video access units themselves.
///
/// Presentation-wide rather than a rendition, and deliberately so. An in-band
/// caption service has no media of its own: the bytes ride in every video
/// rendition's segments, so the service exists once for the presentation while
/// each video rendition is expected to carry it. Modelling it as a rendition
/// would give it a store entry, a URI, and a media playlist, none of which it
/// can have.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClosedCaptionService {
    /// Which in-band channel carries it, as the delivery layer names it.
    pub channel: CaptionChannel,
    pub name: Arc<str>,
    pub language: Option<Arc<str>>,
    pub is_default: bool,
    pub autoselect: bool,
}

/// An in-band caption channel, independent of how a manifest spells it.
///
/// Kept in mux terms so the packaging contract does not depend on the delivery
/// protocol's vocabulary; the HLS projection maps this onto `INSTREAM-ID`.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CaptionChannel {
    /// A CEA-608 Line 21 field, counted from zero.
    #[display("cea608-field{_0}")]
    Cea608Field(u8),
    /// A CEA-708 DTVCC service block number.
    #[display("cea708-service{_0}")]
    Cea708Service(u8),
}

/// Immutable description of every output produced by one muxer publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackagedPresentation {
    /// Wall time used as the common PROGRAM-DATE-TIME origin for all outputs.
    pub time_anchor: SystemTime,
    pub renditions: Arc<[PackagedRendition]>,
    pub groups: Arc<[RenditionGroup]>,
    pub combinations: Arc<[PlayableCombination]>,
    /// In-band caption services every video rendition is expected to carry.
    ///
    /// Empty until something establishes them. A pass-through publication
    /// starts empty and gains services once detection confirms the bitstream
    /// actually carries them, which is why this is not derived from the input
    /// track set.
    pub closed_captions: Arc<[ClosedCaptionService]>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PackagedPresentationError {
    #[error("a packaged presentation has no renditions")]
    Empty,
    #[error("packaging rendition id {0} occurs more than once")]
    DuplicateRenditionId(PackagingRenditionId),
    #[error("rendition key {0} occurs more than once")]
    DuplicateRenditionKey(RenditionKey),
    #[error("rendition group key {0} occurs more than once")]
    DuplicateGroupKey(RenditionGroupKey),
    #[error("{rendition_id} references unknown source track {track_id}")]
    UnknownSourceTrack {
        rendition_id: PackagingRenditionId,
        track_id: TrackId,
    },
    #[error("{rendition_id} references source track {track_id} more than once")]
    DuplicateSourceTrack {
        rendition_id: PackagingRenditionId,
        track_id: TrackId,
    },
    #[error("{rendition_id} has an empty RFC 6381 codec string")]
    EmptyCodecs { rendition_id: PackagingRenditionId },
    #[error("rendition group {0} is empty")]
    EmptyGroup(RenditionGroupKey),
    #[error("rendition group {group} references unknown {rendition_id}")]
    UnknownGroupedRendition {
        group: RenditionGroupKey,
        rendition_id: PackagingRenditionId,
    },
    #[error("{rendition_id} appears more than once in rendition group {group}")]
    DuplicateGroupedRendition {
        group: RenditionGroupKey,
        rendition_id: PackagingRenditionId,
    },
    #[error("{rendition_id} does not match the media kind of rendition group {group}")]
    GroupKindMismatch {
        group: RenditionGroupKey,
        rendition_id: PackagingRenditionId,
    },
    #[error("rendition group {0} declares more than one default")]
    MultipleGroupDefaults(RenditionGroupKey),
    #[error("{rendition_id} must belong to exactly one group")]
    InvalidGroupMembership { rendition_id: PackagingRenditionId },
    #[error("a playable combination is empty")]
    EmptyCombination,
    #[error("a playable combination references unknown rendition group {0}")]
    UnknownCombinationGroup(RenditionGroupKey),
    #[error("a playable combination contains more than one {0:?} group")]
    DuplicateCombinationKind(MediaKind),
    #[error("a playable combination contains neither audio nor video")]
    CombinationNotPresentable,
    #[error("rendition group {0} is not used by any playable combination")]
    UnusedGroup(RenditionGroupKey),
}

impl PackagedPresentation {
    /// Builds the deterministic pass-through topology used when no application
    /// supplies compatibility relationships.
    ///
    /// Every rendition of a media kind becomes an alternative in one group,
    /// and the single combination permits the available video, audio, and
    /// subtitle groups together. Protocol adapters may provide stable source
    /// keys and defaults without needing to understand this grouping policy.
    pub fn with_default_topology(
        time_anchor: SystemTime,
        input: &PresentationPlan,
        mut renditions: Vec<PackagedRendition>,
    ) -> Result<Self, PackagedPresentationError> {
        let mut groups = Vec::new();
        for (kind, key) in [
            (MediaKind::Video, "video"),
            (MediaKind::Audio, "audio"),
            (MediaKind::Subtitle, "subtitle"),
        ] {
            if !renditions
                .iter()
                .any(|rendition| rendition.media.kind() == kind && rendition.is_default)
                && let Some(first) = renditions
                    .iter_mut()
                    .find(|rendition| rendition.media.kind() == kind)
            {
                first.is_default = true;
            }
            let members: Vec<_> = renditions
                .iter()
                .filter(|rendition| rendition.media.kind() == kind)
                .map(|rendition| rendition.packaging_rendition_id)
                .collect();
            if !members.is_empty() {
                groups.push(RenditionGroup {
                    key: RenditionGroupKey::new(key),
                    media_kind: kind,
                    renditions: members.into(),
                });
            }
        }
        let combination = PlayableCombination {
            groups: groups
                .iter()
                .map(|group| group.key.clone())
                .collect::<Vec<_>>()
                .into(),
        };
        Self::new(time_anchor, input, renditions, groups, vec![combination])
    }

    pub fn new(
        time_anchor: SystemTime,
        input: &PresentationPlan,
        renditions: impl Into<Arc<[PackagedRendition]>>,
        groups: impl Into<Arc<[RenditionGroup]>>,
        combinations: impl Into<Arc<[PlayableCombination]>>,
    ) -> Result<Self, PackagedPresentationError> {
        let value = Self {
            time_anchor,
            renditions: renditions.into(),
            groups: groups.into(),
            combinations: combinations.into(),
            // Captions are established after publication, by observing the
            // bitstream rather than by describing it up front.
            closed_captions: Arc::from([]),
        };
        value.validate(input)?;
        Ok(value)
    }

    pub fn rendition(&self, rendition_id: PackagingRenditionId) -> Option<&PackagedRendition> {
        self.renditions
            .iter()
            .find(|rendition| rendition.packaging_rendition_id == rendition_id)
    }

    fn validate(&self, input: &PresentationPlan) -> Result<(), PackagedPresentationError> {
        if self.renditions.is_empty() {
            return Err(PackagedPresentationError::Empty);
        }
        self.validate_renditions(input)?;
        let memberships = self.validate_groups()?;
        for rendition in self.renditions.iter() {
            if memberships.get(&rendition.packaging_rendition_id) != Some(&1) {
                return Err(PackagedPresentationError::InvalidGroupMembership {
                    rendition_id: rendition.packaging_rendition_id,
                });
            }
        }
        self.validate_combinations()
    }

    fn validate_renditions(
        &self,
        input: &PresentationPlan,
    ) -> Result<(), PackagedPresentationError> {
        let mut rendition_ids = HashSet::new();
        let mut rendition_keys = HashSet::new();
        for rendition in self.renditions.iter() {
            if !rendition_ids.insert(rendition.packaging_rendition_id) {
                return Err(PackagedPresentationError::DuplicateRenditionId(
                    rendition.packaging_rendition_id,
                ));
            }
            if !rendition_keys.insert(rendition.key.clone()) {
                return Err(PackagedPresentationError::DuplicateRenditionKey(
                    rendition.key.clone(),
                ));
            }
            if rendition.codecs.is_empty() {
                return Err(PackagedPresentationError::EmptyCodecs {
                    rendition_id: rendition.packaging_rendition_id,
                });
            }
            let mut source_tracks = HashSet::new();
            for &track_id in rendition.source_tracks.iter() {
                if input.catalog().get(track_id).is_none() {
                    return Err(PackagedPresentationError::UnknownSourceTrack {
                        rendition_id: rendition.packaging_rendition_id,
                        track_id,
                    });
                }
                if !source_tracks.insert(track_id) {
                    return Err(PackagedPresentationError::DuplicateSourceTrack {
                        rendition_id: rendition.packaging_rendition_id,
                        track_id,
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_groups(
        &self,
    ) -> Result<HashMap<PackagingRenditionId, usize>, PackagedPresentationError> {
        let by_id: HashMap<_, _> = self
            .renditions
            .iter()
            .map(|rendition| (rendition.packaging_rendition_id, rendition))
            .collect();
        let mut group_keys = HashSet::new();
        let mut memberships: HashMap<PackagingRenditionId, usize> = HashMap::new();
        for group in self.groups.iter() {
            if !group_keys.insert(group.key.clone()) {
                return Err(PackagedPresentationError::DuplicateGroupKey(
                    group.key.clone(),
                ));
            }
            if group.renditions.is_empty() {
                return Err(PackagedPresentationError::EmptyGroup(group.key.clone()));
            }
            let mut members = HashSet::new();
            let mut defaults = 0;
            for &rendition_id in group.renditions.iter() {
                let Some(rendition) = by_id.get(&rendition_id) else {
                    return Err(PackagedPresentationError::UnknownGroupedRendition {
                        group: group.key.clone(),
                        rendition_id,
                    });
                };
                if !members.insert(rendition_id) {
                    return Err(PackagedPresentationError::DuplicateGroupedRendition {
                        group: group.key.clone(),
                        rendition_id,
                    });
                }
                if rendition.media.kind() != group.media_kind {
                    return Err(PackagedPresentationError::GroupKindMismatch {
                        group: group.key.clone(),
                        rendition_id,
                    });
                }
                defaults += usize::from(rendition.is_default);
                *memberships.entry(rendition_id).or_default() += 1;
            }
            if defaults > 1 {
                return Err(PackagedPresentationError::MultipleGroupDefaults(
                    group.key.clone(),
                ));
            }
        }
        Ok(memberships)
    }

    fn validate_combinations(&self) -> Result<(), PackagedPresentationError> {
        let group_by_key: HashMap<RenditionGroupKey, &RenditionGroup> = self
            .groups
            .iter()
            .map(|group| (group.key.clone(), group))
            .collect();
        let mut used_groups = HashSet::new();
        for combination in self.combinations.iter() {
            if combination.groups.is_empty() {
                return Err(PackagedPresentationError::EmptyCombination);
            }
            let mut kinds = HashSet::new();
            let mut presentable = false;
            for group_key in combination.groups.iter() {
                let Some(group) = group_by_key.get(group_key) else {
                    return Err(PackagedPresentationError::UnknownCombinationGroup(
                        group_key.clone(),
                    ));
                };
                if !kinds.insert(group.media_kind) {
                    return Err(PackagedPresentationError::DuplicateCombinationKind(
                        group.media_kind,
                    ));
                }
                presentable |= matches!(group.media_kind, MediaKind::Audio | MediaKind::Video);
                used_groups.insert(group_key.clone());
            }
            if !presentable {
                return Err(PackagedPresentationError::CombinationNotPresentable);
            }
        }
        for group in self.groups.iter() {
            if !used_groups.contains(&group.key) {
                return Err(PackagedPresentationError::UnusedGroup(group.key.clone()));
            }
        }
        Ok(())
    }
}

/// Output of starting a muxer: bytes and their topology are inseparable.
pub struct StartedMuxer {
    pub muxer: Box<dyn super::Muxer>,
    pub presentation: Arc<PackagedPresentation>,
}

/// All immutable planning inputs needed to construct a muxer publication.
#[derive(Clone, Copy, Debug)]
pub struct MuxerStartRequest<'a> {
    /// Retained admission media, validated before the presentation is exposed.
    pub presentation: &'a PresentationPlan,
    pub segmentation: &'a crate::segment::SegmentationPlan,
    pub time_anchor: SystemTime,
    pub events: &'a EventSink,
}

// Keep this import used in rustdoc/type navigation even though compatibility
// currently compares the format through RenditionConfig.
const _: Option<MediaSegmentFormat> = None;

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::SystemTime};

    use crate::{
        domain::{MediaKind, TrackId, fixtures::track},
        media::fixtures::presentation,
        mux::fixtures::RenditionBuilder,
    };

    use super::*;

    fn input() -> PresentationPlan {
        presentation(vec![track(0, MediaKind::Video), track(1, MediaKind::Audio)])
    }

    fn rendition(id: u32, track_id: u32, kind: MediaKind) -> PackagedRendition {
        RenditionBuilder::new(id, kind)
            .source_tracks(&[track_id])
            .build()
    }

    #[test]
    fn default_topology_combines_video_and_audio_alternatives()
    -> Result<(), PackagedPresentationError> {
        let input = input();
        let presentation = PackagedPresentation::with_default_topology(
            SystemTime::UNIX_EPOCH,
            &input,
            vec![
                rendition(0, 0, MediaKind::Video),
                rendition(1, 1, MediaKind::Audio),
                rendition(2, 1, MediaKind::Audio),
            ],
        )?;

        assert_eq!(presentation.groups.len(), 2);
        assert_eq!(presentation.groups[0].media_kind, MediaKind::Video);
        assert_eq!(presentation.groups[1].renditions.len(), 2);
        assert_eq!(presentation.combinations[0].groups.len(), 2);
        assert!(presentation.renditions[1].is_default);
        assert!(!presentation.renditions[2].is_default);
        Ok(())
    }

    #[test]
    fn validation_rejects_unknown_source_tracks_before_publication() {
        let input = input();
        let bad = rendition(0, 99, MediaKind::Video);

        assert_eq!(
            PackagedPresentation::with_default_topology(SystemTime::UNIX_EPOCH, &input, vec![bad],),
            Err(PackagedPresentationError::UnknownSourceTrack {
                rendition_id: PackagingRenditionId(0),
                track_id: TrackId(99),
            })
        );
    }

    #[test]
    fn compatibility_requires_exact_identity_codec_kind_and_container() {
        let original = rendition(0, 0, MediaKind::Video);
        let mut reconnected = original.clone();
        reconnected.packaging_rendition_id = PackagingRenditionId(9);
        assert!(original.compatible_with(&reconnected));

        reconnected.codecs = Arc::from("hvc1.2.4.L153.B0");
        assert!(!original.compatible_with(&reconnected));
    }
}
