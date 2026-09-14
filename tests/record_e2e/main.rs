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
    if cfg.duration_secs < 2 * cfg.min_secs_per_part() {
        return Err(format!("record reconnect needs at least {}s total with {}s segments, got {}s; raise RUSHLS_TEST_RECORD_E2E_SECS", 2 * cfg.min_secs_per_part(), cfg.segment_secs, cfg.duration_secs).into());
    }
    let secs_a = cfg.duration_secs / 2;
    let secs_b = cfg.duration_secs - secs_a;
    eprintln!("record e2e reconnect: {}s + {}s synthetic ({} @ {}fps)", secs_a, secs_b, cfg.size, cfg.fps);
    let mut work = harness::WorkDir::new(&cfg)?;
    if work.keep() { eprintln!("record e2e reconnect: workdir {}", work.path().display()); }
    let ts_a = generate::generate_mpegts_named(&cfg, &work, "src-a.ts", secs_a)?;
    let ts_b = generate::generate_mpegts_named(&cfg, &work, "src-b.ts", secs_b)?;
    let outcome = reconnect::publish_two_halves(&cfg, &work, &ts_a, &ts_b, secs_a, secs_b).await?;
    let result = reconnect::verify_reconnect(&cfg, &work, &outcome);
    if work.keep() { eprintln!("record e2e reconnect: kept {} ({} files)", work.path().display(), outcome.outcome.files.len()); work.leak(); }
    result?;
    Ok(())
}

/// Churn: N separately encoded parts, N sequential sessions, one archive.
/// Each part starts on an IDR like a real reconnecting encoder; the
/// publication id in the pattern keeps the restarted segment numbers apart.
/// The total synthetic duration is split across churn_count parts so a short
/// local loop and a long nightly both exercise rapid reconnects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs ffmpeg/ffprobe; CI runs this with --ignored"]
async fn record_churn_keeps_every_sample() -> TestResult {
    if generate::missing_tools().is_some() {
        eprintln!("skipping record churn e2e: ffmpeg and/or ffprobe are not on PATH");
        return Ok(());
    }
    let cfg = config::E2eConfig::from_env();
    let total = cfg.duration_secs;
    // Every part must hold at least one full segment boundary plus margin
    // (see min_secs_per_part): shrink the session count to fit rather than
    // failing inside preroll.
    let min_part = cfg.min_secs_per_part();
    if total < 2 * min_part {
        return Err(format!("record churn needs at least {}s total with {}s segments, got {}s; raise RUSHLS_TEST_RECORD_E2E_SECS or lower RUSHLS_TEST_RECORD_E2E_CHURN_COUNT", 2 * min_part, cfg.segment_secs, total).into());
    }
    let max_n = (total / min_part) as usize;
    let wanted = cfg.churn_count.min(cfg.duration_secs).max(2) as usize;
    let n = wanted.min(max_n).max(2);
    let base = total / n as u64;
    let rem = (total % n as u64) as usize;
    let mut secs = Vec::with_capacity(n);
    for i in 0..n {
        let extra = if i < rem { 1 } else { 0 };
        secs.push(base + extra);
    }
    eprintln!("record e2e churn: {} sessions over {}s synthetic ({} @ {}fps, wanted {})", n, total, cfg.size, cfg.fps, cfg.churn_count);
    let mut work = harness::WorkDir::new(&cfg)?;
    if work.keep() { eprintln!("record e2e churn: workdir {}", work.path().display()); }
    let mut files = Vec::with_capacity(n);
    for (i, s) in secs.iter().enumerate() {
        let name = format!("src-churn-{:02}.ts", i);
        files.push(generate::generate_mpegts_named(&cfg, &work, &name, *s)?);
    }
    let churn = reconnect::publish_many_halves(&cfg, &work, &files, &secs).await?;
    let result = reconnect::verify_many_halves(&cfg, &work, &churn.outcome, &churn.secs);
    if work.keep() { eprintln!("record e2e churn: kept {} ({} files)", work.path().display(), churn.outcome.files.len()); work.leak(); }
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
    let rig = harness::TestRig::start(work.archive(), "{rendition}_{segment}.mp4", &cfg)?;
    let result = rig.run_burst(bytes[..cut].to_vec()).await;
    let drained = rig.drain_and_collect().await?;
    let files = drained.files.len();
    let checked = malformed::check_truncated_prefix(&cfg, &work, &result, &drained);
    if work.keep() { eprintln!("record e2e truncated: kept {} ({} files)", work.path().display(), files); work.leak(); }
    checked?;
    Ok(())
}
