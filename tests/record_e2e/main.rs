//! Record end-to-end (ignored by default).
//!
//! Synthetic A+V -> burst MPEG-TS ingest -> [record] -> ffprobe verification.
mod config;
mod generate;
mod harness;
mod malformed;
mod publish;
mod reconnect;
mod verify;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs ffmpeg/ffprobe; CI runs this with --ignored"]
async fn record_burst_keeps_every_sample() -> TestResult {
    if generate::missing_tools().is_some() {
        eprintln!("skipping record e2e: ffmpeg and/or ffprobe are not on PATH");
        return Ok(());
    }
    let cfg = config::E2eConfig::from_env();
    eprintln!("record e2e: {}s synthetic ({} @ {}fps, {})", cfg.duration_secs, cfg.size, cfg.fps, cfg.segment_expectation());
    let mut work = harness::WorkDir::new(&cfg)?;
    if work.keep() { eprintln!("record e2e: workdir {}", work.path().display()); }
    let ts = generate::generate_mpegts(&cfg, &work)?;
    let outcome = harness::publish_and_record(&cfg, &work, &ts).await?;
    let result = verify::verify_archive(&cfg, &work, &outcome);
    if work.keep() { eprintln!("record e2e: kept {} ({} files)", work.path().display(), outcome.files.len()); work.leak(); }
    result?;
    Ok(())
}

/// Reconnect grace: two separately encoded halves, two sequential sessions,
/// one archive. The second half restarts segment numbers like a real
/// reconnecting encoder; the publication id in the pattern keeps them apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs ffmpeg/ffprobe; CI runs this with --ignored"]
async fn record_reconnect_grace_keeps_every_sample() -> TestResult {
    if generate::missing_tools().is_some() {
        eprintln!("skipping record reconnect e2e: ffmpeg and/or ffprobe are not on PATH");
        return Ok(());
    }
    let cfg = config::E2eConfig::from_env();
    let secs_a = cfg.duration_secs / 2;
    let secs_b = cfg.duration_secs - secs_a;
    eprintln!("record e2e reconnect: {}s + {}s synthetic ({} @ {}fps)", secs_a, secs_b, cfg.size, cfg.fps);
    let mut work = harness::WorkDir::new(&cfg)?;
    if work.keep() { eprintln!("record e2e reconnect: workdir {}", work.path().display()); }
    let ts_a = generate::generate_mpegts_named(&cfg, &work, "src-a.ts", secs_a)?;
    let ts_b = generate::generate_mpegts_named(&cfg, &work, "src-b.ts", secs_b)?;
    let outcome = reconnect::publish_two_halves(&work, &ts_a, &ts_b, secs_a, secs_b).await?;
    let result = reconnect::verify_reconnect(&cfg, &work, &outcome);
    if work.keep() { eprintln!("record e2e reconnect: kept {} ({} files)", work.path().display(), outcome.outcome.files.len()); work.leak(); }
    result?;
    Ok(())
}

/// Garbage in, nothing out: non-media input is rejected and the archive
/// stays empty. Fast and in-process, so this runs in the default suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_non_media_input() -> TestResult {
    malformed::expect_rejected(vec![0u8; 65536], "zeros").await?;
    malformed::expect_rejected(vec![0xFFu8; 65536], "all-ones").await?;
    malformed::expect_rejected(Vec::new(), "empty").await?;
    Ok(())
}

/// A valid file cut at 60 percent on a packet edge must leave a clean,
/// contiguous, fully decodable prefix within the full-length totals.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs ffmpeg/ffprobe; CI runs this with --ignored"]
async fn record_truncated_burst_keeps_prefix() -> TestResult {
    if generate::missing_tools().is_some() {
        eprintln!("skipping record truncation e2e: ffmpeg and/or ffprobe are not on PATH");
        return Ok(());
    }
    let cfg = config::E2eConfig::from_env();
    eprintln!("record e2e truncated: {}s synthetic ({} @ {}fps)", cfg.duration_secs, cfg.size, cfg.fps);
    let mut work = harness::WorkDir::new(&cfg)?;
    if work.keep() { eprintln!("record e2e truncated: workdir {}", work.path().display()); }
    let ts = generate::generate_mpegts(&cfg, &work)?;
    let bytes = std::fs::read(&ts)?;
    let cut = bytes.len() * 6 / 10 / 188 * 188;
    eprintln!("record e2e truncated: cutting {} bytes to {} on a packet edge", bytes.len(), cut);
    let rig = harness::TestRig::start(work.archive(), "{rendition}_{segment}.mp4")?;
    let result = rig.run_burst(bytes[..cut].to_vec()).await;
    let drained = rig.drain_and_collect().await?;
    let files = drained.files.len();
    let checked = malformed::check_truncated_prefix(&cfg, &work, &result, &drained);
    if work.keep() { eprintln!("record e2e truncated: kept {} ({} files)", work.path().display(), files); work.leak(); }
    checked?;
    Ok(())
}
