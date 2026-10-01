use super::exposition::Samples;
use crate::{
    delivery::store::{
        LiveStream,
        telemetry::{PublicationSnapshot, PublicationTotalSnapshot},
    },
    domain::StreamId,
    observe::{Operation, OperationOutcome, OperationSnapshot},
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
#[allow(clippy::too_many_lines)]
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
        let labels = format!(
            "{stream},rendition=\"{}\",kind=\"{}\"",
            t.id,
            kind_name(t.kind)
        );
        output.gauge(
            "rushls_rendition_output_expected",
            "Live cadence is expected for this current continuous-media rendition.",
            &labels,
            u8::from(r.expected),
        );
        output.gauge(
            "rushls_rendition_output_started",
            "Usable output arrived in the current publication.",
            &labels,
            u8::from(t.last_timestamp.is_some()),
        );
        output.gauge(
            "rushls_rendition_startup_elapsed_seconds",
            "Wall time awaiting first output while cadence is expected.",
            &labels,
            r.startup_elapsed.as_secs_f64(),
        );
        output.gauge(
            "rushls_rendition_startup_budget_seconds",
            "First-output deadline budget from publication attachment.",
            &labels,
            t.startup_budget.as_secs_f64(),
        );
        output.gauge(
            "rushls_rendition_output_overdue_seconds",
            "Elapsed wall time beyond the current output deadline; zero when not expected.",
            &labels,
            r.overdue.as_secs_f64(),
        );
        output.gauge(
            "rushls_rendition_expected_publish_interval_seconds",
            "Planned part or segment interval, independent of observed speed.",
            &labels,
            t.interval.as_secs_f64(),
        );
        output.gauge(
            "rushls_rendition_publication_tolerance_seconds",
            "Publication jitter budget added to the planned interval.",
            &labels,
            t.tolerance.as_secs_f64(),
        );
        output.gauge(
            "rushls_rendition_target_duration_seconds",
            "Advertised HLS segment target duration.",
            &labels,
            t.target.as_secs_f64(),
        );
        if let Some(part) = t.part_target {
            output.gauge(
                "rushls_rendition_part_target_seconds",
                "Advertised HLS part target duration.",
                &labels,
                part.as_secs_f64(),
            );
        }
        if let Some(last) = t.last_timestamp {
            output.gauge(
                "rushls_rendition_last_publish_timestamp_seconds",
                "Unix time of the latest live usable-media commit.",
                &labels,
                last,
            );
        }
        if let Some(lag) = r.lag {
            output.gauge("rushls_rendition_output_lag_seconds", "Signed wall progress minus contiguous media progress from a common publication baseline.", &labels, lag);
        }
        if let Some(end) = r.media_end {
            output.gauge(
                "rushls_rendition_media_end_seconds",
                "Contiguous media end on the publication timeline; freezes across timestamp gaps.",
                &labels,
                end,
            );
        }
        output.counter("rushls_rendition_media_published_seconds_total", "Usable media duration committed once, excluding initialization and part-parent completion.", &labels, t.media_seconds);
        output.counter(
            "rushls_rendition_output_deadline_misses_total",
            "Distinct output silence episodes crossing their deadline.",
            &labels,
            t.deadline_misses,
        );
        output.counter(
            "rushls_rendition_timeline_breaks_total",
            "Publications whose media timestamps stopped being contiguous.",
            &labels,
            t.timeline_breaks,
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
    for g in &snapshot.groups {
        let labels = format!("{stream},group=\"{}\"", escape_label(&g.name));
        if g.skew.is_some() {
            let fastest = snapshot
                .renditions
                .iter()
                .filter(|r| g.members.contains(&r.timing.id))
                .filter_map(|r| r.media_end)
                .reduce(f64::max);
            for r in snapshot
                .renditions
                .iter()
                .filter(|r| g.members.contains(&r.timing.id))
            {
                if let Some((fastest, end)) = fastest.zip(r.media_end) {
                    output.gauge(
                        "rushls_rendition_behind_fastest_seconds",
                        "Media timeline delay behind the fastest member of this comparison group.",
                        &format!(
                            "{labels},rendition=\"{}\",kind=\"{}\"",
                            r.timing.id,
                            kind_name(r.timing.kind)
                        ),
                        fastest - end,
                    );
                }
            }
        }
        if let Some(skew) = g.skew {
            output.gauge(
                "rushls_stream_rendition_media_skew_seconds",
                "Fastest minus slowest contiguous media end within the active comparison group.",
                &labels,
                skew,
            );
        }
        if let Some(spread) = g.spread {
            output.gauge(
                "rushls_stream_rendition_publish_spread_seconds",
                "Most recently resolved matched-media-boundary arrival spread.",
                &labels,
                spread.as_secs_f64(),
            );
        }
        if let Some(at) = g.spread_timestamp {
            output.gauge(
                "rushls_stream_rendition_publish_spread_timestamp_seconds",
                "Unix time at which the latest matched boundary comparison resolved.",
                &labels,
                at,
            );
        }
        output.gauge(
            "rushls_stream_rendition_comparisons_pending",
            "Matched-boundary comparisons still missing one or more renditions.",
            &labels,
            g.pending,
        );
        output.gauge(
            "rushls_stream_rendition_comparison_age_seconds",
            "Age of the oldest retained unresolved boundary comparison.",
            &labels,
            g.pending_age.as_secs_f64(),
        );
        output.counter(
            "rushls_stream_rendition_comparisons_incomplete_total",
            "Unresolved comparisons discarded in the current publication.",
            &labels,
            g.incomplete,
        );
    }
    for (rendition, kind, count) in live.gap_counts() {
        output.counter(
            "rushls_rendition_gaps_total",
            "Open segments replaced by advertised gaps at publication end.",
            &format!(
                "{stream},rendition=\"{rendition}\",kind=\"{}\",reason=\"unfinished_segment\"",
                kind_name(kind)
            ),
            count,
        );
    }
    let mut minimum: Option<f64> = None;
    for (rendition, media_kind, held) in live.rendition_retention() {
        let kind = kind_name(media_kind);
        output.gauge(
            "rushls_rendition_retention_seconds",
            "Advertised media duration for one current rendition, including open parts.",
            &format!("{stream},rendition=\"{rendition}\",kind=\"{kind}\""),
            held.as_secs_f64(),
        );
        if media_kind != crate::domain::MediaKind::Subtitle {
            minimum = Some(minimum.map_or(held.as_secs_f64(), |m| m.min(held.as_secs_f64())));
        }
    }
    if let Some(minimum) = minimum {
        output.gauge(
            "rushls_stream_retention_min_seconds",
            "Shortest current continuous-media rendition duration; not a timeline intersection.",
            &stream,
            minimum,
        );
    }
}

pub fn kind_name(kind: crate::domain::MediaKind) -> &'static str {
    match kind {
        crate::domain::MediaKind::Video => "video",
        crate::domain::MediaKind::Audio => "audio",
        crate::domain::MediaKind::Subtitle => "subtitle",
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

pub fn tracks(output: &mut Samples, session: &crate::session::SessionSnapshot) {
    for track in &session.track_progress {
        let labels = format!(
            "stream=\"{}\",session=\"{}\",track=\"{}\",kind=\"{}\"",
            escape_label(session.stream.as_str()),
            session.id,
            track.id.0,
            kind_name(track.kind)
        );
        output.gauge(
            "rushls_track_source_started",
            "Source media arrived for this validated input track.",
            &labels,
            u8::from(track.source_timestamp.is_some()),
        );
        output.gauge(
            "rushls_track_normalized_started",
            "Normalization produced a sample for this input track.",
            &labels,
            u8::from(track.normalized_timestamp.is_some()),
        );
        if let Some(at) = track.source_timestamp {
            output.gauge(
                "rushls_track_last_source_timestamp_seconds",
                "Unix time of the last demultiplexed packet consumed by normalization.",
                &labels,
                at,
            );
        }
        if let Some(at) = track.normalized_timestamp {
            output.gauge(
                "rushls_track_last_normalized_timestamp_seconds",
                "Unix time of the last normalized sample before pacing.",
                &labels,
                at,
            );
        }
        if let Some(end) = track.normalized_end {
            output.gauge(
                "rushls_track_normalized_media_end_seconds",
                "Furthest normalized presentation end, including timeline offsets.",
                &labels,
                end,
            );
        }
        output.counter(
            "rushls_track_source_packets_total",
            "Media packets consumed by normalization for this input track.",
            &labels,
            track.source_packets,
        );
        output.counter(
            "rushls_track_source_payload_bytes_total",
            "Demultiplexed payload bytes consumed by normalization for this track.",
            &labels,
            track.source_bytes,
        );
        output.counter(
            "rushls_track_normalized_samples_total",
            "Samples emitted by normalization before pacing.",
            &labels,
            track.normalized_samples,
        );
        output.counter(
            "rushls_track_normalized_media_seconds_total",
            "Sum of normalized sample durations, before output packaging.",
            &labels,
            track.normalized_seconds,
        );
    }
}
