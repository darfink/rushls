//! Deciding what belongs in one media playlist, and in what order.
//!
//! The [`manifest`](crate::delivery::hls::manifest) writers know how each tag is
//! spelled; this decides which of them a given snapshot calls for. The split
//! matters most around the open segment: `EXT-X-DISCONTINUITY`, `EXT-X-MAP`,
//! and `EXT-X-PROGRAM-DATE-TIME` all describe the segment their parts belong
//! to, so they have to be emitted before its first `EXT-X-PART` — long before
//! the segment itself completes.

use std::{fmt::Write, num::NonZeroU8};

use crate::{
    delivery::hls::{
        InitializationId, OpenSegment, RenditionSnapshot, StoredPart, StoredSegment,
        StoredSegmentKind, StreamSnapshot,
        manifest::{
            MediaPlaylistWriter, Part, PreloadHint, PreloadHintType, RenditionReport, Segment,
            ServerControl,
        },
    },
    domain::{RenditionId, TickTimestamp, Timebase},
};

use super::{
    PlaylistPolicy, ProgramDateTimePolicy, ProjectionError,
    timing::program_date_time,
    uri::{PlaylistUris, Resource, ResourceNaming},
};

/// Version 6 is the floor for `EXT-X-MAP` in a playlist of media segments.
const VERSION_WITH_MAP: u8 = 6;
/// Version 9 is the floor for partial segments and everything built on them.
const VERSION_WITH_PARTS: u8 = 9;

/// Renders one rendition's media playlist.
///
/// `server_control` is supplied rather than derived because it is a property of
/// the whole presentation: every playlist under one multivariant playlist has
/// to carry the identical value, which no single rendition can work out alone.
pub fn media_playlist(
    stream: &StreamSnapshot,
    rendition: &RenditionSnapshot,
    server_control: Option<ServerControl>,
    policy: &PlaylistPolicy,
    uris: &PlaylistUris,
) -> Result<String, ProjectionError> {
    let contract = rendition.contract;
    let names = Names {
        uris,
        naming: ResourceNaming::for_format(contract.segment_format),
    };
    let timebase = rendition
        .config
        .ok_or(ProjectionError::RenditionUnconfigured {
            rendition_id: rendition.rendition_id,
        })?
        .timebase;

    let mut out = String::with_capacity(estimated_size(rendition));
    let mut writer = MediaPlaylistWriter::new(&mut out)?;
    writer.version(version(contract.is_chunked()))?;
    writer.target_duration(contract.target_duration)?;
    if let Some(control) = server_control {
        writer.server_control(control)?;
    }
    if let Some(part_target) = contract.part_target {
        writer.part_information(part_target)?;
    }
    writer.media_sequence(rendition.media_sequence)?;
    if rendition.discontinuity_sequence > 0 {
        writer.discontinuity_sequence(rendition.discontinuity_sequence)?;
    }

    // Tracks what the playlist has already said, so EXT-X-MAP is repeated only
    // where it actually changes and PROGRAM-DATE-TIME is not restated on every
    // segment unless asked for.
    let mut state = Emitted::default();
    for segment in &rendition.segments {
        write_parent_tags(
            &mut writer,
            &mut state,
            ParentSegment::Completed(segment),
            stream,
            rendition,
            &names,
            timebase,
            policy,
        )?;
        for part in &segment.parts {
            write_part(&mut writer, rendition.rendition_id, part, &names, timebase)?;
        }
        writer.segment(Segment {
            uri: &names.name(Resource::Segment(rendition.rendition_id, segment.id))?,
            duration: timebase.ticks_to_duration(segment.duration),
            gap: matches!(segment.kind, StoredSegmentKind::Gap),
        })?;
    }

    if let Some(open) = &rendition.open_segment {
        write_parent_tags(
            &mut writer,
            &mut state,
            ParentSegment::Open(open),
            stream,
            rendition,
            &names,
            timebase,
            policy,
        )?;
        for part in &open.parts {
            write_part(&mut writer, rendition.rendition_id, part, &names, timebase)?;
        }
    }

    // A preload hint names media that does not exist yet, which is exactly what
    // lets a client have its request already in flight when it does.
    if let Some(next) = rendition.live_edge.next_part_id {
        writer.preload_hint(PreloadHint {
            hint_type: PreloadHintType::Part,
            uri: &names.name(Resource::Part(rendition.rendition_id, next))?,
        })?;
    }

    write_rendition_reports(&mut writer, stream, rendition.rendition_id, &names)?;

    if rendition.live_edge.ended {
        writer.endlist()?;
    }
    Ok(out)
}

/// Names one rendition's resources the way its own media playlist must.
///
/// The pair travels together everywhere below: the format decides how a
/// resource is spelled, and the stream's configured base decides where that
/// spelling is rooted.
struct Names<'a> {
    uris: &'a PlaylistUris,
    naming: ResourceNaming,
}

impl Names<'_> {
    fn name(&self, resource: Resource) -> Result<String, ProjectionError> {
        self.uris
            .in_media_playlist(self.naming, resource)
            .ok_or(ProjectionError::UnnameableResource)
    }

    fn has_initialization(&self) -> bool {
        self.naming.has_initialization()
    }
}

/// Whichever parent segment the playlist is currently describing.
///
/// The open one carries the same leading tags as a completed one — that is the
/// whole reason it is represented here at all — while differing in everything
/// that requires knowing how long it turned out to be.
enum ParentSegment<'a> {
    Completed(&'a StoredSegment),
    Open(&'a OpenSegment),
}

impl ParentSegment<'_> {
    fn publication(&self) -> u64 {
        match self {
            Self::Completed(segment) => segment.publication,
            Self::Open(open) => open.publication,
        }
    }

    fn initialization(&self) -> InitializationId {
        match self {
            Self::Completed(segment) => segment.initialization,
            Self::Open(open) => open.initialization,
        }
    }

    fn media_start(&self) -> TickTimestamp {
        match self {
            Self::Completed(segment) => segment.media_start,
            Self::Open(open) => open.media_start,
        }
    }

    fn discontinuity_before(&self) -> bool {
        match self {
            Self::Completed(segment) => segment.discontinuity_before,
            Self::Open(open) => open.discontinuity_before,
        }
    }
}

#[derive(Default)]
struct Emitted {
    initialization: Option<InitializationId>,
    any_segment: bool,
}

/// Writes the tags that describe a parent segment rather than its media.
///
/// Emitted for the open segment too, before its first part: a client that saw
/// parts without them would decode the successor's media against the previous
/// publication's initialization and timeline.
#[allow(clippy::too_many_arguments)]
fn write_parent_tags<W: Write + ?Sized>(
    writer: &mut MediaPlaylistWriter<'_, W>,
    state: &mut Emitted,
    segment: ParentSegment<'_>,
    stream: &StreamSnapshot,
    rendition: &RenditionSnapshot,
    names: &Names<'_>,
    timebase: Timebase,
    policy: &PlaylistPolicy,
) -> Result<(), ProjectionError> {
    // The tag stays printed for as long as its segment is visible, including
    // at the window head; EXT-X-DISCONTINUITY-SEQUENCE counts only the ones
    // that have already left.
    if segment.discontinuity_before() {
        writer.discontinuity()?;
    }

    if names.has_initialization() {
        let initialization = segment.initialization();
        if state.initialization != Some(initialization) {
            let held = rendition.initialization_for(initialization).ok_or(
                ProjectionError::InitializationMissing {
                    rendition_id: rendition.rendition_id,
                    initialization,
                },
            )?;
            writer.initialization_map(
                &names.name(Resource::Initialization(rendition.rendition_id, held.id))?,
            )?;
            state.initialization = Some(initialization);
        }
    }

    let first = !state.any_segment;
    state.any_segment = true;
    let wanted = match policy.program_date_time {
        ProgramDateTimePolicy::EverySegment => true,
        // One anchor per continuous range is all a client needs: it derives
        // every later segment's time by accumulating EXTINF from the last tag,
        // and a discontinuity is precisely where that accumulation restarts.
        ProgramDateTimePolicy::AtDiscontinuities => first || segment.discontinuity_before(),
    };
    if wanted
        && let Some(anchor) = stream.time_anchor(segment.publication())
        && let Some(time) = program_date_time(anchor, segment.media_start(), timebase)
    {
        writer.program_date_time(time)?;
    }
    Ok(())
}

fn write_part<W: Write + ?Sized>(
    writer: &mut MediaPlaylistWriter<'_, W>,
    rendition_id: RenditionId,
    part: &StoredPart,
    names: &Names<'_>,
    timebase: Timebase,
) -> Result<(), ProjectionError> {
    writer.part(Part {
        uri: &names.name(Resource::Part(rendition_id, part.id))?,
        duration: timebase.ticks_to_duration(part.duration),
        independent: part.independent,
        gap: false,
    })?;
    Ok(())
}

/// Reports every *other* rendition's live edge, so a client switching between
/// them can request the right position without a blind reload first.
fn write_rendition_reports<W: Write + ?Sized>(
    writer: &mut MediaPlaylistWriter<'_, W>,
    stream: &StreamSnapshot,
    self_id: RenditionId,
    names: &Names<'_>,
) -> Result<(), ProjectionError> {
    for entry in stream.renditions.iter() {
        if entry.rendition_id == self_id || !entry.active {
            continue;
        }
        let edge = entry.snapshot().live_edge;
        // A part's parent is in the playlist as soon as its parts are, so the
        // reported MSN follows the last part when there is one and the last
        // completed segment otherwise.
        let (last_media_sequence, last_part) = match (edge.last_part, edge.last_segment) {
            (Some((cursor, _)), _) => (Some(cursor.msn.0), Some(u64::from(cursor.part_index.0))),
            (None, Some((msn, _))) => (Some(msn.0), None),
            (None, None) => continue,
        };
        writer.rendition_report(RenditionReport {
            uri: &names.name(Resource::MediaPlaylist(
                entry.rendition_id,
                entry.media.kind(),
            ))?,
            last_media_sequence,
            last_part,
        })?;
    }
    Ok(())
}

fn version(chunked: bool) -> NonZeroU8 {
    let version = if chunked {
        VERSION_WITH_PARTS
    } else {
        VERSION_WITH_MAP
    };
    NonZeroU8::new(version).unwrap_or(NonZeroU8::MIN)
}

/// A rough allocation so a busy playlist is not rebuilt through a dozen growths.
fn estimated_size(rendition: &RenditionSnapshot) -> usize {
    const PER_SEGMENT: usize = 96;
    const PER_PART: usize = 72;
    let parts: usize = rendition
        .segments
        .iter()
        .map(|segment| segment.parts.len())
        .sum::<usize>()
        + rendition
            .open_segment
            .as_ref()
            .map_or(0, |open| open.parts.len());
    512 + rendition.segments.len() * PER_SEGMENT + parts * PER_PART
}
