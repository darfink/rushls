//! Retained media represented without reassembling stored parts.
//!
//! A completed chunked segment lives as the parts that composed it. Keeping
//! those buffers separate lets every serving adapter hand them out by refcount
//! instead of copying a segment-sized allocation for every viewer.

use std::sync::Arc;

use bytes::Bytes;

use crate::{
    delivery::store::{HeldBytes, SegmentBody, StoredPart, StoredSegment, StoredSegmentKind},
    domain::Payload,
};

/// Bytes to send, in the buffers they were stored in.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaBody {
    frames: MediaFrames,
    length: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum MediaFrames {
    #[default]
    Empty,
    Single(Payload),
    Chunked(Arc<[Arc<StoredPart>]>),
    Ranged(Vec<Payload>),
}

impl MediaBody {
    pub fn single(payload: Payload) -> Self {
        let length = payload.len() as u64;
        Self {
            frames: MediaFrames::Single(payload),
            length,
        }
    }

    fn ranged(frames: Vec<Payload>) -> Self {
        let length = frames.iter().map(|frame| frame.len() as u64).sum();
        Self {
            frames: MediaFrames::Ranged(frames),
            length,
        }
    }

    pub(crate) fn from_payloads(frames: Vec<Payload>) -> Self {
        Self::ranged(frames)
    }

    /// Builds a body over in-memory frames only.
    ///
    /// `None` when any payload lives on disk: those bytes are not in these
    /// buffers, and a later range clip must not skip them. The origin loads
    /// disk frames into memory first.
    pub fn from_segment(segment: &StoredSegment) -> Option<Self> {
        match &segment.kind {
            StoredSegmentKind::Media(SegmentBody::Contiguous(HeldBytes::Memory(payload))) => {
                Some(Self::single(payload.clone()))
            }
            StoredSegmentKind::Media(SegmentBody::Chunked(parts))
                if parts.iter().all(|part| part.payload.is_memory()) =>
            {
                Some(Self {
                    frames: MediaFrames::Chunked(Arc::clone(parts)),
                    length: parts.iter().map(|part| part.payload.len() as u64).sum(),
                })
            }
            StoredSegmentKind::Media(
                SegmentBody::Contiguous(HeldBytes::Disk(_)) | SegmentBody::Chunked(_),
            )
            | StoredSegmentKind::Gap => None,
        }
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn into_frames(self) -> MediaFrameIter {
        match self.frames {
            MediaFrames::Empty => MediaFrameIter::Empty,
            MediaFrames::Single(payload) => MediaFrameIter::Single(Some(payload)),
            MediaFrames::Chunked(parts) => MediaFrameIter::Chunked { parts, index: 0 },
            MediaFrames::Ranged(frames) => MediaFrameIter::Ranged(frames.into_iter()),
        }
    }

    /// Clips to an inclusive byte range, splitting refcounted frames where the
    /// range falls inside one.
    ///
    /// `None` if a chunked frame is not in memory. Skipping it would shift
    /// every later offset while HTTP still advertised the full length.
    #[must_use]
    pub fn range(&self, start: u64, end: u64) -> Option<Self> {
        let frames = match &self.frames {
            MediaFrames::Empty => Vec::new(),
            MediaFrames::Single(payload) => clip_range(std::iter::once(payload), start, end),
            MediaFrames::Chunked(parts) => {
                let payloads = parts
                    .iter()
                    .map(|part| part.payload.as_memory())
                    .collect::<Option<Vec<_>>>()?;
                clip_range(payloads.into_iter(), start, end)
            }
            MediaFrames::Ranged(frames) => clip_range(frames.iter(), start, end),
        };
        Some(Self::ranged(frames))
    }
}

fn clip_range<'a>(frames: impl Iterator<Item = &'a Payload>, start: u64, end: u64) -> Vec<Payload> {
    let mut clipped = Vec::new();
    let mut position = 0_u64;
    for frame in frames {
        let length = frame.len() as u64;
        let frame_end = position + length;
        if frame_end > start && position <= end {
            let from = start.saturating_sub(position).min(length);
            // `end + 1` is the exclusive bound; saturating keeps an
            // end-of-representation range (`end == u64::MAX`) from wrapping
            // around to zero.
            let to = end.saturating_add(1).saturating_sub(position).min(length);
            if from >= to {
                continue;
            }
            // Frame payloads live in addressable memory, so these offsets fit.
            let from = usize::try_from(from).unwrap_or(usize::MAX);
            let to = usize::try_from(to).unwrap_or(usize::MAX);
            clipped.push(Payload::from_bytes(frame.bytes().slice(from..to)));
        }
        position = frame_end;
    }
    clipped
}

pub enum MediaFrameIter {
    Empty,
    Single(Option<Payload>),
    Chunked {
        parts: Arc<[Arc<StoredPart>]>,
        index: usize,
    },
    Ranged(std::vec::IntoIter<Payload>),
}

impl Iterator for MediaFrameIter {
    type Item = Bytes;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Empty => None,
            Self::Single(payload) => payload.take().map(Payload::into_bytes),
            Self::Chunked { parts, index } => {
                // Chunked bodies are all-RAM by construction (`from_segment`).
                let bytes = parts.get(*index)?.payload.as_memory()?.bytes().clone();
                *index += 1;
                Some(bytes)
            }
            Self::Ranged(frames) => frames.next().map(Payload::into_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        delivery::store::{
            DiskRef, InitializationId, Msn, PartCursor, PartId, PartIndex, SegmentId, StoredPart,
        },
        domain::{Payload, Timebase},
    };

    fn contiguous(payload: HeldBytes) -> StoredSegment {
        StoredSegment {
            id: SegmentId(1),
            msn: Msn(1),
            publication: 1,
            initialization: InitializationId(1),
            media_start: 0,
            duration: 1,
            timebase: Timebase::hz90k(),
            independent: true,
            discontinuity_before: false,
            kind: StoredSegmentKind::Media(SegmentBody::Contiguous(payload)),
            gzip: None,
        }
    }

    fn part(id: u64, payload: HeldBytes) -> Arc<StoredPart> {
        Arc::new(StoredPart {
            id: PartId(id),
            cursor: PartCursor {
                msn: Msn(1),
                part_index: PartIndex(u32::try_from(id).expect("fixture id")),
            },
            publication: 1,
            initialization: InitializationId(1),
            media_start: 0,
            duration: 1,
            timebase: Timebase::hz90k(),
            independent: id == 0,
            payload,
            gzip: None,
        })
    }

    fn chunked(payloads: Vec<HeldBytes>) -> StoredSegment {
        let parts: Arc<[Arc<StoredPart>]> = payloads
            .into_iter()
            .enumerate()
            .map(|(index, payload)| {
                part(
                    u64::try_from(index).expect("fixture part count fits u64"),
                    payload,
                )
            })
            .collect::<Vec<_>>()
            .into();
        StoredSegment {
            id: SegmentId(1),
            msn: Msn(1),
            publication: 1,
            initialization: InitializationId(1),
            media_start: 0,
            duration: 1,
            timebase: Timebase::hz90k(),
            independent: true,
            discontinuity_before: false,
            kind: StoredSegmentKind::Media(SegmentBody::Chunked(parts)),
            gzip: None,
        }
    }

    fn disk(len: usize) -> HeldBytes {
        HeldBytes::Disk(DiskRef {
            path: Arc::new(std::path::PathBuf::from("/nonexistent")),
            len,
        })
    }

    #[test]
    fn from_segment_refuses_disk_payloads() {
        assert!(
            MediaBody::from_segment(&contiguous(HeldBytes::Memory(Payload::from(&b"abcd"[..]))))
                .is_some()
        );
        assert!(MediaBody::from_segment(&contiguous(disk(4))).is_none());
        assert!(
            MediaBody::from_segment(&chunked(vec![
                HeldBytes::Memory(Payload::from(&b"ab"[..])),
                HeldBytes::Memory(Payload::from(&b"cd"[..])),
            ]))
            .is_some()
        );
        assert!(
            MediaBody::from_segment(&chunked(vec![
                HeldBytes::Memory(Payload::from(&b"ab"[..])),
                disk(2),
            ]))
            .is_none(),
            "a mixed parent cannot be served as chunked frames"
        );
        let mut gap = contiguous(HeldBytes::Memory(Payload::from(&b"x"[..])));
        gap.kind = StoredSegmentKind::Gap;
        assert!(MediaBody::from_segment(&gap).is_none());
    }

    #[test]
    fn range_of_memory_frames_keeps_offsets() {
        let chunked = MediaBody::from_segment(&chunked(vec![
            HeldBytes::Memory(Payload::from(&b"aaaa"[..])),
            HeldBytes::Memory(Payload::from(&b"bbbb"[..])),
            HeldBytes::Memory(Payload::from(&b"cccc"[..])),
        ]))
        .expect("all-RAM chunked body");
        let clipped = chunked.range(3, 8).expect("chunked RAM frames clip");
        let bytes: Vec<u8> = clipped.into_frames().flatten().collect();
        assert_eq!(bytes, b"abbbbc");

        let reassembled = MediaBody::from_payloads(vec![
            Payload::from(&b"aaaa"[..]),
            Payload::from(&b"bbbb"[..]),
            Payload::from(&b"cccc"[..]),
        ]);
        let clipped = reassembled.range(3, 8).expect("reassembled frames clip");
        let bytes: Vec<u8> = clipped.into_frames().flatten().collect();
        assert_eq!(bytes, b"abbbbc");
    }
}
