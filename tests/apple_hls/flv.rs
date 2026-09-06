//! Split a checked-in FLV into RTMP tag bodies.

use bytes::Bytes;
use cc_rtmp::{EnhancedValidationMode, ValidatedMedia};
use rushls::source::IngressEvent;

/// FLV tags as RTMP ingress events. Script `onMetaData` is dropped: the native
/// source does not need it, and unknown AMF must not be mistaken for captions.
pub fn ingress_events(flv: &[u8]) -> Result<Vec<IngressEvent>, String> {
    let mut events = Vec::new();
    for tag in tags(flv)? {
        match tag.kind {
            8 => events.push(IngressEvent::Audio {
                timestamp: tag.timestamp,
                media: ValidatedMedia::parse_audio(
                    Bytes::copy_from_slice(tag.payload),
                    EnhancedValidationMode::Strict,
                )
                .map_err(|error| error.to_string())?,
            }),
            9 => events.push(IngressEvent::Video {
                timestamp: tag.timestamp,
                media: ValidatedMedia::parse_video(
                    Bytes::copy_from_slice(tag.payload),
                    EnhancedValidationMode::Strict,
                )
                .map_err(|error| error.to_string())?,
            }),
            18 => {}
            other => return Err(format!("unsupported FLV tag type {other}")),
        }
    }
    if events.is_empty() {
        return Err("FLV contained no audio or video tags".into());
    }
    Ok(events)
}

/// Same media as [`ingress_events`], with every AAC tag duplicated as Enhanced
/// OneTrack ids 1 and 2 so Apple sees two audio renditions.
pub fn ingress_events_two_aac(flv: &[u8]) -> Result<Vec<IngressEvent>, String> {
    let mut events = Vec::new();
    for tag in tags(flv)? {
        match tag.kind {
            8 => {
                for track_id in [1_u8, 2] {
                    events.push(IngressEvent::Audio {
                        timestamp: tag.timestamp,
                        media: one_track_aac(tag.payload, track_id)?,
                    });
                }
            }
            9 => events.push(IngressEvent::Video {
                timestamp: tag.timestamp,
                media: ValidatedMedia::parse_video(
                    Bytes::copy_from_slice(tag.payload),
                    EnhancedValidationMode::Strict,
                )
                .map_err(|error| error.to_string())?,
            }),
            18 => {}
            other => return Err(format!("unsupported FLV tag type {other}")),
        }
    }
    Ok(events)
}

struct Tag<'a> {
    kind: u8,
    timestamp: u32,
    payload: &'a [u8],
}

fn tags(flv: &[u8]) -> Result<Vec<Tag<'_>>, String> {
    if flv.len() < 13 || &flv[..3] != b"FLV" {
        return Err("not an FLV file".into());
    }
    let mut offset = 13;
    let mut tags = Vec::new();
    while offset + 11 <= flv.len() {
        let kind = flv[offset];
        let size = u32::from_be_bytes([0, flv[offset + 1], flv[offset + 2], flv[offset + 3]]);
        let timestamp = u32::from_be_bytes([0, flv[offset + 4], flv[offset + 5], flv[offset + 6]])
            | (u32::from(flv[offset + 7]) << 24);
        let start = offset + 11;
        let end = start
            .checked_add(size as usize)
            .ok_or("FLV tag size overflow")?;
        if end + 4 > flv.len() {
            break;
        }
        tags.push(Tag {
            kind,
            timestamp,
            payload: &flv[start..end],
        });
        offset = end + 4;
    }
    Ok(tags)
}

fn one_track_aac(
    legacy: &[u8],
    track_id: u8,
) -> Result<cc_rtmp::ValidatedMedia<cc_rtmp::ParsedAudio>, String> {
    if legacy.len() < 2 || legacy[0] != 0xaf {
        return Err("expected a legacy AAC FLV audio tag".into());
    }
    let mut payload = Vec::with_capacity(7 + legacy.len().saturating_sub(2));
    payload.extend_from_slice(&[0x95, legacy[1], b'm', b'p', b'4', b'a', track_id]);
    payload.extend_from_slice(&legacy[2..]);
    ValidatedMedia::parse_audio(Bytes::from(payload), EnhancedValidationMode::Strict)
        .map_err(|error| error.to_string())
}
