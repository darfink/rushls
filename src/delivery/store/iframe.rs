//! Index every fragment-opening random-access sample while completed CMAF media is in RAM.

use std::num::NonZeroU64;

use crate::{domain::MediaKind, mux::MediaSegmentFormat};

/// One independently decodable byte range and its position in the parent timeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IFrameRange {
    pub offset: u64,
    pub length: NonZeroU64,
    pub start: u64,
}

/// Index all fragments before spill. Each chunk supplies its start relative to
/// the parent; tfdt/CTS differences locate additional keyframes inside a chunk.
pub fn ranges<'a>(
    kind: MediaKind,
    format: MediaSegmentFormat,
    chunks: impl Iterator<Item = (&'a [u8], i64)>,
    duration: u64,
) -> std::sync::Arc<[IFrameRange]> {
    if kind != MediaKind::Video || format != MediaSegmentFormat::Cmaf {
        return [].into();
    }
    let mut frames = Vec::new();
    let mut base = 0u64;
    for (bytes, start) in chunks {
        let Some(mut indexed) = chunk_ranges(bytes, start, base) else {
            return [].into();
        };
        frames.append(&mut indexed);
        let Some(next) = u64::try_from(bytes.len())
            .ok()
            .and_then(|len| base.checked_add(len))
        else {
            return [].into();
        };
        base = next;
    }
    // A parent begins at a random-access boundary. If its media disagrees,
    // preserve its timeline as one GAP rather than silently shortening it.
    if frames.first().is_none_or(|frame| frame.start != 0)
        || frames.last().is_some_and(|frame| frame.start >= duration)
        || frames.windows(2).any(|pair| pair[0].start >= pair[1].start)
    {
        return [].into();
    }
    frames.into()
}

fn chunk_ranges(bytes: &[u8], start: i64, base: u64) -> Option<Vec<IFrameRange>> {
    let mut offset = 0;
    let mut origin = None;
    let mut frames = Vec::new();
    while offset < bytes.len() {
        let (atom, size) = transmux::parse_box(bytes.get(offset..)?).ok()?;
        if atom.header.box_type.is(b"moof") {
            let fragment = transmux::MovieFragmentBox::parse_body(atom.body).ok()?;
            let [track] = fragment.traf.as_slice() else {
                return None;
            };
            let sample = track.trun.first()?.samples.first()?;
            let pts = i64::try_from(track.tfdt.as_ref()?.base_media_decode_time())
                .ok()?
                .checked_add(i64::from(
                    sample.sample_composition_time_offset.unwrap_or(0),
                ))?;
            let first = *origin.get_or_insert(pts);
            if let Some(length) =
                prefix_len(MediaKind::Video, MediaSegmentFormat::Cmaf, &bytes[offset..])
            {
                frames.push(IFrameRange {
                    offset: base.checked_add(u64::try_from(offset).ok()?)?,
                    length,
                    start: u64::try_from(start.checked_add(pts.checked_sub(first)?)?).ok()?,
                });
            }
        }
        offset = offset.checked_add(size)?;
    }
    Some(frames)
}

/// Returns the prefix through the first sample, only for a self-contained
/// video fragment. HLS draft section 3.3 permits an I-frame resource to omit
/// the remaining mdat bytes, while retaining the original fragment header.
/// Non-CMAF media and fragments without an opening sync sample have no range.
pub fn prefix_len(kind: MediaKind, format: MediaSegmentFormat, bytes: &[u8]) -> Option<NonZeroU64> {
    if kind != MediaKind::Video || format != MediaSegmentFormat::Cmaf {
        return None;
    }
    let mut offset = 0usize;
    let mut sample_end = None;
    while offset < bytes.len() {
        let (atom, size) = transmux::parse_box(bytes.get(offset..)?).ok()?;
        if atom.header.box_type.is(b"moof") {
            let fragment = transmux::MovieFragmentBox::parse_body(atom.body).ok()?;
            let [track] = fragment.traf.as_slice() else {
                return None;
            };
            // Require movie-fragment-relative addressing and an explicit decode
            // time. These are guarantees of the CMAF muxer, not guesses based
            // on a part's INDEPENDENT flag (which can describe a later sample).
            if track.tfhd.base_data_offset.is_some()
                || track.tfhd.flags & 0x0002_0000 == 0
                || track.tfdt.is_none()
            {
                return None;
            }
            let run = track.trun.first()?;
            let sample = run.samples.first()?;
            let flags = run
                .first_sample_flags
                .or(sample.sample_flags)
                .or(track.tfhd.default_sample_flags)?;
            if flags & 0x0001_0000 != 0 || (flags >> 24) & 3 != 2 {
                return None;
            }
            let length = sample.sample_size.or(track.tfhd.default_sample_size)?;
            if length == 0 {
                return None;
            }
            let start = offset.checked_add(usize::try_from(run.data_offset?).ok()?)?;
            sample_end = Some((start, start.checked_add(usize::try_from(length).ok()?)?));
        } else if atom.header.box_type.is(b"mdat") {
            let (start, end) = sample_end?;
            let body_start = offset.checked_add(size.checked_sub(atom.body.len())?)?;
            if start != body_start || end > offset.checked_add(size)? {
                return None;
            }
            return NonZeroU64::new(u64::try_from(end).ok()?);
        } else if atom.header.box_type.is(b"moov") || atom.header.box_type.is(b"ftyp") {
            // An initialization section must never be part of the byte range.
            return None;
        }
        offset = offset.checked_add(size)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        delivery::hls::fixtures::cmaf_fragment,
        mux::fixtures::{H264_IDR, H264_P},
    };

    #[test]
    fn multiple_fragments_use_composition_times_and_absolute_byte_offsets()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::delivery::hls::fixtures::cmaf_fragment_at;
        let first = cmaf_fragment_at(true, 100, 2)?;
        let second = cmaf_fragment_at(true, 104, -1)?;
        let mut bytes = first.as_bytes().to_vec();
        bytes.extend_from_slice(second.as_bytes());
        let frames = ranges(
            MediaKind::Video,
            MediaSegmentFormat::Cmaf,
            std::iter::once((bytes.as_slice(), 0)),
            6,
        );
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].start, 0);
        assert_eq!(frames[1].start, 1);
        assert_eq!(
            frames[1].offset,
            u64::try_from(first.len())? + frames[0].offset
        );
        for frame in frames.iter() {
            let start = usize::try_from(frame.offset)?;
            let end = start + usize::try_from(frame.length.get())?;
            assert_eq!(&bytes[start + 4..start + 8], b"moof");
            assert_eq!(&bytes[end - H264_IDR.len()..end], H264_IDR);
        }
        Ok(())
    }

    #[test]
    fn iframe_prefix_contains_headers_and_exactly_the_opening_sample()
    -> Result<(), Box<dyn std::error::Error>> {
        let payload = cmaf_fragment(true)?;
        let length = prefix_len(
            MediaKind::Video,
            MediaSegmentFormat::Cmaf,
            payload.as_bytes(),
        )
        .ok_or("missing I-frame index")?;
        let end = usize::try_from(length.get())?;
        assert_eq!(end, payload.len() - H264_P.len());
        assert_eq!(&payload.as_bytes()[end - H264_IDR.len()..end], H264_IDR);
        // An INDEPENDENT flag from the segmenter alone must not authorize a range.
        let dependent = cmaf_fragment(false)?;
        assert_eq!(
            prefix_len(
                MediaKind::Video,
                MediaSegmentFormat::Cmaf,
                dependent.as_bytes()
            ),
            None
        );
        assert_eq!(
            prefix_len(
                MediaKind::Audio,
                MediaSegmentFormat::Cmaf,
                payload.as_bytes()
            ),
            None
        );
        assert_eq!(
            prefix_len(
                MediaKind::Video,
                MediaSegmentFormat::MpegTs,
                payload.as_bytes()
            ),
            None
        );
        for truncated in 0..payload.len() {
            assert_eq!(
                prefix_len(
                    MediaKind::Video,
                    MediaSegmentFormat::Cmaf,
                    &payload.as_bytes()[..truncated]
                ),
                None
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;
    use broadcast_common::Parse;

    /// The range is intentionally shorter than the mdat's declared size, as
    /// draft section 3.3 permits. Exercise a decoder as well as the box index.
    #[test]
    #[ignore = "requires ffmpeg on PATH"]
    fn iframe_prefix_decodes_to_the_same_frame_as_the_full_fragment()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{delivery::hls::fixtures::cmaf_fragment, mux::fixtures::H264_EXTRADATA};
        let config = transmux::CodecConfig::Avc {
            config: transmux::AVCConfigurationBox::new(
                transmux::AVCDecoderConfigurationRecord::parse(H264_EXTRADATA)?,
            ),
            width: 16,
            height: 16,
        };
        let init = transmux::build_init_segment(&[transmux::TrackSpec::new(1, 2, config)], 2)?;
        let fragment = cmaf_fragment(true)?;
        let mut payload = Vec::new();
        for decode in [0, 2, 4] {
            payload.extend_from_slice(
                crate::delivery::hls::fixtures::cmaf_fragment_at(true, decode, 0)?.as_bytes(),
            );
        }
        let indexed = ranges(
            MediaKind::Video,
            MediaSegmentFormat::Cmaf,
            std::iter::once((payload.as_slice(), 0)),
            6,
        );
        assert_eq!(indexed.len(), 3);
        let mut inputs = vec![fragment.as_bytes()];
        for frame in indexed.iter() {
            let start = usize::try_from(frame.offset)?;
            inputs.push(&payload[start..start + usize::try_from(frame.length.get())?]);
        }
        let mut frames = Vec::new();
        for bytes in inputs {
            let path =
                std::env::temp_dir().join(format!("rushls-iframe-{}.mp4", uuid::Uuid::now_v7()));
            std::fs::write(&path, [init.as_slice(), bytes].concat())?;
            let decoded = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args([
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "pipe:1",
                ])
                .output();
            std::fs::remove_file(path)?;
            let decoded = decoded?;
            assert!(
                decoded.status.success(),
                "{}",
                String::from_utf8_lossy(&decoded.stderr)
            );
            assert_eq!(decoded.stdout.len(), 16 * 16 * 3);
            frames.push(decoded.stdout);
        }
        assert!(frames.windows(2).all(|pair| pair[0] == pair[1]));
        Ok(())
    }
}
