use super::exposition::Samples;
use crate::{
    delivery::store::{
        LiveStream,
        telemetry::{
            GroupTimingSnapshot, PublicationSnapshot, PublicationTotalSnapshot,
            RenditionTimingSnapshot,
        },
    },
    domain::{MediaKind, RenditionId, StreamId},
    observe::{
        Operation, OperationOutcome, OperationSnapshot, counters::series, tracks::TrackSnapshot,
    },
};
use rushls_common::metrics::escape_label;

pub fn totals(output: &mut Samples, totals: &PublicationTotalSnapshot) {
    output.counter(
        "rushls_output_gaps_total",
        "Open segments replaced by advertised gaps when their publication ends.",
        "reason=\"unfinished_segment\"",
        totals.gaps,
    );
    output.counter(
        "rushls_output_deadline_misses_total",
        "Missed rendition output deadlines, once per silence episode; survives stream removal.",
        "",
        totals.deadline_misses,
    );
    output.counter(
        "rushls_output_timeline_breaks_total",
        "Noncontiguous output timestamps within a publication.",
        "",
        totals.timeline_breaks,
    );
    output.counter(
        "rushls_output_incomplete_comparisons_total",
        "Media-boundary comparisons discarded at history capacity or publication end.",
        "",
        totals.incomplete_comparisons,
    );
    output.histogram(
        "rushls_output_publish_interval_seconds",
        "Elapsed wall time between successful continuous-media commits, including pacing.",
        "",
        &totals.intervals,
    );
    output.histogram(
        "rushls_output_publish_spread_seconds",
        "Arrival spread for matched media boundaries within comparison groups.",
        "",
        &totals.spreads,
    );
}

pub fn operations(output: &mut Samples, snapshots: &[OperationSnapshot; 7]) {
    for operation in Operation::ALL {
        let labels = format!("operation=\"{}\"", operation.name());
        let snapshot = &snapshots[operation as usize];
        output.counter(
            "rushls_operations_started_total",
            "Operations entered, including immediately satisfied waits.",
            &labels,
            snapshot.started,
        );
        output.gauge(
            "rushls_operations_in_flight",
            "Operations entered but not completed or cancelled.",
            &labels,
            snapshot.in_flight,
        );
        for outcome in OperationOutcome::ALL {
            output.counter(
                "rushls_operations_finished_total",
                "Operation outcomes, with cancellation accounted at future drop.",
                &format!("{labels},outcome=\"{}\"", outcome.name()),
                snapshot.outcomes[outcome as usize],
            );
        }
        output.histogram(
            "rushls_operation_duration_seconds",
            "Operation wall duration, including intentional waits.",
            &labels,
            &snapshot.duration,
        );
    }
}

// Keep this declarative metric table adjacent to its HELP text.
series! {
    RENDITION_SERIES: RenditionTimingSnapshot {
        Gauge("rushls_rendition_output_expected",
            "Live cadence is expected for this current continuous-media rendition.")
            = |rendition: &RenditionTimingSnapshot| rendition.expected,
        Gauge("rushls_rendition_output_started",
            "Usable output arrived in the current publication.")
            = |rendition: &RenditionTimingSnapshot| rendition.timing.last_timestamp.is_some(),
        Gauge("rushls_rendition_startup_elapsed_seconds",
            "Wall time awaiting first output while cadence is expected.")
            = |rendition: &RenditionTimingSnapshot| rendition.startup_elapsed.as_secs_f64(),
        Gauge("rushls_rendition_startup_budget_seconds",
            "First-output deadline budget from publication attachment.")
            = |rendition: &RenditionTimingSnapshot| rendition.timing.startup_budget.as_secs_f64(),
        Gauge("rushls_rendition_output_overdue_seconds",
            "Elapsed wall time beyond the current output deadline; zero when not expected.")
            = |rendition: &RenditionTimingSnapshot| rendition.overdue.as_secs_f64(),
        Gauge("rushls_rendition_expected_publish_interval_seconds",
            "Planned part or segment interval, independent of observed speed.")
            = |rendition: &RenditionTimingSnapshot| rendition.timing.interval.as_secs_f64(),
        Gauge("rushls_rendition_publication_tolerance_seconds",
            "Publication jitter budget added to the planned interval.")
            = |rendition: &RenditionTimingSnapshot| rendition.timing.tolerance.as_secs_f64(),
        Gauge("rushls_rendition_target_duration_seconds",
            "Advertised HLS segment target duration.")
            = |rendition: &RenditionTimingSnapshot| rendition.timing.target.as_secs_f64(),
        Counter("rushls_rendition_media_published_seconds_total",
            "Usable media duration committed once, excluding initialization and part-parent completion.")
            = |rendition: &RenditionTimingSnapshot| rendition.timing.media_seconds,
        Counter("rushls_rendition_output_deadline_misses_total",
            "Distinct output silence episodes crossing their deadline.")
            = |rendition: &RenditionTimingSnapshot| rendition.timing.deadline_misses,
        Counter("rushls_rendition_timeline_breaks_total",
            "Publications whose media timestamps stopped being contiguous.")
            = |rendition: &RenditionTimingSnapshot| rendition.timing.timeline_breaks,
    }
}

pub fn stream(
    output: &mut Samples,
    id: &StreamId,
    live: &LiveStream,
    snapshot: &PublicationSnapshot,
) {
    let stream = format!("stream=\"{}\"", escape_label(id.as_str()));
    output.gauge(
        "rushls_stream_publisher_active",
        "A publisher currently expects live output; excludes disconnected tail draining.",
        &stream,
        u8::from(snapshot.active),
    );
    for r in &snapshot.renditions {
        let t = &r.timing;
        let labels = rendition_labels(&stream, t.id, t.kind);
        output.series(RENDITION_SERIES, &labels, r);
        output.optional_gauge(
            "rushls_rendition_part_target_seconds",
            "Advertised HLS part target duration.",
            &labels,
            t.part_target.map(|part| part.as_secs_f64()),
        );
        output.optional_gauge(
            "rushls_rendition_last_publish_timestamp_seconds",
            "Unix time of the latest live usable-media commit.",
            &labels,
            t.last_timestamp,
        );
        output.optional_gauge(
            "rushls_rendition_output_lag_seconds",
            "Signed wall progress minus contiguous media progress from a common publication baseline.",
            &labels,
            r.lag,
        );
        output.optional_gauge(
            "rushls_rendition_media_end_seconds",
            "Contiguous media end on the publication timeline; freezes across timestamp gaps.",
            &labels,
            r.media_end,
        );
        for track in t.sources.iter() {
            output.gauge(
                "rushls_rendition_source_info",
                "Current mapping from an output rendition to its source track.",
                &format!("{labels},track=\"{}\"", track.0),
                1,
            );
        }
    }
    for group in &snapshot.groups {
        comparison_group(output, &stream, group, &snapshot.renditions);
    }
    for (rendition, kind, count) in live.gap_counts() {
        let mut labels = rendition_labels(&stream, rendition, kind);
        labels.push_str(",reason=\"unfinished_segment\"");
        output.counter(
            "rushls_rendition_gaps_total",
            "Open segments replaced by advertised gaps at publication end.",
            &labels,
            count,
        );
    }
    let mut minimum: Option<f64> = None;
    for (rendition, media_kind, held) in live.rendition_retention() {
        output.gauge(
            "rushls_rendition_retention_seconds",
            "Advertised media duration for one current rendition, including open parts.",
            &rendition_labels(&stream, rendition, media_kind),
            held.as_secs_f64(),
        );
        if media_kind != MediaKind::Subtitle {
            minimum = Some(minimum.map_or(held.as_secs_f64(), |m| m.min(held.as_secs_f64())));
        }
    }
    output.optional_gauge(
        "rushls_stream_retention_min_seconds",
        "Shortest current continuous-media rendition duration; not a timeline intersection.",
        &stream,
        minimum,
    );
}

series! {
    GROUP_SERIES: GroupTimingSnapshot {
        Gauge("rushls_stream_rendition_comparisons_pending",
            "Matched-boundary comparisons still missing one or more renditions.")
            = |group: &GroupTimingSnapshot| group.pending,
        Gauge("rushls_stream_rendition_comparison_age_seconds",
            "Age of the oldest retained unresolved boundary comparison.")
            = |group: &GroupTimingSnapshot| group.pending_age.as_secs_f64(),
        Counter("rushls_stream_rendition_comparisons_incomplete_total",
            "Unresolved comparisons discarded in the current publication.")
            = |group: &GroupTimingSnapshot| group.incomplete,
    }
}

fn comparison_group(
    output: &mut Samples,
    stream: &str,
    group: &GroupTimingSnapshot,
    renditions: &[RenditionTimingSnapshot],
) {
    let labels = format!("{stream},group=\"{}\"", escape_label(&group.name));
    if let Some(skew) = group.skew {
        let members = renditions
            .iter()
            .filter(|r| group.members.contains(&r.timing.id));
        let fastest = members.clone().filter_map(|r| r.media_end).reduce(f64::max);
        for r in members {
            if let Some((fastest, end)) = fastest.zip(r.media_end) {
                output.gauge(
                    "rushls_rendition_behind_fastest_seconds",
                    "Media timeline delay behind the fastest member of this comparison group.",
                    &rendition_labels(&labels, r.timing.id, r.timing.kind),
                    fastest - end,
                );
            }
        }
        output.gauge(
            "rushls_stream_rendition_media_skew_seconds",
            "Fastest minus slowest contiguous media end within the active comparison group.",
            &labels,
            skew,
        );
    }
    output.optional_gauge(
        "rushls_stream_rendition_publish_spread_seconds",
        "Most recently resolved matched-media-boundary arrival spread.",
        &labels,
        group.spread.map(|spread| spread.as_secs_f64()),
    );
    output.optional_gauge(
        "rushls_stream_rendition_publish_spread_timestamp_seconds",
        "Unix time at which the latest matched boundary comparison resolved.",
        &labels,
        group.spread_timestamp,
    );
    output.series(GROUP_SERIES, &labels, group);
}

fn rendition_labels(scope: &str, id: RenditionId, kind: MediaKind) -> String {
    format!("{scope},rendition=\"{id}\",kind=\"{}\"", kind_name(kind))
}

pub fn kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Subtitle => "subtitle",
    }
}

pub fn http(output: &mut Samples, snapshot: &crate::observe::http::HttpSnapshot) {
    output.gauge(
        "rushls_http_connections",
        "Admitted HTTP connections currently open.",
        "",
        snapshot.connections,
    );
    output.gauge(
        "rushls_http_connection_capacity",
        "Configured maximum concurrent HTTP connections.",
        "",
        snapshot.connection_capacity,
    );
    output.gauge(
        "rushls_http_request_capacity",
        "Configured maximum admitted HTTP requests including response bodies.",
        "",
        snapshot.request_capacity,
    );
    output.counter(
        "rushls_http_admission_refusals_total",
        "HTTP work refused by the admission budget.",
        "scope=\"request\"",
        snapshot.requests_rejected,
    );
    output.counter(
        "rushls_http_admission_refusals_total",
        "HTTP work refused by the admission budget.",
        "scope=\"connection\"",
        snapshot.connections_rejected,
    );
    http_requests(output, snapshot, None);
}

/// Stream families are exported only on /metrics/streams. Retained stream
/// ownership bounds their lifetime; unused resource classes add no series.
pub fn http_requests(
    output: &mut Samples,
    snapshot: &crate::observe::http::HttpSnapshot,
    stream: Option<&StreamId>,
) {
    use crate::observe::http::HttpResource;
    macro_rules! metric {
        ($suffix:literal) => {
            if stream.is_some() {
                concat!("rushls_stream_http_", $suffix)
            } else {
                concat!("rushls_http_", $suffix)
            }
        };
    }
    let prefix = stream.map_or_else(String::new, |id| {
        format!("stream=\"{}\",", escape_label(id.as_str()))
    });
    for resource in HttpResource::ALL {
        let class = &snapshot.classes[resource as usize];
        if stream.is_some() && class.started == 0 {
            continue;
        }
        let labels = format!("{prefix}resource=\"{}\"", resource.name());
        output.counter(
            metric!("requests_started_total"),
            "HTTP requests entering admission, including refusals.",
            &labels,
            class.started,
        );
        output.gauge(
            metric!("requests_in_flight"),
            "HTTP request handlers and bodies not yet finished.",
            &labels,
            class.in_flight,
        );
        output.counter(
            metric!("body_bytes_total"),
            "Encoded and ranged response body bytes yielded to HTTP, not acknowledged by viewers.",
            &labels,
            class.body_bytes,
        );
        for (outcome, count) in [
            ("completed", class.completed),
            ("cancelled", class.cancelled),
            ("error", class.errors),
        ] {
            output.counter(
                metric!("requests_finished_total"),
                "Request and body lifetime outcomes.",
                &format!("{labels},outcome=\"{outcome}\""),
                count,
            );
        }
        output.histogram(
            metric!("handler_duration_seconds"),
            "HTTP handler wall duration, including protocol waits.",
            &labels,
            &class.handler_duration,
        );
        output.histogram(
            metric!("body_duration_seconds"),
            "Response body lifetime until completion, error, or cancellation.",
            &labels,
            &class.body_duration,
        );
    }
    for ((resource, method, status), count) in &snapshot.responses {
        output.counter(
            metric!("responses_total"),
            "HTTP response headers produced, before body transfer.",
            &format!(
                "{prefix}resource=\"{}\",method=\"{}\",status=\"{status}\"",
                resource.name(),
                method.name()
            ),
            count,
        );
    }
    for ((resource, reason), count) in &snapshot.failures {
        output.counter(
            metric!("failures_total"),
            "HTTP error responses by bounded cause; unknown_resource does not distinguish expired from nonexistent media.",
            &format!("{prefix}resource=\"{}\",reason=\"{}\"", resource.name(), reason.name()),
            count,
        );
    }
}

series! {
    TRACK_SERIES: TrackSnapshot {
        Gauge("rushls_track_source_started",
            "Source media arrived for this validated input track.")
            = |track: &TrackSnapshot| track.source_timestamp.is_some(),
        Gauge("rushls_track_normalized_started",
            "Normalization produced a sample for this input track.")
            = |track: &TrackSnapshot| track.normalized_timestamp.is_some(),
        Counter("rushls_track_source_packets_total",
            "Media packets consumed by normalization for this input track.")
            = |track: &TrackSnapshot| track.source_packets,
        Counter("rushls_track_source_payload_bytes_total",
            "Demultiplexed payload bytes consumed by normalization for this track.")
            = |track: &TrackSnapshot| track.source_bytes,
        Counter("rushls_track_normalized_samples_total",
            "Samples emitted by normalization before pacing.")
            = |track: &TrackSnapshot| track.normalized_samples,
        Counter("rushls_track_normalized_media_seconds_total",
            "Sum of normalized sample durations, before output packaging.")
            = |track: &TrackSnapshot| track.normalized_seconds,
    }
}

pub fn tracks(output: &mut Samples, session: &crate::session::SessionSnapshot) {
    for track in &session.track_progress {
        let labels = format!(
            "stream=\"{}\",session=\"{}\",track=\"{}\",kind=\"{}\"",
            escape_label(session.stream.as_str()),
            session.id,
            track.id.0,
            kind_name(track.kind)
        );
        output.series(TRACK_SERIES, &labels, track);
        output.optional_gauge(
            "rushls_track_last_source_timestamp_seconds",
            "Unix time of the last demultiplexed packet consumed by normalization.",
            &labels,
            track.source_timestamp,
        );
        output.optional_gauge(
            "rushls_track_last_normalized_timestamp_seconds",
            "Unix time of the last normalized sample before pacing.",
            &labels,
            track.normalized_timestamp,
        );
        output.optional_gauge(
            "rushls_track_normalized_media_end_seconds",
            "Furthest normalized presentation end, including timeline offsets.",
            &labels,
            track.normalized_end,
        );
    }
}
