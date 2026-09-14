// Paused-clock fixtures use exactly representable whole seconds.
#![allow(clippy::float_cmp)]
use super::*;
mod fixtures;
use fixtures::*;

#[tokio::test(start_paused = true)]
async fn silent_output_ages_and_counts_only_one_miss_until_recovery() {
    let mut output = publication(vec![descriptor(0, MediaKind::Video, 1000)]);
    output.committed(1, RenditionId(0), Some((0, 1000)));
    advance(3).await;
    let snapshot = output.snapshot();
    assert_eq!(snapshot.renditions[0].overdue, Duration::from_secs(1));
    assert_eq!(snapshot.renditions[0].lag, Some(2.0));
    assert_eq!(snapshot.renditions[0].timing.deadline_misses, 1);
    advance(2).await;
    assert_eq!(output.snapshot().renditions[0].timing.deadline_misses, 1);
    output.committed(1, RenditionId(0), Some((1000, 1000)));
    assert_eq!(output.snapshot().renditions[0].overdue, Duration::ZERO);
    advance(3).await;
    output.committed(1, RenditionId(0), Some((2000, 1000)));
    assert_eq!(
        output.snapshot().renditions[0].timing.deadline_misses,
        2,
        "recovery records a miss even without an intervening scrape or tick"
    );
    assert_eq!(output.totals.0.lock().intervals.count, 2);
}

#[tokio::test(start_paused = true)]
async fn equal_rendition_stalls_have_zero_skew_but_positive_lag() {
    let mut output = publication(vec![
        descriptor(0, MediaKind::Video, 1000),
        descriptor(1, MediaKind::Video, 90_000),
    ]);
    output.committed(1, RenditionId(0), Some((0, 1000)));
    output.committed(1, RenditionId(1), Some((0, 90_000)));
    advance(4).await;
    let snapshot = output.snapshot();
    assert_eq!(snapshot.groups[0].skew, Some(0.0));
    assert!(
        snapshot
            .renditions
            .iter()
            .all(|r| r.lag == Some(3.0) && r.overdue == Duration::from_secs(2))
    );
}

#[tokio::test(start_paused = true)]
async fn skew_compares_media_time_and_spread_matches_the_same_boundary() {
    let mut output = publication(vec![
        descriptor(0, MediaKind::Video, 1000),
        descriptor(1, MediaKind::Video, 90_000),
    ]);
    output.committed(1, RenditionId(0), Some((0, 1000)));
    advance(1).await;
    output.committed(1, RenditionId(1), Some((0, 45_000)));
    let snapshot = output.snapshot();
    assert_eq!(snapshot.groups[0].skew, Some(0.5));
    assert_eq!(snapshot.groups[0].spread, None);
    assert_eq!(snapshot.groups[0].pending_age, Duration::from_secs(1));
    advance(1).await;
    output.committed(1, RenditionId(1), Some((45_000, 45_000)));
    let snapshot = output.snapshot();
    assert_eq!(snapshot.groups[0].skew, Some(0.0));
    assert_eq!(snapshot.groups[0].spread, Some(Duration::from_secs(2)));
    assert_eq!(snapshot.groups[0].pending, 0);
}

#[tokio::test(start_paused = true)]
async fn missing_first_media_is_distinct_from_sparse_subtitles() {
    let mut output = publication(vec![
        descriptor(0, MediaKind::Video, 1000),
        descriptor(1, MediaKind::Subtitle, 1000),
    ]);
    advance(8).await;
    let snapshot = output.snapshot();
    assert!(snapshot.renditions[0].expected);
    assert_eq!(snapshot.renditions[0].overdue, Duration::from_secs(1));
    assert_eq!(
        snapshot.renditions[0].startup_elapsed,
        Duration::from_secs(8)
    );
    assert_eq!(snapshot.renditions[0].timing.last_timestamp, None);
    assert!(!snapshot.renditions[1].expected);
    assert_eq!(snapshot.renditions[1].timing.deadline_misses, 0);
    assert!(snapshot.groups.is_empty());
}

#[tokio::test(start_paused = true)]
async fn disconnect_disarms_deadlines_and_takeover_resets_baselines() {
    let d = descriptor(0, MediaKind::Video, 1000);
    let mut output = publication(vec![d.clone()]);
    output.committed(1, RenditionId(0), Some((0, 1000)));
    output.stop(1);
    advance(30).await;
    output.committed(1, RenditionId(0), Some((1000, 1000)));
    assert_eq!(output.snapshot().renditions[0].overdue, Duration::ZERO);
    assert_eq!(
        output.snapshot().renditions[0].timing.media_seconds,
        2.0,
        "tail media remains counted"
    );
    output.attach(
        2,
        [(RenditionId(0), d, Duration::from_secs(6))].into_iter(),
        vec![],
    );
    output.stop(1);
    output.committed(1, RenditionId(0), Some((9000, 1000)));
    let snapshot = output.snapshot();
    assert!(snapshot.active);
    assert_eq!(snapshot.renditions[0].lag, None);
    assert_eq!(snapshot.renditions[0].timing.last_timestamp, None);
    assert_eq!(snapshot.renditions[0].timing.media_seconds, 2.0);
    assert_eq!(snapshot.renditions[0].timing.deadline_misses, 0);
}

#[tokio::test(start_paused = true)]
async fn metadata_completion_and_timestamp_jumps_do_not_fake_progress() {
    let mut output = publication(vec![descriptor(0, MediaKind::Video, 1000)]);
    output.committed(1, RenditionId(0), None);
    assert_eq!(output.snapshot().renditions[0].timing.last_timestamp, None);
    output.committed(1, RenditionId(0), Some((0, 1000)));
    advance(3).await;
    output.committed(1, RenditionId(0), None);
    assert_eq!(output.snapshot().renditions[0].timing.media_seconds, 1.0);
    output.committed(1, RenditionId(0), Some((9000, 1000)));
    let snapshot = output.snapshot();
    assert_eq!(snapshot.renditions[0].media_end, Some(1.0));
    assert_eq!(snapshot.renditions[0].lag, Some(2.0));
    assert_eq!(snapshot.renditions[0].timing.timeline_breaks, 1);
}

#[tokio::test(start_paused = true)]
async fn comparison_history_is_bounded_and_discarded_work_is_counted() {
    let mut output = publication(vec![
        descriptor(0, MediaKind::Video, 1000),
        descriptor(1, MediaKind::Video, 1000),
    ]);
    for second in 0..300 {
        output.committed(1, RenditionId(0), Some((second * 1000, 1000)));
    }
    let snapshot = output.snapshot();
    assert_eq!(snapshot.groups[0].pending, HISTORY_LIMIT);
    assert_eq!(snapshot.groups[0].incomplete, 44);
    output.stop(1);
    assert_eq!(output.totals.0.lock().incomplete_comparisons, 300);
}
