//! An origin plus a publication, ready for Apple's playlist validator.
//!
//! Cleartext by default. See [`crate::certs`] for why, and for how to opt into
//! TLS when a machine already trusts a certificate the suite may use.

use std::{sync::Arc, time::Duration};

use rushls::{
    admission::{OpenStreamAuthenticator, StreamPolicy},
    delivery::hls::{service::PlaylistReadiness, uri::UriBase},
    segment::SegmentationPolicy,
    server::{
        Node, NodeConfig,
        http::{HttpConfig, Readiness, bind_tls, serve},
    },
    session::{PendingPermit, SessionConfig, SessionOutcome, run_session},
    source::PendingPublish,
};
use tokio::net::TcpListener;

use crate::{certs, observe::Recorder, report::Expect, validator};

/// Eight seconds of 1 s GOPs lock onto this cadence with several completed
/// segments, without waiting on Apple's 6 s default.
pub fn default_segmentation() -> SegmentationPolicy {
    SegmentationPolicy::latency_first(Duration::from_secs(2), Duration::from_millis(500))
}

/// A cadence whose maximum is far enough above its desire to absorb a long GOP.
///
/// `latency_first` fixes the maximum at twice the desire, so a five-second key
/// frame interval has no legal boundary under a two-second desire and the
/// publication is refused. Naming the maximum separately is what lets a case
/// ask "can it cope with a long GOP" rather than "does it refuse one".
pub fn segmentation_with_headroom(
    desired: Duration,
    part: Duration,
    maximum: Duration,
) -> SegmentationPolicy {
    SegmentationPolicy {
        maximum_segment_duration: maximum,
        ..SegmentationPolicy::latency_first(desired, part)
    }
}

/// What one case asks of the origin, beyond the media it publishes.
#[derive(Clone, Copy, Debug)]
pub struct Setup {
    pub segmentation: SegmentationPolicy,
    pub readiness: PlaylistReadiness,
    pub retain: Option<Duration>,
    /// What the publication deliberately contains, so a conformance finding
    /// that is merely inapplicable can be told apart from a defect.
    pub expect: Expect,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            segmentation: default_segmentation(),
            readiness: PlaylistReadiness::CompletedSegment,
            retain: None,
            expect: Expect::default(),
        }
    }
}

impl Setup {
    pub fn segmentation(mut self, segmentation: SegmentationPolicy) -> Self {
        self.segmentation = segmentation;
        self
    }

    /// Names the case in report output and in any `RUSHLS_TEST_REPORT_DIR` file.
    pub fn named(mut self, name: &'static str) -> Self {
        self.expect.name = name;
        self
    }

    pub fn expect(mut self, apply: impl FnOnce(Expect) -> Expect) -> Self {
        self.expect = apply(self.expect);
        self
    }
}

pub struct Origin {
    node: Node,
    session: SessionConfig,
    url: String,
    ca_pem: Option<std::path::PathBuf>,
    expect: Expect,
    recorder: Recorder,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    _https: Option<Arc<certs::SharedHttps>>,
    _disk: Option<TemporaryDvr>,
}

impl Origin {
    pub async fn start() -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        Self::start_with(Setup::default()).await
    }

    pub async fn start_with(
        mut setup: Setup,
    ) -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        if std::env::var_os("RUSHLS_TEST_REQUIRE_APPLE_TOOLS").is_some()
            && (!cfg!(target_os = "macos") || !validator::tools_available())
        {
            return Err(
                "macOS, mediastreamvalidator, and hlsreport are required for this run".into(),
            );
        }
        if cfg!(not(target_os = "macos")) {
            eprintln!("skipping Apple HLS tests: macOS and mediastreamvalidator are required");
            return Ok(None);
        }
        if !validator::mediastreamvalidator_available() {
            eprintln!("skipping Apple HLS tests: mediastreamvalidator is not on PATH");
            return Ok(None);
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let https = certs::shared_https()?;
        if https.is_none() && certs::tls_requested() {
            return Err("TLS was requested but no usable certificate was produced".into());
        }
        let scheme = if https.is_some() { "https" } else { "http" };
        let host = certs::origin_host(address.port()).await?;
        // Set here rather than per case: whether the transport findings are
        // real is a property of how the suite was launched, not of the media.
        setup.expect.tls = https.is_some();

        let mut config = NodeConfig::default();
        // Absolute names so every fetch after the multivariant playlist keeps
        // the scheme under test rather than falling back to a relative one.
        config.hls.uri_base = UriBase::new(format!("{scheme}://{host}:{}", address.port()));
        config.hls.readiness = setup.readiness;
        configure_authoring(&mut config, &setup.expect);
        config.session.segmentation = setup.segmentation;
        let disk = configure_retention(&mut config, setup.retain)?;
        let session = config.session;
        let recorder = Recorder::default();
        let mut policy = StreamPolicy::permissive();
        if !setup.expect.permissive {
            policy.input_mode = rushls::domain::InputMode::Strict;
        }
        let node = Node::new(
            config,
            Arc::new(OpenStreamAuthenticator::new(policy)),
            recorder.events(),
            None,
        )?;

        let application = Arc::new(crate::trace::Traced::new(
            node.application(),
            setup.expect.name,
        )?);
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let stop = async {
            let _ = stopped.await;
        };
        let server = match &https {
            Some(https) => {
                let settings = https.settings.clone();
                let tls_listener = bind_tls(
                    listener,
                    settings.clone(),
                    node.services().meters.clone(),
                    node.services().events.clone(),
                )?;
                tokio::spawn(serve(
                    tls_listener,
                    application,
                    HttpConfig {
                        tls: Some(settings),
                        tls_address: Some(address),
                        ..HttpConfig::default()
                    },
                    None,
                    None,
                    Readiness::ready(),
                    stop,
                ))
            }
            None => tokio::spawn(serve(
                listener,
                application,
                HttpConfig::default(),
                None,
                None,
                Readiness::ready(),
                stop,
            )),
        };

        Ok(Some(Self {
            node,
            session,
            url: format!(
                "{scheme}://{host}:{}/live/camera/index.m3u8",
                address.port()
            ),
            ca_pem: https.as_ref().and_then(|https| https.ca_pem.clone()),
            expect: setup.expect,
            recorder,
            shutdown: Some(shutdown),
            server: Some(server),
            _https: https,
            _disk: disk,
        }))
    }

    pub async fn publish_and_validate(
        &self,
        pending: Box<dyn PendingPublish>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.publish(pending).await?;
        self.validate_playlist()
    }

    /// Runs a publication to completion, leaving the origin serving what it made.
    pub async fn publish(
        &self,
        pending: Box<dyn PendingPublish>,
    ) -> Result<SessionOutcome, Box<dyn std::error::Error + Send + Sync>> {
        let outcome = run_session(
            pending,
            self.node.services(),
            &self.session,
            PendingPermit::unlimited(),
        )
        .await;
        match outcome {
            Ok(outcome @ (SessionOutcome::Ended | SessionOutcome::Interrupted)) => Ok(outcome),
            Ok(other) => Err(format!("session ended unexpectedly: {other:?}").into()),
            Err(error) => Err(error.into()),
        }
    }

    /// Anything the origin reported as a failure while this case ran.
    pub fn reported_failures(&self) -> Vec<String> {
        self.recorder.failures()
    }

    /// Runs the session in the background so a live transport can stay open
    /// while Apple fetches. The caller closes the publisher after this returns.
    pub fn spawn_session(
        &self,
        pending: Box<dyn PendingPublish>,
    ) -> tokio::task::JoinHandle<Result<SessionOutcome, rushls::session::SessionError>> {
        let services = self.node.services().clone();
        let session = self.session;
        tokio::spawn(async move {
            run_session(pending, &services, &session, PendingPermit::unlimited()).await
        })
    }

    pub fn validate_playlist(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        validator::wait_for_playlist(&self.url, self.ca_pem.as_deref())?;
        validator::validate(&self.url, &self.expect)?;
        Ok(())
    }

    /// The multivariant playlist URL, for cases that assert on its text.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn certificate_authority(&self) -> Option<&std::path::Path> {
        self.ca_pem.as_deref()
    }

    /// Fetches every media playlist the multivariant playlist names.
    ///
    /// Both forms count: a variant names its playlist on the line after
    /// `EXT-X-STREAM-INF`, while an audio or subtitle rendition names it in a
    /// `URI` attribute on `EXT-X-MEDIA`. A check that reads only one of the two
    /// silently exempts half the presentation.
    pub fn media_playlists(
        &self,
    ) -> Result<Vec<(String, String)>, Box<dyn std::error::Error + Send + Sync>> {
        let multivariant = validator::fetch(&self.url, self.ca_pem.as_deref())?;
        let mut playlists = Vec::new();
        for line in multivariant.lines() {
            let line = line.trim();
            let uri = if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
                rest.split("URI=\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
            } else if line.starts_with('#') || line.is_empty() {
                None
            } else {
                Some(line)
            };
            let Some(uri) = uri else { continue };
            if playlists.iter().any(|(seen, _)| seen == uri) {
                continue;
            }
            playlists.push((
                uri.to_owned(),
                validator::fetch(uri, self.ca_pem.as_deref())?,
            ));
        }
        if playlists.is_empty() {
            return Err(
                format!("the multivariant playlist named no media:\n{multivariant}").into(),
            );
        }
        Ok(playlists)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

fn configure_authoring(config: &mut NodeConfig, expect: &Expect) {
    config.store.independent_segments = !expect.permissive;
    if expect.full_authoring {
        // Apple fetches some initially advertised parts near the end of
        // its exhaustive scan. Keep URLs alive across that scan plus
        // request overhead; the part-tag window and production defaults stay intact.
        config.store.retention.part_fetch_grace_period =
            validator::SCAN_TIMEOUT.saturating_mul(2).into();
    }
}

fn configure_retention(
    config: &mut NodeConfig,
    retain: Option<Duration>,
) -> std::io::Result<Option<TemporaryDvr>> {
    if let Some(retain) = retain {
        let disk = TemporaryDvr::new()?;
        config.store.retention.retain = retain.into();
        config.store.retention.maximum_payload_bytes = 512 * 1024 * 1024;
        // The compact ladder retains roughly 4 GiB. Keep a hard disk cap
        // that fits a hosted runner while exercising production spill behavior.
        config.store.disk = Some(rushls::delivery::store::DiskLimits {
            directory: disk.0.clone(),
            maximum_payload_bytes: 5 * 1024 * 1024 * 1024,
        });
        config.store.retention.maximum_segments = 100_000;
        config.store.retention.maximum_parts = 200_000;
        return Ok(Some(disk));
    }
    Ok(None)
}

/// Retained media is disposable; reports are saved separately before teardown.
/// Keep this last in Origin so the node drops before its directory is removed.
struct TemporaryDvr(std::path::PathBuf);

impl TemporaryDvr {
    fn new() -> std::io::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rushls-apple-dvr-{}-{id}", std::process::id()));
        // Never reuse a directory left by another test process.
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TemporaryDvr {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn temporary_dvr_removes_payloads_without_sharing_directories() -> std::io::Result<()> {
    let first = TemporaryDvr::new()?;
    let second = TemporaryDvr::new()?;
    assert_ne!(first.0, second.0);
    let path = first.0.clone();
    std::fs::write(path.join("segment.m4s"), b"temporary media")?;
    drop(first);
    assert!(!path.exists());
    assert!(second.0.exists());
    Ok(())
}
