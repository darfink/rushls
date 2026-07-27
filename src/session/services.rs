use std::{sync::Arc, time::Duration};

use crate::{
    admission::Authenticator,
    delivery::hls::PublisherFactory,
    media::NormalizerFactory,
    mux::MuxerFactory,
    observe::{Events, ProcessMeters},
    segment::{PrerollLimits, SegmentationPolicy},
    source::{DiscoveryLimits, InputLimits},
};

use super::{Registry, SupervisionPolicy};

/// The collaborators every session shares, wired once at start-up.
///
/// Erased to trait objects rather than threaded as type parameters. Each of
/// these is consulted at most once per session, so the indirection is
/// unmeasurable, and in exchange nothing about how a node is assembled leaks
/// into the signature of the code that runs a session.
#[derive(Clone)]
pub struct Services {
    pub authenticator: Arc<dyn Authenticator>,
    pub normalizers: Arc<dyn NormalizerFactory>,
    pub muxers: Arc<dyn MuxerFactory>,
    pub publishers: Arc<dyn PublisherFactory>,
    pub sessions: Registry,
    pub meters: ProcessMeters,
    pub events: Events,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionConfig {
    /// How long the handshake and authentication may take.
    ///
    /// Every later stage bounds its own wait, so without this one a connection
    /// that opens a socket and then says nothing would occupy a task forever —
    /// the cheapest denial of service available against an ingest node.
    pub maximum_admission_time: Duration,
    pub discovery: DiscoveryLimits,
    pub input: InputLimits,
    pub preroll: PrerollLimits,
    pub segmentation: SegmentationPolicy,
    pub supervision: SupervisionPolicy,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            maximum_admission_time: Duration::from_secs(10),
            discovery: DiscoveryLimits {
                maximum_probe_bytes: 1024 * 1024,
                maximum_wall_time: Duration::from_secs(10),
            },
            input: InputLimits::permissive(),
            preroll: PrerollLimits::permissive(),
            segmentation: SegmentationPolicy::default(),
            supervision: SupervisionPolicy::default(),
        }
    }
}
