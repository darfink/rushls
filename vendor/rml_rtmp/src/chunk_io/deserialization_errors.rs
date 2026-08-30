use std::io;
use thiserror::Error;

/// Data for when an error occurs while attempting to deserialize a RTMP chunk
/// An enumeration defining all the possible errors that could occur while deserializing
/// RTMP chunks.
#[derive(Debug, Error)]
pub enum ChunkDeserializationError {
    /// The RTMP chunk format requires that RTMP chunks that are not type 0 utilize information
    /// from the previously received chunk on that same chunk stream id.  This error occurs when a
    /// non-0 chunk is received on a stream that has not received a type 0 chunk yet.
    #[error(
        "Received chunk with non-zero chunk type on csid {csid} prior to receiving a type 0 chunk"
    )]
    NoPreviousChunkOnStream { csid: u32 },

    /// The max chunk size does not allow chunk sizes more than 2,147,483,647 (since it's encoded in only
    /// 31 bytes of the SetChunkSize message), so this error occurs when a chunk size of greater than
    /// this value is attempted to be set
    #[error("Requested an invalid max chunk size of {chunk_size}.  The largest chunk size possible is 2147483647")]
    InvalidMaxChunkSize { chunk_size: usize },

    /// The configured policy limit for an inbound RTMP resource was exceeded.
    #[error("Inbound RTMP {resource} limit exceeded: attempted {attempted}, maximum {maximum}")]
    ResourceLimitExceeded {
        resource: &'static str,
        attempted: usize,
        maximum: usize,
    },

    /// An I/O error occurred while reading the input buffer
    #[error("{0}")]
    Io(#[from] io::Error),

    /// CROWDCAST: a peer declared a message length shorter than the payload it
    /// has already sent on that chunk stream.
    ///
    /// Upstream computed the remaining byte count with an unchecked
    /// subtraction, so this case panicked on arithmetic underflow. On a public
    /// ingest port that is remotely reachable, so it is surfaced as a normal
    /// protocol error and the connection is dropped instead.
    #[error(
        "Chunk stream {csid} declared message length {message_length} but {buffered} bytes are already buffered"
    )]
    MessageLengthSmallerThanBufferedPayload {
        csid: u32,
        message_length: usize,
        buffered: usize,
    },
}
