//! Length-delimited AV1 OBUs used by RTMP and the AV1G TS mapping.
use scuffle_av1::{ObuHeader, ObuType, seq::SequenceHeaderObu};
use std::io::Cursor;

pub fn sequence(bytes: &[u8]) -> Option<SequenceHeaderObu> {
    let mut cursor = Cursor::new(bytes);
    while usize::try_from(cursor.position()).ok()? < bytes.len() {
        let header = ObuHeader::parse(&mut cursor).ok()?;
        let start = usize::try_from(cursor.position()).ok()?;
        let end = start.checked_add(usize::try_from(header.size?).ok()?)?;
        let body = bytes.get(start..end)?;
        if header.obu_type == ObuType::SequenceHeader {
            return SequenceHeaderObu::parse(header, &mut &body[..]).ok();
        }
        cursor.set_position(u64::try_from(end).ok()?);
    }
    None
}

/// A key frame requires a frame header, not merely a sequence header OBU.
pub fn keyframe(bytes: &[u8], reduced: bool) -> Option<bool> {
    let mut cursor = Cursor::new(bytes);
    while usize::try_from(cursor.position()).ok()? < bytes.len() {
        let header = ObuHeader::parse(&mut cursor).ok()?;
        let start = usize::try_from(cursor.position()).ok()?;
        let end = start.checked_add(usize::try_from(header.size?).ok()?)?;
        let body = bytes.get(start..end)?;
        if matches!(header.obu_type, ObuType::Frame | ObuType::FrameHeader) {
            let first = *body.first()?;
            if reduced {
                return Some(true);
            }
            // An existing or hidden frame is not a displayable random-access
            // boundary. Require show_frame as well as KEY_FRAME.
            return Some(first & 0xf0 == 0x10);
        }
        cursor.set_position(u64::try_from(end).ok()?);
    }
    None
}

/// Count shown pictures without treating hidden reference frames as intervals.
/// More complex temporal/spatial mappings remain explicitly unverified.
pub fn displayed_pictures(bytes: &[u8]) -> Option<usize> {
    let mut cursor = Cursor::new(bytes);
    let mut shown = 0;
    while usize::try_from(cursor.position()).ok()? < bytes.len() {
        let header = ObuHeader::parse(&mut cursor).ok()?;
        if header
            .extension_header
            .is_some_and(|extension| extension.temporal_id != 0 || extension.spatial_id != 0)
        {
            return None;
        }
        let start = usize::try_from(cursor.position()).ok()?;
        let end = start.checked_add(usize::try_from(header.size?).ok()?)?;
        let body = bytes.get(start..end)?;
        if matches!(header.obu_type, ObuType::Frame | ObuType::FrameHeader) {
            let first = *body.first()?;
            if first & 0x80 != 0 || first & 0x10 != 0 {
                shown += 1;
            }
        }
        cursor.set_position(u64::try_from(end).ok()?);
    }
    Some(shown)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_access_requires_a_shown_key_frame() {
        // OBU_FRAME, has_size_field, one-byte frame header prefix.
        assert_eq!(keyframe(&[0x32, 1, 0x10], false), Some(true));
        assert_eq!(keyframe(&[0x32, 1, 0x00], false), Some(false));
        assert_eq!(keyframe(&[0x32, 1, 0x30], false), Some(false));
        assert_eq!(keyframe(&[0x32, 1, 0x80], false), Some(false));
        assert_eq!(keyframe(&[0x32, 0], true), None);
        assert_eq!(keyframe(&[0x32, 2, 0x10], false), None);
        assert!(sequence(&[0x0a, 255]).is_none());
    }
}
