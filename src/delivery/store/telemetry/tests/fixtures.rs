use super::super::*;
use crate::mux::fixtures::{RenditionBuilder, config};

pub fn descriptor(id: u32, kind: MediaKind, hz: u32) -> PackagedRendition {
    let hz = std::num::NonZeroU32::new(hz).expect("fixture clock is positive");
    RenditionBuilder::new(id, kind)
        .config(config(
            Timebase::new(nz::u32!(1), hz),
            u64::from(hz.get()) * 6,
            Some(u64::from(hz.get())),
        ))
        .build()
}

pub fn publication(descriptors: Vec<PackagedRendition>) -> PublicationTelemetry {
    let mut telemetry = PublicationTelemetry::default();
    let ids = descriptors
        .iter()
        .map(|d| RenditionId(d.packaging_rendition_id.0))
        .collect();
    telemetry.attach(
        1,
        descriptors.into_iter().map(|d| {
            (
                RenditionId(d.packaging_rendition_id.0),
                d,
                Duration::from_secs(6),
            )
        }),
        vec![(Arc::from("video"), ids)],
    );
    telemetry
}

pub async fn advance(seconds: u64) {
    tokio::time::advance(Duration::from_secs(seconds)).await;
}
