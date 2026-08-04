//! Retained media represented without reassembling stored parts.
//!
//! A completed chunked segment lives as the parts that composed it. Keeping
//! those buffers separate lets every serving adapter hand them out by refcount
//! instead of copying a segment-sized allocation for every viewer.

use std::sync::Arc;

use bytes::Bytes;

use crate::{
    delivery::store::{SegmentBody, StoredPart, StoredSegment, StoredSegmentKind},
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

    pub fn from_segment(segment: &StoredSegment) -> Self {
        match &segment.kind {
            StoredSegmentKind::Media(SegmentBody::Contiguous(payload)) => {
                Self::single(payload.clone())
            }
            StoredSegmentKind::Media(SegmentBody::Chunked(parts)) => Self {
                frames: MediaFrames::Chunked(Arc::clone(parts)),
                length: parts.iter().map(|part| part.payload.len() as u64).sum(),
            },
            // A gap has no bytes by construction; it exists to keep a media
            // sequence number from vanishing, not to be fetched.
            StoredSegmentKind::Gap => Self::default(),
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
    #[must_use]
    pub fn range(&self, start: u64, end: u64) -> Self {
        let frames = match &self.frames {
            MediaFrames::Empty => Vec::new(),
            MediaFrames::Single(payload) => clip_range(std::iter::once(payload), start, end),
            MediaFrames::Chunked(parts) => {
                clip_range(parts.iter().map(|part| &part.payload), start, end)
            }
            MediaFrames::Ranged(frames) => clip_range(frames.iter(), start, end),
        };
        Self::ranged(frames)
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
            let to = (end + 1 - position).min(length);
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
                let bytes = parts.get(*index)?.payload.bytes().clone();
                *index += 1;
                Some(bytes)
            }
            Self::Ranged(frames) => frames.next().map(Payload::into_bytes),
        }
    }
}
