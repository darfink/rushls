//! Apple HLS integration tests.
//!
//! These are macOS-only and require `mediastreamvalidator` plus the ability to
//! install a test CA in a temporary user keychain. Linux CI skips them.

mod certs;
mod flv;
mod harness;
mod publish;
mod validator;

use rushls::source::transport::srt::{SrtCaller, SrtConfig, SrtListener};

use crate::publish::{H264_AAC_TS, H264_DUAL_AAC_TS, HEVC_AAC_TS};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_legacy_h264_aac() -> TestResult {
    run(publish::rtmp_h264_aac()?).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_enhanced_hevc_aac() -> TestResult {
    run(publish::rtmp_hevc_aac()?).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_multitrack_h264_two_aac() -> TestResult {
    run(publish::rtmp_h264_two_aac()?).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_captions_webvtt() -> TestResult {
    run(publish::rtmp_h264_aac_captions()?).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_aac() -> TestResult {
    run(publish::mpegts(H264_AAC_TS)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_hevc_aac() -> TestResult {
    run(publish::mpegts(HEVC_AAC_TS)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_two_aac() -> TestResult {
    run(publish::mpegts(H264_DUAL_AAC_TS)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn srt_h264_aac() -> TestResult {
    let Some(origin) = harness::Origin::start().await? else {
        return Ok(());
    };

    let config = SrtConfig::default();
    let mut listener = SrtListener::bind("127.0.0.1:0".parse()?, config.clone(), 4).await?;
    let address = listener.local_address();
    let caller = tokio::spawn(async move {
        let caller = SrtCaller::connect(address, &config, "publish:live/camera:secret")
            .await
            .map_err(|error| error.to_string())?;
        caller
            .send_mpegts(H264_AAC_TS)
            .await
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(caller)
    });

    let pending = listener
        .accept()
        .await
        .ok_or("SRT listener closed")?
        .map_err(|error| error.to_string())?;
    // Keep the caller socket open until Apple has fetched: closing first
    // races the demuxer and looks like a non-MPEG-TS input.
    let session = origin.spawn_session(publish::labeled(Box::new(pending)));
    origin.validate_playlist()?;
    drop(caller.await.map_err(|_| "SRT caller panicked")??);
    match session.await? {
        Ok(
            rushls::session::SessionOutcome::Ended
            | rushls::session::SessionOutcome::Interrupted,
        ) => Ok(()),
        Ok(other) => Err(format!("SRT session ended unexpectedly: {other:?}").into()),
        Err(error) => Err(error.into()),
    }
}

async fn run(pending: Box<dyn rushls::source::PendingPublish>) -> TestResult {
    let Some(origin) = harness::Origin::start().await? else {
        return Ok(());
    };
    origin.publish_and_validate(pending).await
}
