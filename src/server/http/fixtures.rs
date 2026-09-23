//! Test material for the HTTP surface.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use parking_lot::Mutex;

use crate::delivery::{
    Origin,
    hls::{
        StreamStore,
        service::{Config as HlsConfig, Service as HlsService},
    },
};
use crate::domain::SessionId;
use crate::observe::{EventObserver, Events, NodeEvent, SessionEvent};
use crate::server::runtime::ViewerApplication;

use super::tls::TlsSettings;

/// The production HLS/media composition used by HTTP integration tests.
pub(crate) fn application(store: &StreamStore) -> Arc<ViewerApplication> {
    application_with(store, HlsConfig::default())
}

pub(crate) fn application_with(store: &StreamStore, hls: HlsConfig) -> Arc<ViewerApplication> {
    let origin = Arc::new(Origin::new(store.clone()));
    let hls = Arc::new(HlsService::new(Arc::clone(&origin), hls));
    Arc::new(ViewerApplication::new(origin, hls))
}

/// A self-signed pair written into `directory`, plus the certificate's DER.
///
/// The DER is what a reload test compares: it is the exact bytes a client sees
/// as the peer certificate, so "did the rotation take effect" needs no parsing
/// to answer.
pub fn write_pair(directory: &Path, name: &str) -> (TlsSettings, Vec<u8>) {
    let certified = rcgen::generate_simple_self_signed([name.to_owned()])
        .expect("a self-signed certificate is generated");
    let settings = TlsSettings {
        certificate: directory.join("certificate.pem"),
        key: directory.join("key.pem"),
        ..TlsSettings::default()
    };
    write_atomically(&settings.certificate, certified.cert.pem().as_bytes());
    write_atomically(
        &settings.key,
        certified.signing_key.serialize_pem().as_bytes(),
    );
    (settings, certified.cert.der().to_vec())
}

/// Writes through a rename, the way real rotation tooling does.
///
/// Writing in place would let a watcher observe a half-written file, which is
/// the very race the debounce exists to absorb — tests should exercise the
/// same shape production sees, not an easier one.
pub fn write_atomically(path: &Path, contents: &[u8]) {
    let staged = path.with_extension("staged");
    std::fs::write(&staged, contents).expect("the staged file is written");
    std::fs::rename(&staged, path).expect("the staged file is renamed into place");
}

/// Writes a pair the way a Kubernetes secret mount presents one, and returns
/// the settings naming the stable symlinks plus the new certificate's DER.
///
/// Reproduces the layout exactly, because its shape is the whole point:
///
/// ```text
/// tls.crt -> ..data/tls.crt
/// ..data  -> ..2026_07_29_10_00_00.000
/// ..2026_07_29_10_00_00.000/tls.crt
/// ```
///
/// Rotation writes a fresh timestamped directory and atomically swaps
/// `..data`, so no filesystem event ever names `tls.crt`. Calling this twice
/// on the same directory rotates it the way kubelet does.
#[cfg(unix)]
pub fn write_projected_pair(directory: &Path, name: &str) -> (TlsSettings, Vec<u8>) {
    let certified = rcgen::generate_simple_self_signed([name.to_owned()])
        .expect("a self-signed certificate is generated");

    let generation = directory.join(format!(
        "..{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&generation).expect("the generation directory is created");
    std::fs::write(generation.join("tls.crt"), certified.cert.pem())
        .expect("the certificate is written");
    std::fs::write(
        generation.join("tls.key"),
        certified.signing_key.serialize_pem(),
    )
    .expect("the key is written");

    // The swap kubelet performs: a symlink is created under a temporary name
    // and renamed over the live one, so readers never see a missing `..data`.
    let data = directory.join("..data");
    let staged = directory.join("..data_tmp");
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(
        generation.file_name().expect("the generation has a name"),
        &staged,
    )
    .expect("the staged link is created");
    std::fs::rename(&staged, &data).expect("the staged link is renamed into place");

    let settings = TlsSettings {
        certificate: directory.join("tls.crt"),
        key: directory.join("tls.key"),
        ..TlsSettings::default()
    };
    for (link, target) in [
        (&settings.certificate, "..data/tls.crt"),
        (&settings.key, "..data/tls.key"),
    ] {
        if std::fs::symlink_metadata(link).is_err() {
            std::os::unix::fs::symlink(target, link).expect("the stable link is created");
        }
    }
    (settings, certified.cert.der().to_vec())
}

/// A private directory for one test, removed if a previous run left it behind.
pub fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let directory =
        std::env::temp_dir().join(format!("rushls-tls-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is created");
    directory
}

/// Collects process events so a test can assert on what was reported.
#[derive(Default)]
pub struct NodeEventRecorder {
    events: Mutex<Vec<NodeEvent>>,
}

impl NodeEventRecorder {
    /// The recorder and the [`Events`] handle that feeds it.
    pub fn install() -> (Arc<Self>, Events) {
        let recorder = Arc::new(Self::default());
        let events = Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>);
        (recorder, events)
    }

    pub fn recorded(&self) -> Vec<NodeEvent> {
        self.events.lock().clone()
    }
}

impl EventObserver for NodeEventRecorder {
    fn observe(&self, _session: SessionId, _event: SessionEvent) {}

    fn observe_node(&self, event: NodeEvent) {
        self.events.lock().push(event);
    }
}

/// Real CMAF bytes with one AAC priming frame and video starting 500 ms later.
/// The shared presentation origin must survive both muxing and HTTP projection.
pub fn offset_cmaf() -> Result<
    (
        Arc<crate::mux::PackagedPresentation>,
        Vec<crate::mux::PackagedMedia>,
    ),
    Box<dyn std::error::Error>,
> {
    use crate::{
        admission::StreamPolicy,
        domain::{
            AudioTiming, AudioTrim, Codec, FrameRate, MediaKind, MediaParameters, Payload,
            Timebase, TrackId,
            fixtures::{TrackBuilder, catalog},
        },
        media::{AudioSample, NormalizedMedia, VideoSample, validate},
        mux::{
            FinishReason, MuxerFactory, MuxerStartRequest, PassThroughMuxerFactory,
            fixtures::{
                AAC_EXTRADATA, AAC_FRAME, H264_EXTRADATA, H264_IDR, H264_P, discarded_events,
            },
        },
        segment::{SegmentationPlan, fixtures::PlanBuilder},
    };
    let audio_base = Timebase::new(nz::u32!(1), nz::u32!(48_000));
    let video_base = Timebase::new(nz::u32!(1), nz::u32!(16_384));
    let audio = TrackBuilder::new(0, MediaKind::Audio)
        .timebase(audio_base)
        .parameters(MediaParameters::Audio {
            sample_rate: nz::u32!(48_000),
            channels: nz::u16!(1),
            frame_size: Some(nz::u32!(1_024)),
            bit_depth: None,
            timing: AudioTiming {
                initial_padding_samples: 1_024,
                ..AudioTiming::default()
            },
        })
        .codec(Codec::Aac)
        .codec_extradata(AAC_EXTRADATA.to_vec())
        .build();
    let video = TrackBuilder::new(1, MediaKind::Video)
        .timebase(video_base)
        .parameters(MediaParameters::Video {
            width: nz::u32!(16),
            height: nz::u32!(16),
            frame_rate: Some(FrameRate::new(nz::u32!(2), nz::u32!(1))),
            video_delay: 0,
        })
        .codec_extradata(H264_EXTRADATA.to_vec())
        .build();
    let input = validate(&catalog(vec![audio, video]), &StreamPolicy::permissive())?;
    let segmentation = SegmentationPlan::new(
        &input,
        vec![
            PlanBuilder::new(0, audio_base, nz::u64!(49_152))
                .part(nz::u32!(2), nz::u64!(2_048))
                .build(),
            PlanBuilder::new(1, video_base, nz::u64!(16_384))
                .part(nz::u32!(1), nz::u64!(8_192))
                .presentation_origin(-8_192)
                .build(),
        ],
    )?;
    let events = discarded_events();
    let mut started = PassThroughMuxerFactory.start(MuxerStartRequest {
        presentation: &input,
        segmentation: &segmentation,
        time_anchor: crate::delivery::hls::fixtures::anchor(),
        events: &events,
    })?;
    let mut media = Vec::new();
    for frame in -1..4 {
        started.muxer.push(
            NormalizedMedia::Audio(AudioSample {
                track_id: TrackId(0),
                codec: Codec::Aac,
                pts: frame * 1_024,
                duration: 1_024,
                trim: AudioTrim {
                    leading_samples: if frame == -1 { 1_024 } else { 0 },
                    trailing_samples: 0,
                },
                payload: Payload::from(AAC_FRAME.to_vec()),
            }),
            &mut media,
        )?;
    }
    for frame in 0..2 {
        started.muxer.push(
            NormalizedMedia::Video(VideoSample {
                track_id: TrackId(1),
                codec: Codec::H264,
                pts: frame * 8_192,
                dts: frame * 8_192,
                duration: 8_192,
                random_access: frame == 0,
                payload: Payload::from(if frame == 0 {
                    H264_IDR.to_vec()
                } else {
                    H264_P.to_vec()
                }),
            }),
            &mut media,
        )?;
    }
    started.muxer.finish(FinishReason::Final, &mut media)?;
    Ok((started.presentation, media))
}
