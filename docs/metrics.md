# Metrics

Rushls exposes node metrics at `/metrics` and stream diagnostics at `/metrics/streams`.
The `[metrics]` configuration controls the listener and bearer authentication. Both endpoints use the same authorization rules.

## Diagnose a reported stall

| Question | Measurement |
|---|---|
| Is a publisher still active? | `rushls_stream_publisher_active` |
| Did the origin miss an output deadline? | `rushls_rendition_output_overdue_seconds` and `rushls_rendition_output_deadline_misses_total` |
| Does media keep pace with real time? | `rushls_rendition_output_lag_seconds` and `rate(rushls_rendition_media_published_seconds_total[1m])` |
| Which rendition is behind? | `rushls_rendition_behind_fastest_seconds` |
| What is the largest media difference? | `rushls_stream_rendition_media_skew_seconds` |
| How much later did siblings publish equivalent media? | `rushls_stream_rendition_publish_spread_seconds` |
| Did input stop first? | Per-track source and normalization timestamps |
| Did serving fail despite timely output? | HTTP status, body outcomes, operation timing, and public-path probe |

Output metrics describe media committed to the origin store. Rushls renders playlists on request.
Healthy output alone does not prove healthy HTTP serving, CDN freshness, network throughput, or player decoding.
The public-path probe supplies another observation. Player buffer and rebuffer telemetry remain external to Rushls.

## Publication timing

Rendition measurements use `stream`, `rendition`, and `kind` labels.
The rendition ID is the durable delivery identity, such as `rendition/0`.
`rushls_rendition_source_info` maps each output to its source track.
Session track metrics identify the current input session separately.

The successful store commit records publication time after request-facing snapshot updates.
Commit serialization prevents displaced publishers from updating successor metrics.
The measurement includes store work and lock delay before the commit. It precedes HTTP response and network transfer.

Monotonic elapsed time drives deadlines, intervals, lag, and publication spread.
Pacing sleeps remain part of elapsed time because viewers also wait through them.
Unix timestamps support correlation across systems, but clock agreement is necessary for external age calculations.

| Family | Definition |
|---|---|
| `rushls_rendition_output_expected` | One for active continuous audio/video output, zero for sparse subtitles or disconnected publications |
| `rushls_rendition_output_started` | One after the first live media interval commits in the current publication |
| `rushls_rendition_startup_elapsed_seconds` | Elapsed time awaiting first output while output is expected |
| `rushls_rendition_startup_budget_seconds` | Maximum planned segment duration plus the publication tolerance |
| `rushls_rendition_expected_publish_interval_seconds` | Planned part interval, or segment interval for segment-only output |
| `rushls_rendition_publication_tolerance_seconds` | One planned interval of additional publication tolerance |
| `rushls_rendition_output_overdue_seconds` | Elapsed time beyond the current deadline, or zero when output is not expected |
| `rushls_rendition_output_deadline_misses_total` | Distinct silence episodes that cross their deadline |
| `rushls_rendition_last_publish_timestamp_seconds` | Unix time of the latest live media commit |
| `rushls_rendition_media_published_seconds_total` | Newly committed media duration, including accepted tail media, without part-parent double counting |
| `rushls_rendition_media_end_seconds` | Contiguous media end on the publication timeline |
| `rushls_rendition_output_lag_seconds` | Signed wall progress minus contiguous media progress from a shared baseline |
| `rushls_rendition_target_duration_seconds` | Advertised HLS segment target |
| `rushls_rendition_part_target_seconds` | Advertised part target, absent for segment-only output |
| `rushls_rendition_timeline_breaks_total` | Publications with noncontiguous output timestamps |
| `rushls_rendition_gaps_total` | Unfinished segments replaced by advertised gaps |

A part-based rendition advances on each committed part. A segment-only rendition advances on each committed segment.
Initialization, metadata changes, and part-parent completion do not advance the cadence clock.

The next-output budget is the planned interval plus its exported tolerance.
For example, a one-second interval permits two seconds between commits before overdue time increases.
This policy measures origin output continuity. It is not a claim about a universal HLS jitter limit or actual player buffer exhaustion.
The tolerance does not adapt to a slow publisher. It is currently fixed to one planned interval.

The first continuous-media commit establishes one shared wall/media baseline.
Each sibling retains its actual media offset. An independent baseline for each rendition would hide startup delay.
Negative lag means available media is ahead of elapsed time. Positive lag means output is behind that baseline.
This measurement is relative output drift, not camera capture latency.

A timestamp gap freezes the contiguous edge for the rest of that publication.
Later timestamps cannot erase missing media or make the origin appear to catch up.
The timeline-break counter identifies this condition. A new publication resets the baseline and continuity state.

The maintenance pass and stream scrapes detect deadlines during silence.
The commit path also detects a missed deadline before resetting the silence episode.
A shared latch prevents duplicate counting. Recovery between scrapes still increments the counter.
Node totals survive stream removal until process restart.
Rendition counters persist across compatible reconnects while the durable rendition remains retained.

Detected input closure disables live timing before paced tail processing.
Session shutdown, finalization, lease release, and takeover also disable the old publication.
The origin cannot know about a remote disconnect before its transport reports it.
Stored tail media still contributes to media-duration totals. Idle retention metrics remain available.

## Rendition comparison

Comparison labels add `group` to the stream identity.
`group/<key>` compares alternatives from one topology group.
`combination/<index>` compares continuous audio/video members in a playable combination.
Sparse subtitles and I-frame projections do not participate.

| Family | Definition |
|---|---|
| `rushls_stream_rendition_media_skew_seconds` | Maximum minus minimum contiguous media end among group members |
| `rushls_rendition_behind_fastest_seconds` | A member's media delay behind the fastest member in that group |
| `rushls_stream_rendition_publish_spread_seconds` | Arrival spread for the most recently resolved common media boundary |
| `rushls_stream_rendition_publish_spread_timestamp_seconds` | Unix time when that comparison resolved |
| `rushls_stream_rendition_comparisons_pending` | Comparisons still missing at least one member |
| `rushls_stream_rendition_comparison_age_seconds` | Age of the oldest retained unresolved comparison |
| `rushls_stream_rendition_comparisons_incomplete_total` | Comparisons discarded at the history limit or publication end |

The comparison uses shared presentation time after timebase conversion. It does not compare segment IDs or last-publication timestamps.
The first commit that crosses a common media boundary establishes arrival for each rendition.
A resolved spread alone cannot represent a missing rendition. Pending age and per-rendition startup/overdue metrics cover that case.

History is bounded to 256 intervals per rendition and 256 unresolved boundaries per comparison group.
Discarded comparisons increment an explicit counter. Their omission must not look like a healthy zero.
Group state resets on publication changes. A node counter preserves discarded work across those changes.
Skew is absent until all members have a contiguous media end, and while the publisher is inactive.
A zero skew does not establish progress: all renditions can stop together.

Node histograms `rushls_output_publish_interval_seconds` and `rushls_output_publish_spread_seconds` preserve timing distributions without stream labels.
Spread observations include both alternative groups and playable combinations. They are comparison observations, not unique media objects.
Histograms use classic Prometheus buckets with `+Inf`, `_sum`, and `_count`.
Publication timing histograms stay node-wide; HTTP timing histograms are also available per retained stream.

## Input, operations, and HTTP

Track metrics start from the validated presentation and normalization timeline.
Unknown track IDs cannot create new metric entries.
Each track exports source packets, source payload bytes, normalized samples, normalized duration, and the latest timestamps.
`rushls_track_source_started` and `rushls_track_normalized_started` distinguish missing first progress from recent progress.

The source timestamp marks demultiplexed packet consumption at normalization. It is not a raw socket-arrival timestamp.
Normalization timestamps precede pacing and packaging. These measurements locate progress boundaries but do not establish keyframe eligibility for output.

`rushls_session_transport_loss_observable` is zero for the current source implementations.
There is no loss counter until a transport supplies a defined measurement.
Source bytes exclude transport overhead, and source packets are media packets rather than network packets.
Transport retransmissions and viewer throughput require external observations.

Operations use a bounded `operation` label:

| Operation | Boundary |
|---|---|
| `blocking_reload` | Explicit HLS position wait, including an immediately satisfied position |
| `initial_readiness` | First playlist wait for usable media |
| `hinted_part` | Request waiting for a published hinted part |
| `playlist_projection` | Actual playlist projection on a render-cache miss |
| `disk_read` | Retained payload disk read, including admission wait |
| `disk_write` | Spill write after worker dequeue |
| `store_backpressure` | Publisher readiness call awaiting store progress |

Each operation exports starts, in-flight work, finished outcomes, and a duration histogram.
Outcomes are `completed`, `ended`, `expired`, `error`, and `cancelled`.
Dropping an unfinished future records cancellation and releases its in-flight count.
The same operation boundary owns starts and outcomes.
Disk spill pending/capacity metrics cover queue pressure before write timing begins.

HTTP metrics classify resources as `playlist`, `media`, `operator`, or `other` from the request path.
Method labels are `GET`, `HEAD`, or `other`. Status labels contain response status codes.
Paths, tokens, viewers, and arbitrary header values never become labels.

| Family | Definition |
|---|---|
| `rushls_http_requests_started_total` | Requests entering admission, including refusals |
| `rushls_http_responses_total` | Response headers produced, by resource, method, and status |
| `rushls_http_requests_finished_total` | Request/body completion, cancellation, or error |
| `rushls_http_requests_in_flight` | Unfinished handlers and response bodies |
| `rushls_http_body_bytes_total` | Actual encoded and ranged body bytes yielded to the HTTP stack |
| `rushls_http_handler_duration_seconds` | Handler duration, including intentional protocol waits |
| `rushls_http_body_duration_seconds` | Body lifetime until completion, cancellation, or error |
| `rushls_http_admission_refusals_total` | Request or connection capacity refusals |
| `rushls_http_connections` | Admitted HTTP connections currently open |
| `rushls_http_connection_capacity` | Configured connection budget |
| `rushls_http_request_capacity` | Configured request budget |

Stream HTTP metrics are exposed only at `/metrics/streams` with the prefix `rushls_stream_http_`.
They cover requests started, requests in flight, responses, failures, body bytes, request outcomes, and both duration histograms.
Each family adds a `stream` label to the corresponding node metric labels.
Only recognized playlist/media paths for an existing retained stream receive this label.
Unknown streams remain in node totals; arbitrary request paths do not create stream entries.
Counters survive compatible publisher reconnects while the stream is retained and disappear when the stream is retired.
A later publication under the same name starts new counters; use `rate` or `increase` to handle resets.

`rushls_http_failures_total` and `rushls_stream_http_failures_total` add a bounded `reason` label:
`unknown_stream`, `unknown_rendition`, `unknown_resource`, `invalid_directive`, `unsatisfied`,
`projection`, `unauthorized`, `forbidden`, `admission`, or `other`.
A completed HTTP body can still carry a 404 or 503; use response status and failure reason alongside body outcomes.
`unknown_resource` does not distinguish an expired object from an object that never existed.
These metrics do not include request URLs, tokens, media sequence numbers, or viewer identities.
Connection refusals occur before a request identifies a stream and remain node-wide.

For a stall affecting one stream, first compare output progress with HTTP failures:

```promql
sum by (resource, status) (
  increase(rushls_stream_http_responses_total{stream="live/STREAM_ID"}[5m])
)
sum by (resource, reason) (
  increase(rushls_stream_http_failures_total{stream="live/STREAM_ID"}[5m])
)
histogram_quantile(0.95,
  sum by (le, resource) (
    rate(rushls_stream_http_body_duration_seconds_bucket{stream="live/STREAM_ID"}[5m])
  )
)
```

Handler time includes intentional blocking reload waits. A high value alone does not establish an incident.
Compare body duration and cancellation with the stream's output lag and overdue metrics.
HTTP histograms add 17 series each per observed resource class (14 finite buckets, `+Inf`, sum, and count).
Unused resource classes are omitted from stream exports. Choose a scrape interval shorter than stream retention to capture final counters.

HTTP body completion does not prove viewer receipt or playback.
A client can disconnect after the server hands bytes to its HTTP stack.
Malformed requests rejected by the HTTP parser never enter middleware request totals.
Connection admission counters provide evidence for sockets refused before request handling.

`rushls_media_resolved_total` and `rushls_media_resolved_bytes_total` measure media lookup before HTTP transformations.
`rushls_hls_playlists_resolved_total` and `rushls_hls_playlist_projections_total` distinguish successful resolution from cache-miss projection.
These counters do not measure transferred bytes.
HTTP totals are authoritative for status outcomes, including middleware denials and admission refusals.

The dedicated metrics listener still shares the process HTTP budget with viewer listeners.
Under saturation, scrapes can fail. Monitoring must alert on scrape failure and must not replace missing values with healthy zeros.
A separate operator budget remains a deployment/runtime change outside this metric contract.

## Capacity and lifecycle

Session lifecycle, segmentation failures, recording losses, TLS handshakes, playback denials, and hook delivery metrics remain available.
`rushls_publishers_address_limited_total` counts connections refused by `limits.publishers_per_address`; the matching event names the address.
These families have useful existing meanings and do not require replacement solely for naming consistency.
Detailed failure text stays in events rather than metric labels.
`rushls_session_info` carries principal identity. Numeric session metrics carry only session and stream identity.

`rushls_session_pipeline_capacity_bytes` reports the configured accounted-buffer limit.
`rushls_session_pipeline_used_bytes` and `rushls_session_pipeline_peak_bytes` report
current and peak reservations. `rushls_session_pipeline_reserve_bytes` reports
the part of the capacity that only packaging output may use.
`rushls_session_pipeline_capacity_bytes` is `0` for an unlimited budget.
`rushls_session_pipeline_exhaustions_total` counts failed reservations.
`rushls_pipeline_exhaustions_total` retains these counts after sessions finish.
`rushls_session_pipeline_allocation_bytes{origin="transport|demux|normalization|mux|subtitle"}`
attributes live reservations to their allocation origin. Shared bytes keep that
origin as they move between stages. These metrics exclude native MPEG-TS/QUIC
buffers, uninstrumented metadata, OS buffers, and allocator overhead.
`rushls_session_pacing_active` reports pacer state, not all reasons for backpressure.
`store_backpressure` operation metrics cover the separate store-readiness path.

Retention includes idle streams and separates payload, manifest, and disk bytes.
`rushls_stream_retention_max_seconds` reports the longest advertised rendition duration.
`rushls_stream_retention_min_seconds` reports the shortest current continuous-media rendition duration.
`rushls_rendition_retention_seconds` exposes each current rendition separately.
These durations are not an exact intersection of playable timeline intervals.

Node lifecycle counts classify one stream inventory directly. They do not subtract independently sampled unsigned counts.
`rushls_session_capacity`, `rushls_disk_spill_capacity`, and `rushls_build_info` provide capacity and version context.
Host CPU, RSS, filesystem space, file descriptors, and network saturation remain the responsibility of infrastructure exporters.

## Queries and dashboard

The [Grafana dashboard](../examples/monitoring/README.md) gives an overview of ingest, output health, viewers, retention, and hooks.
A second [diagnostics dashboard](../examples/monitoring/diagnostics.json) follows one stream through a reported stall.
Both select a Prometheus data source at import.
The [alert rules](../examples/monitoring/alerts.yml) provide initial thresholds for evaluation, not universal playback guarantees.
A five-second scrape interval is a reasonable starting point for these examples.
Detailed cadence still comes from commit observations, so short incidents do not depend solely on scrape frequency.

```promql
# Worst current deadline lateness in each stream.
max by (instance, stream) (rushls_rendition_output_overdue_seconds)

# Media seconds per wall second, per rendition.
rate(rushls_rendition_media_published_seconds_total[1m])

# Delays in the output pipeline, including cancellations and errors.
rate(rushls_operation_duration_seconds_sum{operation="store_backpressure"}[1m])

# Actual response body throughput, excluding operator endpoints.
sum by (instance, resource) (
  rate(rushls_http_body_bytes_total{resource!="operator"}[1m])
)
```

A single instant rate cannot diagnose every short stall. Deadline counters and histogram observations preserve those incidents.
Prometheus also needs `up` alerts for both scrape jobs, independently of the application gauges.
Stream cardinality depends on publisher churn and retention. Node histograms use only bounded label dimensions.

## Public playback probe

The [probe](../tools/probe_playback.py) performs one read-only pass through a public HLS URL.
It fetches active audio/video playlists and the latest published media resource from each playlist.
It does not fetch preload hints or pretend to implement player decoding.

```sh
python3 tools/probe_playback.py 'https://playback.example/live/stream.m3u8'
```

`--bearer-token-file` supplies optional authorization. The probe sends that header only to the initial origin.
Cross-origin redirects remove the authorization header. URL query credentials follow ordinary HLS URI resolution and explicit query-variable substitution.
The output never contains URLs or tokens.

The probe emits success, total duration, observation time, playlist duration, media download duration, media bytes, and available media-end timestamps.
Its rendition label is the index within that probe's topology, not Rushls' durable rendition identity.
The probe enforces request time, playlist size, media size, and rendition-count limits.
Unsupported playlists or responses produce failure rather than fabricated healthy measurements.

An external scheduler can run the probe and publish its output through a textfile collector.
That scheduler must replace the output atomically and retain failed results.
Probe age must have its own alert, because an old success result does not establish current health.
Program-date-time freshness is an origin timeline observation, not a trusted capture timestamp.

## Timestamp rejections

`rushls_timestamp_rejections_total{code,media_kind}` counts failed sessions with a typed timestamp issue.
Each session increments one series once. Labels use fixed issue codes and media kinds.
The series appears after its first rejection. Track IDs and timestamps are available in failure events, not metric labels.
See [input handling](input-handling.md#when-a-publication-is-rejected) for configuration and hook fields.

### Audio compensation

`rushls_audio_repairs_total{codec,method}` counts compensated holes during normalization.
`rushls_audio_compensation_seconds_total{codec,method}` counts exact missing audio duration for `method="gap"`.
Labels use bounded codec and method values. They do not include track IDs or timestamps.
These counters describe normalization, including media that a later pipeline failure prevents from reaching playback.
See [input handling](input-handling.md#hooks-logs-and-metrics) for policy and host notifications.

## Video cadence

`rushls_video_timestamp_step_seconds{codec}` is a histogram of usable source intervals, including nominal and unknown cadence.
These observations do not prove packet loss.
`rushls_video_cadence_violations_total{codec}` counts violations of explicitly declared cadence, including rejected input.
`rushls_video_compensation_seconds_total{codec,method}` counts accepted excess duration; its method is `gap`.
Unavailable validation does not increment compensation counters.
See [input handling](input-handling.md) for policy and hooks.

## Optional allocator measurements

Binaries built with `--features allocation-counting` expose three additional counters:

| Metric | Meaning |
|---|---|
| `rushls_allocator_calls_total` | Successful Rust allocations and reallocations |
| `rushls_allocator_allocated_bytes_total` | Requested bytes, including complete replacement allocations |
| `rushls_allocator_freed_bytes_total` | Released bytes, including allocations replaced by reallocations |

The difference between allocated and freed bytes approximates current requested Rust heap bytes.
The counters exclude native-library allocations and allocator overhead. Their snapshots are not atomic as a group.
Ordinary binaries omit this instrumentation. See [load validation](development/load-testing.md) for the measured workload and limits.
