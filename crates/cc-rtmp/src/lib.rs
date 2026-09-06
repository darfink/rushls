//! Crowdcast's role-neutral RTMP protocol API.
//!
//! The sans-I/O session and chunking machinery comes from the hardened,
//! workspace-vendored RML core. Enhanced RTMP media is inspected with
//! `scuffle-flv`. Original bytes remain authoritative for forwarding; a CMAF
//! view of the same tags is available for ingest that does not wrap FLV.

pub mod cmaf;
pub mod enhanced;
pub mod media;
pub mod metadata;

/// Low-level RTMP chunk APIs. Applications normally use [`sessions`].
pub mod chunk_io {
    pub use rml_rtmp::chunk_io::*;
}

/// RTMP handshake state machines.
pub mod handshake {
    pub use rml_rtmp::handshake::*;
}

/// Low-level RTMP messages.
pub mod messages {
    pub use rml_rtmp::messages::*;
}

/// Client and server RTMP sessions.
///
/// These are sans-I/O: callers own sockets, timeouts, and backpressure.
pub mod sessions {
    pub use rml_rtmp::sessions::*;
}

/// RTMP timestamp type.
pub mod time {
    pub use rml_rtmp::time::*;
}

pub use cmaf::{CmafCodec, CmafUnit};
pub use enhanced::{EnhancedCapabilities, EnhancedValidationMode};
pub use media::{
    MediaClassification, MediaInterpretation, ParsedAudio, ParsedVideo, ValidatedMedia,
};
pub use metadata::{
    EncoderSummary, MAX_ENCODER_LEN, MetadataCodec, ParsedMetadata, TrackMetadata,
    ValidatedMetadata, normalize_encoder_vendor,
};
pub use rml_amf0;

/// Socket-operation timeouts used by async adapters built around the sans-I/O
/// session API. `None` disables the corresponding timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerSessionTimeouts {
    pub handshake_read: Option<std::time::Duration>,
    pub session_read: Option<std::time::Duration>,
    pub write: Option<std::time::Duration>,
}

impl Default for ServerSessionTimeouts {
    fn default() -> Self {
        Self {
            handshake_read: Some(std::time::Duration::from_secs(2)),
            session_read: Some(std::time::Duration::from_millis(2_500)),
            write: Some(std::time::Duration::from_secs(2)),
        }
    }
}
