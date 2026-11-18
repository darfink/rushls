use std::{sync::Arc, time::Duration};

use crate::{
    admission::Authenticator,
    delivery::hls::PublisherFactory,
    domain::InputMode,
    media::{self, NormalizeError, PresentationPlan, StartedNormalizer, TimelineCalibration},
    mux::{self, MuxError, MuxerStartRequest, StartedMuxer},
    observe::{Events, ProcessMeters},
    segment::{PrerollLimits, SegmentationPolicy},
    source::{DiscoveryLimits, InputLimits},
};

use super::{Registry, SupervisionPolicy};

/// The collaborators every session shares, wired once at start-up.
///
/// The authenticator and publisher factory are trait objects so a node can
/// choose its admission policy and recording configuration without threading
/// their types through the session driver.
#[derive(Clone)]
pub struct Services {
    pub authenticator: Arc<dyn Authenticator>,
    pub publishers: Arc<dyn PublisherFactory>,
    pub sessions: Registry,
    pub meters: ProcessMeters,
    pub events: Events,
    #[cfg(test)]
    pub(crate) stages: Option<super::tests::StageOverrides>,
}

#[cfg_attr(
    not(test),
    expect(
        clippy::unused_self,
        reason = "only tests can replace stage constructors"
    )
)]
impl Services {
    pub(super) fn start_normalizer(
        &self,
        presentation: &PresentationPlan,
        timeline: &TimelineCalibration,
        mode: InputMode,
    ) -> Result<StartedNormalizer, NormalizeError> {
        #[cfg(test)]
        if let Some(stages) = &self.stages {
            return stages.start_normalizer(presentation, timeline, mode);
        }
        media::start_normalizer(presentation, timeline, mode)
    }

    pub(super) fn start_muxer(
        &self,
        request: MuxerStartRequest<'_>,
    ) -> Result<StartedMuxer, MuxError> {
        #[cfg(test)]
        if let Some(stages) = &self.stages {
            return Ok(stages.start_muxer(request));
        }
        mux::start_muxer(request)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionConfig {
    /// Shared accounted bytes retained by one active publisher; `None` means
    /// accounted but unlimited, for trusted deployments.
    pub memory_per_publisher: Option<usize>,
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
            memory_per_publisher: Some(crate::domain::PipelineBudget::DEFAULT_LIMIT),
            maximum_admission_time: Duration::from_secs(10),
            discovery: DiscoveryLimits {
                // A multi-rendition contribution can carry several megabits
                // per second before the demuxer has observed enough packets to
                // finish stream discovery.
                maximum_probe_bytes: crate::source::PipelineMemory::DISCOVERY,
                maximum_wall_time: Duration::from_secs(10),
            },
            input: InputLimits::permissive(),
            preroll: PrerollLimits::permissive(),
            segmentation: SegmentationPolicy::default(),
            supervision: SupervisionPolicy::default(),
        }
    }
}
