//! Deciding what belongs in one media playlist, and in what order.
//!
//! The [`manifest`](crate::delivery::hls::manifest) writers know how each tag is
//! spelled; this decides which of them a given snapshot calls for. The split
//! matters most around the open segment: `EXT-X-DISCONTINUITY`, `EXT-X-MAP`,
//! and `EXT-X-PROGRAM-DATE-TIME` all describe the segment their parts belong
//! to, so they have to be emitted before its first `EXT-X-PART` — long before
//! the segment itself completes.

use std::{
    num::{NonZeroU8, NonZeroU64},
    sync::Arc,
    time::Duration,
};

use crate::{
    delivery::hls::{
        InitializationId, OpenSegment, RenditionSnapshot, StoredPart, StoredSegment,
        StoredSegmentKind, StreamSnapshot,
        manifest::{
            MediaPlaylistWriter, Part, PreloadHint, PreloadHintType, RenditionReport, Segment,
            ServerControl,
        },
        uri::{PlaylistUris, QUERYPARAM_VERSION, RenditionUris, TOKEN_QUERYPARAM},
    },
    domain::{RenditionId, TickTimestamp, Timebase},
};

use super::{
    PlaylistDelta, PlaylistPolicy, ProgramDateTimePolicy, ProjectionError,
    timing::program_date_time,
};

/// Version 6 is the floor for `EXT-X-MAP` in a playlist of media segments.
const VERSION_WITH_MAP: u8 = 6;
/// Version 9 is the floor for partial segments and everything built on them.
const VERSION_WITH_PARTS: u8 = 9;
/// Version 9 is also the floor for `EXT-X-SKIP`.
const VERSION_WITH_SKIP: u8 = 9;
/// Version 10 is the floor for `RECENTLY-REMOVED-DATERANGES` on that tag.
const VERSION_WITH_SKIP_DATERANGES: u8 = 10;

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
    delta: PlaylistDelta,
) -> Result<String, ProjectionError> {
    let contract = rendition.contract;
    let names = uris.within(rendition.rendition_id, contract.segment_format);
    let timebase = rendition
        .config
        .ok_or(ProjectionError::RenditionUnconfigured {
            rendition_id: rendition.rendition_id,
        })?
        .timebase;

    let skip_count = skipped_parents(rendition, server_control, delta, timebase);
    let mut out = String::with_capacity(estimated_size(rendition));
    let mut writer = MediaPlaylistWriter::new(&mut out)?;
    writer.version(version(
        contract.is_chunked(),
        (skip_count > 0).then_some(delta),
        uris.query_variables(),
    ))?;
    if uris.query_variables() {
        writer.define_queryparam(TOKEN_QUERYPARAM)?;
    }
    writer.target_duration(contract.target_duration)?;
    if let Some(control) = server_control {
        writer.server_control(control)?;
    }
    if let Some(part_target) = contract.part_target {
        writer.part_information(part_target)?;
    }
    // MEDIA-SEQUENCE is the window head, including skipped parents.
    // SKIPPED-SEGMENTS is how a client reconstructs that prefix.
    writer.media_sequence(rendition.media_sequence)?;
    if rendition.discontinuity_sequence > 0 {
        writer.discontinuity_sequence(rendition.discontinuity_sequence)?;
    }

    // Tracks what the playlist has already said, so EXT-X-MAP is repeated only
    // where it actually changes and PROGRAM-DATE-TIME is not restated on every
    // segment unless asked for.
    let mut state = Emitted::default();
    let mut uri = String::new();
    let parents: &[Arc<StoredSegment>] = &rendition.segments;
    let (skipped, remaining) = parents.split_at(skip_count);
    // Walk the omitted prefix so discontinuity / PDT / in-effect MAP match the
    // full playlist. The first survivor is not the playlist head.
    for segment in skipped {
        observe_parent(&mut state, segment, names.has_initialization());
    }
    if let Some(count) = NonZeroU64::new(u64::try_from(skip_count).unwrap_or(0)) {
        writer.skipped_segments(count, delta.recently_removed_dateranges())?;
        // Re-emit the MAP in effect. A tail cut that omits it is merge-correct
        // (the MAP sat on skipped parents) and validator-hostile.
        state.initialization = None;
    }
    for segment in remaining {
        write_parent_tags(
            &mut writer,
            &mut state,
            segment.as_ref().into(),
            stream,
            rendition,
            &names,
            timebase,
            policy,
            &mut uri,
        )?;
        for part in rendition.segments.parts(segment) {
            write_part(&mut writer, part, &names, timebase, &mut uri)?;
        }
        writer.segment(Segment {
            uri: names.segment(segment.id, &mut uri),
            duration: timebase.ticks_to_duration(segment.duration),
            gap: matches!(segment.kind, StoredSegmentKind::Gap),
        })?;
    }

    if let Some(open) = &rendition.open_segment {
        write_parent_tags(
            &mut writer,
            &mut state,
            open.into(),
            stream,
            rendition,
            &names,
            timebase,
            policy,
            &mut uri,
        )?;
        for part in &open.parts {
            write_part(&mut writer, part, &names, timebase, &mut uri)?;
        }
    }

    // A preload hint names media that does not exist yet, which is exactly what
    // lets a client have its request already in flight when it does.
    if let Some(next) = rendition.live_edge.next_part_id {
        writer.preload_hint(PreloadHint {
            hint_type: PreloadHintType::Part,
            uri: names.part(next, &mut uri),
        })?;
    }

    write_rendition_reports(
        &mut writer,
        stream,
        rendition.rendition_id,
        &names,
        &mut uri,
    )?;

    if rendition.live_edge.ended {
        writer.endlist()?;
    }
    Ok(out)
}

/// What the playlist says about a parent segment rather than about its media.
///
/// A completed segment and a still-open one carry the identical set — that is
/// the whole reason the open one is described at all — and differ only in what
/// requires knowing how long the segment turned out to be. Reducing both to
/// these four values is what lets one function write the tags for either.
#[derive(Clone, Copy)]
struct ParentSegment {
    publication: u64,
    initialization: InitializationId,
    media_start: TickTimestamp,
    discontinuity_before: bool,
}

impl From<&StoredSegment> for ParentSegment {
    fn from(segment: &StoredSegment) -> Self {
        Self {
            publication: segment.publication,
            initialization: segment.initialization,
            media_start: segment.media_start,
            discontinuity_before: segment.discontinuity_before,
        }
    }
}

impl From<&OpenSegment> for ParentSegment {
    fn from(open: &OpenSegment) -> Self {
        Self {
            publication: open.publication,
            initialization: open.initialization,
            media_start: open.media_start,
            discontinuity_before: open.discontinuity_before,
        }
    }
}

#[derive(Default)]
struct Emitted {
    initialization: Option<InitializationId>,
    any_segment: bool,
}

/// Records a parent that will not be written, so later tags see the same
/// in-effect MAP and PDT state the full playlist would have.
fn observe_parent(state: &mut Emitted, segment: &StoredSegment, has_initialization: bool) {
    if has_initialization {
        state.initialization = Some(segment.initialization);
    }
    state.any_segment = true;
}

/// How many completed parents a delta may replace with `EXT-X-SKIP`.
///
/// Skip is refused on a terminal playlist, when nothing was advertised, and
/// when the client did not ask for a delta. A short live window still
/// advertises `CAN-SKIP-UNTIL` and produces a no-op (no `EXT-X-SKIP`).
fn skipped_parents(
    rendition: &RenditionSnapshot,
    server_control: Option<ServerControl>,
    delta: PlaylistDelta,
    timebase: Timebase,
) -> usize {
    if matches!(delta, PlaylistDelta::Full) || rendition.live_edge.ended {
        return 0;
    }
    let Some(until) = server_control.and_then(|control| control.can_skip_until) else {
        return 0;
    };
    skipped_parent_count(&rendition.segments, until, timebase)
}

/// Parents whose end is further than `can_skip_until` from the last completed
/// parent's end.
///
/// Distance is playlist time (`EXTINF`), not the LL-HLS live edge: open parts
/// are not a Media Segment. The last parent is never skipped, and a parent
/// whose end lands exactly on the boundary stays — the spec replaces only
/// what is further than the advertised skip. Parts are not segments.
fn skipped_parent_count(
    segments: &[Arc<StoredSegment>],
    can_skip_until: Duration,
    timebase: Timebase,
) -> usize {
    let mut parents = segments.iter().rev();
    let Some(mut closer) = parents.next() else {
        return 0;
    };
    let mut distance = Duration::ZERO;
    let mut kept = 1;
    for parent in parents {
        distance = distance.saturating_add(timebase.ticks_to_duration(closer.duration));
        if distance > can_skip_until {
            break;
        }
        kept += 1;
        closer = parent;
    }
    segments.len().saturating_sub(kept)
}

/// Writes the tags that describe a parent segment rather than its media.
///
/// Emitted for the open segment too, before its first part: a client that saw
/// parts without them would decode the successor's media against the previous
/// publication's initialization and timeline.
#[allow(clippy::too_many_arguments)]
fn write_parent_tags(
    writer: &mut MediaPlaylistWriter<'_>,
    state: &mut Emitted,
    segment: ParentSegment,
    stream: &StreamSnapshot,
    rendition: &RenditionSnapshot,
    names: &RenditionUris<'_>,
    timebase: Timebase,
    policy: &PlaylistPolicy,
    uri: &mut String,
) -> Result<(), ProjectionError> {
    // The tag stays printed for as long as its segment is visible, including
    // at the window head; EXT-X-DISCONTINUITY-SEQUENCE counts only the ones
    // that have already left.
    if segment.discontinuity_before {
        writer.discontinuity()?;
    }

    if names.has_initialization() {
        let initialization = segment.initialization;
        if state.initialization != Some(initialization) {
            let held = rendition.initialization_for(initialization).ok_or(
                ProjectionError::InitializationMissing {
                    rendition_id: rendition.rendition_id,
                    initialization,
                },
            )?;
            writer.initialization_map(
                names
                    .initialization(held.id, uri)
                    .ok_or(ProjectionError::UnnameableResource)?,
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
        ProgramDateTimePolicy::AtDiscontinuities => first || segment.discontinuity_before,
    };
    if wanted
        && let Some(anchor) = stream.time_anchor(segment.publication)
        && let Some(time) = program_date_time(anchor, segment.media_start, timebase)
    {
        writer.program_date_time(time)?;
    }
    Ok(())
}

fn write_part(
    writer: &mut MediaPlaylistWriter<'_>,
    part: &StoredPart,
    names: &RenditionUris<'_>,
    timebase: Timebase,
    uri: &mut String,
) -> Result<(), ProjectionError> {
    writer.part(Part {
        uri: names.part(part.id, uri),
        duration: timebase.ticks_to_duration(part.duration),
        independent: part.independent,
        gap: false,
    })?;
    Ok(())
}

/// Reports every *other* rendition's live edge, so a client switching between
/// them can request the right position without a blind reload first.
fn write_rendition_reports(
    writer: &mut MediaPlaylistWriter<'_>,
    stream: &StreamSnapshot,
    self_id: RenditionId,
    names: &RenditionUris<'_>,
    uri: &mut String,
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
            uri: names.sibling_playlist(entry.rendition_id, entry.media.kind(), uri),
            last_media_sequence,
            last_part,
        })?;
    }
    Ok(())
}

fn version(chunked: bool, skip: Option<PlaylistDelta>, query_variables: bool) -> NonZeroU8 {
    let mut version = if chunked {
        VERSION_WITH_PARTS
    } else {
        VERSION_WITH_MAP
    };
    match skip {
        Some(PlaylistDelta::Skip) => version = version.max(VERSION_WITH_SKIP),
        Some(PlaylistDelta::SkipV2) => version = version.max(VERSION_WITH_SKIP_DATERANGES),
        None | Some(PlaylistDelta::Full) => {}
    }
    if query_variables {
        version = version.max(QUERYPARAM_VERSION);
    }
    NonZeroU8::new(version).unwrap_or(NonZeroU8::MIN)
}

/// A rough allocation so a busy playlist is not rebuilt through a dozen growths.
fn estimated_size(rendition: &RenditionSnapshot) -> usize {
    const PER_SEGMENT: usize = 96;
    const PER_PART: usize = 72;
    let parts: usize = rendition
        .segments
        .iter()
        .map(|segment| rendition.segments.parts(segment).len())
        .sum::<usize>()
        + rendition
            .open_segment
            .as_ref()
            .map_or(0, |open| open.parts.len());
    512 + rendition.segments.len() * PER_SEGMENT + parts * PER_PART
}
