//! The presentation a player chooses from.
//!
//! Two things here are easy to get subtly wrong and expensive to notice later.
//!
//! `BANDWIDTH` describes what playing a variant actually costs, which is the
//! primary rendition *plus one selectable rendition from every group it
//! references* — the largest such sum, since the client may pick any of them.
//! Advertising only the video rate makes every ABR decision optimistic by
//! exactly the audio bitrate.
//!
//! `CODECS` must list every codec that can appear while playing the variant,
//! including in alternates the client might switch to. A client that admits a
//! variant on its codec list and then meets an unlisted one has already
//! committed.

use std::{collections::BTreeSet, fmt::Write, num::NonZeroU64};

use crate::{
    delivery::hls::{
        RenditionCatalogEntry, StreamSnapshot,
        manifest::{MultivariantPlaylistWriter, PlaylistMediaType, Rendition, Variant, VideoRange},
    },
    domain::{MediaKind, RenditionId},
    mux::{RenditionGroupKey, RenditionMedia, VideoRange as MuxVideoRange},
};

use super::{PlaylistPolicy, ProjectionError, uri::PlaylistUris};

/// Version 6 covers everything this projection emits.
const VERSION: u8 = 6;

/// Renders the multivariant playlist, or `None` if there is no topology yet.
///
/// Absence means the stream has never had a publisher attach, not that it is
/// unready: bandwidth is always advertisable thanks to
/// [`PlaylistPolicy::assumed_bandwidth`], so a presentation is servable from
/// its first instant rather than after its first measured segment. Waiting for
/// media would delay every player's startup by a segment to improve one
/// attribute that ABR corrects within a segment anyway.
pub fn multivariant_playlist(
    stream: &StreamSnapshot,
    policy: &PlaylistPolicy,
    uris: &PlaylistUris,
) -> Result<Option<String>, ProjectionError> {
    let Some(presentation) = &stream.presentation else {
        return Ok(None);
    };
    let groups: Vec<ResolvedGroup<'_>> = presentation
        .groups
        .iter()
        .filter_map(|group| {
            ResolvedGroup::resolve(
                group.key.clone(),
                group.media_kind,
                &group.renditions,
                stream,
            )
        })
        .collect();
    if groups.is_empty() {
        return Ok(None);
    }

    let mut out = String::with_capacity(512);
    let mut writer = MultivariantPlaylistWriter::new(&mut out)?;
    writer.version(VERSION.try_into().unwrap_or(std::num::NonZeroU8::MIN))?;
    // Every segment this origin publishes begins at a boundary that is
    // independently decodable in its own rendition, which is what this asserts
    // and what makes switching between variants seamless.
    writer.independent_segments()?;

    // A group is "alternate" if some combination uses it without it being that
    // combination's primary; those are the ones that become EXT-X-MEDIA.
    let mut alternates = BTreeSet::new();
    for combination in presentation.combinations.iter() {
        let referenced: Vec<&ResolvedGroup<'_>> = combination
            .groups
            .iter()
            .filter_map(|key| groups.iter().find(|group| &group.key == key))
            .collect();
        if let Some(primary) = primary_group(&referenced) {
            for group in &referenced {
                if group.key != primary.key {
                    alternates.insert(group.key.clone());
                }
            }
        }
    }

    for group in groups
        .iter()
        .filter(|group| alternates.contains(&group.key))
    {
        for entry in &group.renditions {
            writer.rendition(Rendition {
                media_type: media_type(group.media_kind),
                group_id: &group.key.0,
                name: &entry.name,
                language: entry.language.as_deref(),
                sample_rate: sample_rate(&entry.media),
                channels: channels(&entry.media),
                default: entry.is_default,
                // Anything the origin publishes is a legitimate automatic
                // choice; a rendition nobody should select automatically would
                // not be in the topology.
                autoselect: true,
                uri: Some(&uris.in_multivariant(entry.rendition_id, group.media_kind)),
            })?;
        }
    }

    let mut written = BTreeSet::new();
    for combination in presentation.combinations.iter() {
        let referenced: Vec<&ResolvedGroup<'_>> = combination
            .groups
            .iter()
            .filter_map(|key| groups.iter().find(|group| &group.key == key))
            .collect();
        let Some(primary) = primary_group(&referenced) else {
            continue;
        };
        let alternate_groups: Vec<&&ResolvedGroup<'_>> = referenced
            .iter()
            .filter(|group| group.key != primary.key)
            .collect();

        for entry in &primary.renditions {
            // One combination per variant line: the same rendition reached
            // through two combinations is one variant, not two.
            if !written.insert(entry.rendition_id) {
                continue;
            }
            write_variant(&mut writer, entry, &alternate_groups, policy, uris)?;
        }
    }

    Ok(Some(out))
}

fn write_variant<W: Write + ?Sized>(
    writer: &mut MultivariantPlaylistWriter<'_, W>,
    primary: &RenditionCatalogEntry,
    alternates: &[&&ResolvedGroup<'_>],
    policy: &PlaylistPolicy,
    uris: &PlaylistUris,
) -> Result<(), ProjectionError> {
    let mut bandwidth = effective_bandwidth(primary, policy);
    // Only the *largest* selectable alternate from each group counts: the
    // client picks one per group, so summing them all would advertise a cost
    // no playback ever incurs.
    for group in alternates {
        if let Some(most) = group
            .renditions
            .iter()
            .map(|entry| effective_bandwidth(entry, policy))
            .max()
        {
            bandwidth = bandwidth.saturating_add(most.get());
        }
    }

    // An average is only meaningful if every contributor has measured one;
    // mixing a measured video rate with an assumed audio one would produce a
    // number describing nothing.
    let average = average_bandwidth(primary, alternates);

    let mut codecs = String::new();
    let mut seen = BTreeSet::new();
    for value in std::iter::once(primary.codecs.as_ref()).chain(
        alternates
            .iter()
            .flat_map(|group| group.renditions.iter().map(|entry| entry.codecs.as_ref())),
    ) {
        for codec in value
            .split(',')
            .map(str::trim)
            .filter(|codec| !codec.is_empty())
        {
            if seen.insert(codec.to_owned()) {
                if !codecs.is_empty() {
                    codecs.push(',');
                }
                codecs.push_str(codec);
            }
        }
    }

    let group_id = |kind: MediaKind| {
        alternates
            .iter()
            .find(|group| group.media_kind == kind)
            .map(|group| group.key.0.as_ref())
    };
    let (resolution, frame_rate, video_range) = match &primary.media {
        RenditionMedia::Video {
            width,
            height,
            frame_rate,
            video_range,
        } => (
            Some((*width, *height)),
            *frame_rate,
            video_range.map(video_range_of),
        ),
        RenditionMedia::Audio { .. } | RenditionMedia::Subtitle => (None, None, None),
    };

    writer.variant(Variant {
        bandwidth,
        average_bandwidth: average,
        codecs: (!codecs.is_empty()).then_some(codecs.as_str()),
        resolution,
        frame_rate,
        video_range,
        video_group_id: group_id(MediaKind::Video),
        audio_group_id: group_id(MediaKind::Audio),
        subtitle_group_id: group_id(MediaKind::Subtitle),
        uri: &uris.in_multivariant(primary.rendition_id, primary.media.kind()),
    })?;
    Ok(())
}

/// What playing this rendition should be assumed to cost.
///
/// The largest of what was measured, what the publisher declared, and the
/// configured assumption. Measurement wins when it exists, but a declaration
/// that turned out optimistic must not survive contact with a peak that
/// exceeded it — under-advertising is what makes a client choose a variant it
/// cannot sustain.
fn effective_bandwidth(entry: &RenditionCatalogEntry, policy: &PlaylistPolicy) -> NonZeroU64 {
    let measured = entry.bandwidth.peak_bits_per_second.unwrap_or(0);
    let declared = entry.declared_bandwidth.unwrap_or(0);
    NonZeroU64::new(measured.max(declared)).unwrap_or(policy.assumed_bandwidth)
}

fn average_bandwidth(
    primary: &RenditionCatalogEntry,
    alternates: &[&&ResolvedGroup<'_>],
) -> Option<NonZeroU64> {
    let mut total = primary.bandwidth.average_bits_per_second?;
    for group in alternates {
        let most = group
            .renditions
            .iter()
            .map(|entry| entry.bandwidth.average_bits_per_second)
            .max()
            .flatten()?;
        total = total.saturating_add(most);
    }
    NonZeroU64::new(total)
}

/// The group whose renditions become variant lines.
///
/// Video when the combination has it, audio otherwise. Everything else the
/// combination references is an alternate the client selects alongside.
fn primary_group<'a, 'b>(referenced: &[&'a ResolvedGroup<'b>]) -> Option<&'a ResolvedGroup<'b>> {
    referenced
        .iter()
        .find(|group| group.media_kind == MediaKind::Video)
        .or_else(|| {
            referenced
                .iter()
                .find(|group| group.media_kind == MediaKind::Audio)
        })
        .copied()
}

/// One topology group with its active renditions resolved to catalog entries.
struct ResolvedGroup<'a> {
    key: RenditionGroupKey,
    media_kind: MediaKind,
    renditions: Vec<&'a RenditionCatalogEntry>,
}

impl<'a> ResolvedGroup<'a> {
    fn resolve(
        key: RenditionGroupKey,
        media_kind: MediaKind,
        members: &[RenditionId],
        stream: &'a StreamSnapshot,
    ) -> Option<Self> {
        let renditions: Vec<&RenditionCatalogEntry> = members
            .iter()
            .filter_map(|id| {
                stream
                    .renditions
                    .iter()
                    .find(|entry| entry.rendition_id == *id && entry.active)
            })
            .collect();
        // A group whose renditions have all retired describes nothing a client
        // could select, and naming it would leave a dangling group reference.
        (!renditions.is_empty()).then_some(Self {
            key,
            media_kind,
            renditions,
        })
    }
}

fn media_type(kind: MediaKind) -> PlaylistMediaType {
    match kind {
        MediaKind::Audio => PlaylistMediaType::Audio,
        MediaKind::Video => PlaylistMediaType::Video,
        MediaKind::Subtitle => PlaylistMediaType::Subtitles,
    }
}

fn video_range_of(range: MuxVideoRange) -> VideoRange {
    match range {
        MuxVideoRange::Sdr => VideoRange::Sdr,
        MuxVideoRange::Hlg => VideoRange::Hlg,
        MuxVideoRange::Pq => VideoRange::Pq,
    }
}

fn sample_rate(media: &RenditionMedia) -> Option<std::num::NonZeroU32> {
    match media {
        RenditionMedia::Audio { sample_rate, .. } => Some(*sample_rate),
        _ => None,
    }
}

fn channels(media: &RenditionMedia) -> Option<std::num::NonZeroU16> {
    match media {
        RenditionMedia::Audio { channels, .. } => Some(*channels),
        _ => None,
    }
}
