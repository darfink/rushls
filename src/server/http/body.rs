//! Sending retained media without reassembling it.
//!
//! A completed chunked segment lives in the store as the parts that composed
//! it, and the whole pipeline from demuxer to store is built to move those
//! buffers by refcount rather than by copy. Flattening them here to satisfy a
//! body type that wants one contiguous slice would put a megabyte-scale memcpy
//! on the busiest path in the process, once per viewer per segment.
//!
//! So the body is a cursor over frames. Each poll hands out the next stored
//! buffer as it already exists.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};

use crate::delivery::hls::serve::MediaBody;

/// An HTTP body over the buffers media was stored in.
pub struct StoredMediaBody {
    frames: std::vec::IntoIter<Bytes>,
    remaining: u64,
}

impl StoredMediaBody {
    pub fn new(media: MediaBody) -> Self {
        let remaining = media.len();
        let frames: Vec<Bytes> = media
            .frames()
            .iter()
            .map(|payload| payload.bytes().clone())
            .collect();
        Self {
            frames: frames.into_iter(),
            remaining,
        }
    }
}

impl Body for StoredMediaBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.frames.next() {
            Some(bytes) => {
                self.remaining = self.remaining.saturating_sub(bytes.len() as u64);
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            None => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    /// Exact, because every byte is already in memory.
    ///
    /// This is what lets the response carry a real `Content-Length` instead of
    /// falling back to chunked transfer encoding, which matters for a player
    /// deciding whether a partial segment arrived whole.
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

/// One byte range a client asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    /// First byte, inclusive.
    pub start: u64,
    /// Last byte, inclusive, as HTTP counts them.
    pub end: u64,
}

/// Parses a single-range `Range` header against a known body length.
///
/// Only one range is supported. Multi-range responses require multipart
/// encoding, no HLS client asks for them, and answering with the whole resource
/// is always a valid response to a range request — so an unsupported form
/// returns `Unsatisfiable` only when it is genuinely out of bounds, and `None`
/// when it is merely something better ignored.
pub fn parse_range(header: &str, length: u64) -> RangeOutcome {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeOutcome::Ignore;
    };
    if spec.contains(',') {
        return RangeOutcome::Ignore;
    }
    let Some((from, to)) = spec.split_once('-') else {
        return RangeOutcome::Ignore;
    };
    let (from, to) = (from.trim(), to.trim());
    if length == 0 {
        return RangeOutcome::Unsatisfiable;
    }

    let range = match (from.is_empty(), to.is_empty()) {
        // `bytes=-N`: the final N bytes.
        (true, false) => {
            let Ok(suffix) = to.parse::<u64>() else {
                return RangeOutcome::Ignore;
            };
            if suffix == 0 {
                return RangeOutcome::Unsatisfiable;
            }
            ByteRange {
                start: length.saturating_sub(suffix),
                end: length - 1,
            }
        }
        (false, true) => {
            let Ok(start) = from.parse::<u64>() else {
                return RangeOutcome::Ignore;
            };
            ByteRange {
                start,
                end: length - 1,
            }
        }
        (false, false) => {
            let (Ok(start), Ok(end)) = (from.parse::<u64>(), to.parse::<u64>()) else {
                return RangeOutcome::Ignore;
            };
            ByteRange {
                start,
                // A client may ask past the end; the answer is what exists.
                end: end.min(length - 1),
            }
        }
        (true, true) => return RangeOutcome::Ignore,
    };

    if range.start >= length || range.start > range.end {
        return RangeOutcome::Unsatisfiable;
    }
    RangeOutcome::Satisfiable(range)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeOutcome {
    /// Serve the whole resource, as if no range had been asked for.
    Ignore,
    Satisfiable(ByteRange),
    /// The range cannot be met, which HTTP says must be reported rather than
    /// silently widened.
    Unsatisfiable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_inclusive_and_clamped_to_what_exists() {
        assert_eq!(
            parse_range("bytes=0-99", 1_000),
            RangeOutcome::Satisfiable(ByteRange { start: 0, end: 99 })
        );
        assert_eq!(
            parse_range("bytes=500-", 1_000),
            RangeOutcome::Satisfiable(ByteRange {
                start: 500,
                end: 999
            })
        );
        assert_eq!(
            parse_range("bytes=-100", 1_000),
            RangeOutcome::Satisfiable(ByteRange {
                start: 900,
                end: 999
            })
        );
        assert_eq!(
            parse_range("bytes=900-5000", 1_000),
            RangeOutcome::Satisfiable(ByteRange {
                start: 900,
                end: 999
            }),
            "asking past the end is answered with what exists, not refused"
        );
    }

    #[test]
    fn out_of_bounds_ranges_are_refused_and_odd_ones_are_ignored() {
        assert_eq!(
            parse_range("bytes=1000-", 1_000),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(parse_range("bytes=0-0", 0), RangeOutcome::Unsatisfiable);
        assert_eq!(
            parse_range("bytes=0-10,20-30", 1_000),
            RangeOutcome::Ignore,
            "a multi-range request is answered in full rather than multipart"
        );
        assert_eq!(parse_range("items=0-10", 1_000), RangeOutcome::Ignore);
        assert_eq!(parse_range("bytes=abc-def", 1_000), RangeOutcome::Ignore);
    }
}
