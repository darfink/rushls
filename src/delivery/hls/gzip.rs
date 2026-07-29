//! The one compressor this origin uses.
//!
//! HLS asks servers to transfer text files — playlists and WebVTT segments —
//! with the `gzip` Content-Encoding where the client accepts it
//! (draft-pantos-hls-rfc8216bis-22 § 6.2.1). Both kinds are compressed exactly
//! once, when the bytes are produced rather than when they are requested: a
//! playlist inside the render cache's critical section, a WebVTT segment as it
//! is published. Nothing on the request path ever compresses or decompresses,
//! which is what keeps `Accept-Encoding` from being a lever an unauthenticated
//! client can pull to make the origin do work.

use std::io::Write;

use bytes::Bytes;
use flate2::{Compression, write::GzEncoder};

/// Compresses one text resource.
///
/// The capacity guess is deliberate: playlists and cue text are repetitive
/// ASCII that compresses to roughly a fifth of their size, so one allocation
/// covers the whole encode.
///
/// Both writes are infallible into a `Vec`, and their errors must not be
/// papered over by returning the input — those bytes would then be served under
/// `Content-Encoding: gzip`, which is worse than any failure they could report.
pub fn gzip(bytes: &Bytes) -> Bytes {
    let mut encoder = GzEncoder::new(Vec::with_capacity(bytes.len() / 4), Compression::default());
    encoder
        .write_all(bytes)
        .expect("writing into a Vec cannot fail");
    Bytes::from(encoder.finish().expect("flushing into a Vec cannot fail"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playlist_round_trips_through_the_encoding_it_is_served_under() {
        use std::io::Read;

        let playlist = Bytes::from_static(
            b"#EXTM3U\n#EXT-X-VERSION:9\n#EXT-X-TARGETDURATION:6\n\
              #EXT-X-PART:DURATION=1,URI=\"part/55.m4s\"\n\
              #EXT-X-PART:DURATION=1,URI=\"part/56.m4s\"\n",
        );

        let encoded = gzip(&playlist);
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(encoded.as_ref())
            .read_to_end(&mut decoded)
            .expect("the origin emits a well-formed gzip member");

        assert_eq!(decoded, playlist);
        assert!(
            encoded.len() < playlist.len(),
            "repetitive playlist text is what the encoding is for"
        );
    }
}
