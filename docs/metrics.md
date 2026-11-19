# Metrics

Rushls exports Prometheus metrics on two endpoints configured under `[metrics]`:

- `/metrics` for node-wide totals and histograms (bounded cardinality).
- `/metrics/streams` for per-stream, per-rendition, and per-session detail.

Both endpoints share the listener and bearer-token settings from `[metrics]`.

## Diagnosing a stall

When viewers report buffering or a frozen stream, work down this checklist:

| Question | What to check |
| --- | --- |
| Is a publisher still connected? | `rushls_stream_publisher_active` |
| Did Rushls miss an output deadline? | `rushls_rendition_output_overdue_seconds` and `rushls_rendition_output_deadline_misses_total` |
| Is output keeping up with real time? | `rushls_rendition_output_lag_seconds` and `rate(rushls_rendition_media_published_seconds_total[1m])` |
| Which rendition fell behind? | `rushls_rendition_behind_fastest_seconds` |
| How far apart are renditions in media time? | `rushls_stream_rendition_media_skew_seconds` |
| How much later did sibling renditions finish the same boundary? | `rushls_stream_rendition_publish_spread_seconds` |
| Did ingest stall first? | Per-track source and normalization timestamps |
| Are media commits healthy while HTTP serving fails? | HTTP status codes, failure reasons, body duration, and the [public playback probe](#public-playback-probe) |

Output metrics reflect media committed to the origin store (playlists are rendered on request). Healthy origin output does not by itself rule out HTTP saturation, CDN caching issues, network drops, or player decoding bugs—combine origin metrics with the playback probe and client-side telemetry.

## Publication timing

Rendition series carry `stream`, `rendition` (the durable delivery ID, such as `rendition/0`), and `kind` labels.
`rushls_rendition_source_info` maps each rendition back to its source track, while session track metrics track the current ingest connection separately.

Commit times are recorded when media finishes committing to the store (including store locking, before HTTP transfer).
Deadlines, intervals, lag, and spread use monotonic time (including pacing sleeps, since viewers wait through them too); Unix timestamps are exported for cross-system correlation.

| Metric | Meaning |
| --- | --- |
| `rushls_rendition_output_expected` | `1` for active continuous audio/video output; `0` for sparse subtitles or disconnected publishers |
| `rushls_rendition_output_started` | `1` once the first live media interval commits in the current publication |
| `rushls_rendition_startup_elapsed_seconds` | Time spent waiting for first output while expected |
| `rushls_rendition_startup_budget_seconds` | Maximum planned segment duration plus publication tolerance |
| `rushls_rendition_expected_publish_interval_seconds` | Planned part interval, or segment interval for segment-only output |
| `rushls_rendition_publication_tolerance_seconds` | Additional tolerance before a commit counts as overdue (one planned interval) |
| `rushls_rendition_output_overdue_seconds` | Seconds past the current deadline (`0` when output is not expected) |
| `rushls_rendition_output_deadline_misses_total` | Distinct silence episodes that crossed their deadline |
| `rushls_rendition_last_publish_timestamp_seconds` | Unix timestamp of the latest live media commit |
| `rushls_rendition_media_published_seconds_total` | Newly committed media duration (including tail media, without double-counting parent segments of parts) |
| `rushls_rendition_media_end_seconds` | Contiguous media end on the publication timeline |
| `rushls_rendition_output_lag_seconds` | Wall-clock progress minus contiguous media progress from the stream's shared baseline |
| `rushls_rendition_target_duration_seconds` | Advertised HLS segment target |
| `rushls_rendition_part_target_seconds` | Advertised HLS part target (omitted for segment-only output) |
| `rushls_rendition_timeline_breaks_total` | Publications that experienced non-contiguous output timestamps |
| `rushls_rendition_gaps_total` | Unfinished segments replaced by `EXT-X-GAP` |

### How deadlines and lag work

- **Cadence clock:** Part-based renditions advance on every committed part; segment-only renditions advance on every committed segment. Init sections, metadata updates, and closing a parent segment whose parts already committed do not reset the clock.
- **Overdue threshold:** A rendition becomes overdue after `expected_publish_interval + publication_tolerance` (for example, 2 seconds for a 1-second part target). Deadlines are checked both on commits and during silence, so a brief stall that recovers between scrapes still increments `rushls_rendition_output_deadline_misses_total` once.
- **Shared lag baseline:** The first continuous-media commit establishes one wall/media baseline for the whole stream, so a slow-starting rendition cannot hide behind its own delayed baseline. Negative lag means media is slightly ahead of elapsed time; positive lag means output has drifted behind (this measures relative origin drift, not camera-to-glass latency).
- **Timeline breaks:** A timestamp gap freezes `media_end_seconds` for the rest of that publication so later timestamps cannot disguise missing media; `rushls_rendition_timeline_breaks_total` flags this until a new publication resets continuity.
- **Disconnects and persistence:** Orderly closure, shutdown, and takeover stop live timing before tail draining, while stored tail media still counts toward `media_published_seconds_total`. Node-wide totals survive stream retirement until process restart, and rendition counters persist across compatible reconnects while the stream stays retained.

## Rendition comparison

Comparison metrics add a `group` label: `group/<key>` compares alternatives within one HLS group (for example, video rungs or audio languages), while `combination/<index>` compares continuous audio/video tracks within a playable combination. Sparse subtitles and I-frame playlists are excluded.

| Metric | Meaning |
| --- | --- |
| `rushls_stream_rendition_media_skew_seconds` | Difference between the fastest and slowest contiguous `media_end_seconds` in the group |
| `rushls_rendition_behind_fastest_seconds` | How far one rendition trails the fastest sibling in its group |
| `rushls_stream_rendition_publish_spread_seconds` | Wall-clock spread between the earliest and latest sibling crossing the most recent common media boundary |
| `rushls_stream_rendition_publish_spread_timestamp_seconds` | Unix time when that boundary resolved |
| `rushls_stream_rendition_comparisons_pending` | Common boundaries still waiting on at least one sibling |
| `rushls_stream_rendition_comparison_age_seconds` | Age of the oldest unresolved comparison |
| `rushls_stream_rendition_comparisons_incomplete_total` | Comparisons discarded when history filled or the publication ended |

Comparisons align renditions on shared presentation time (not segment numbers or last-publish times).
Because a resolved spread cannot show a completely missing rendition, watch `comparisons_pending`, `comparison_age_seconds`, and per-rendition overdue metrics alongside it. Likewise, `media_skew_seconds` is absent while a publisher is inactive or before all siblings have started, and a zero skew does not guarantee progress if every rendition stalled together.

History holds up to 256 intervals per rendition and 256 unresolved boundaries per group; any evicted comparison increments `comparisons_incomplete_total`.
Node-wide histograms `rushls_output_publish_interval_seconds` and `rushls_output_publish_spread_seconds` track latency distributions across all streams without high-cardinality labels.

## Input, operations, and HTTP

### Input tracks

Validated tracks export source packets, source payload bytes (media payloads, excluding transport framing), normalized samples, normalized duration, and latest timestamps.
`rushls_track_source_started` and `rushls_track_normalized_started` distinguish a track that never produced data from one that went quiet.
Source timestamps mark packet intake into normalization (before pacing and packaging), not raw socket arrival.
`rushls_session_transport_loss_observable` is currently `0`; monitor transport retransmissions externally.

### Internal operations

Latency-sensitive origin operations export starts, in-flight counts, outcomes (`completed`, `ended`, `expired`, `error`, `cancelled`), and duration histograms labeled by `operation`:

| `operation` | Covers |
| --- | --- |
| `blocking_reload` | LL-HLS blocking playlist waits (`_HLS_msn` / `_HLS_part`), including immediate hits |
| `initial_readiness` | First playlist wait until usable media exists |
| `hinted_part` | Requests waiting for a preloaded part to finish |
| `playlist_projection` | Playlist rendering on a cache miss |
| `disk_read` | Reading spilled DVR media from disk (including admission wait) |
| `disk_write` | Writing spilled DVR media after dequeue |
| `store_backpressure` | Publisher waiting on store readiness |

Cancelled or dropped futures decrement in-flight counts and record `cancelled`. Disk spill queue pressure before `disk_write` is tracked by the spill pending/capacity metrics.

### HTTP serving

HTTP metrics classify paths into bounded `resource` values (`playlist`, `media`, `operator`, `other`), `method` (`GET`, `HEAD`, `other`), and HTTP `status`. Request URLs, tokens, viewer IPs, and headers never appear as labels.

| Metric | Meaning |
| --- | --- |
| `rushls_http_requests_started_total` | Requests entering admission (including refusals) |
| `rushls_http_responses_total` | Responses emitted, by `resource`, `method`, and `status` |
| `rushls_http_requests_finished_total` | Completed, cancelled, or errored request/body lifetimes |
| `rushls_http_requests_in_flight` | Active handlers and streaming response bodies |
| `rushls_http_body_bytes_total` | Encoded/ranged body bytes handed to the HTTP stack |
| `rushls_http_handler_duration_seconds` | Handler latency, including intentional LL-HLS blocking waits |
| `rushls_http_body_duration_seconds` | Body streaming lifetime until completion, cancellation, or error |
| `rushls_http_admission_refusals_total` | Requests or connections rejected at capacity |
| `rushls_http_connections` | Open admitted HTTP connections |
| `rushls_http_connection_capacity` | Configured `http.max_connections` |
| `rushls_http_request_capacity` | Configured `http.max_requests` |

Per-stream equivalents live on `/metrics/streams` under the `rushls_stream_http_` prefix with a `stream` label for recognized retained streams (requests for unknown streams stay in node totals so arbitrary URLs cannot inflate cardinality).
Counters survive compatible reconnects and disappear when the stream retires; use `rate()` or `increase()` across restarts or new broadcasts under the same name.

`rushls_http_failures_total` and `rushls_stream_http_failures_total` break down failures by bounded `reason`: `unknown_stream`, `unknown_rendition`, `unknown_resource` (either expired or never existed), `invalid_directive`, `unsatisfied`, `projection`, `unauthorized`, `forbidden`, `admission`, or `other`.

To inspect HTTP health for one stream:

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

Keep three caveats in mind when reading HTTP metrics:

- High `handler_duration_seconds` on playlists is normal under LL-HLS because blocking reloads wait for the next part; compare it against `body_duration_seconds` and output overdue metrics.
- `rushls_media_resolved_total`, `rushls_media_resolved_bytes_total`, `rushls_hls_playlists_resolved_total`, and `rushls_hls_playlist_projections_total` measure store lookups before HTTP encoding or range slicing; use `rushls_http_*` for actual status codes and transferred bytes.
- Even on a dedicated listener, `/metrics` shares `http.max_connections` and `http.max_requests` with viewer traffic. Alert on Prometheus `up == 0` so scrape failures under load never look like zeros.

Each stream HTTP histogram adds 17 series per active `resource` class (14 buckets, `+Inf`, `_sum`, `_count`). Keep your scrape interval shorter than `hls.window` if you want final counters before a stream retires.

## Capacity, memory, and lifecycle

Additional families cover session lifecycle, segmentation failures, recording drops, TLS handshakes, playback denials, hook delivery, and per-address limits (`rushls_publishers_address_limited_total`).
Detailed failure text stays in logs and hook events rather than metric labels; `rushls_session_info` maps each session to its `principal`.

### Publisher pipeline memory

- `rushls_session_pipeline_capacity_bytes`: configured `memory.per_publisher` (`0` when `"unlimited"`).
- `rushls_session_pipeline_used_bytes` and `rushls_session_pipeline_peak_bytes`: live and peak reservations.
- `rushls_session_pipeline_reserve_bytes`: headroom reserved strictly for packaging output.
- `rushls_session_pipeline_allocation_bytes{origin="transport|demux|normalization|mux|subtitle"}`: live usage attributed to where bytes were allocated (shared bytes keep their original owner across stages).
- `rushls_session_pipeline_exhaustions_total` (per-session) and `rushls_pipeline_exhaustions_total` (node-wide): failed reservations.
- `rushls_session_pacing_active`: whether `publish.rate.max` is actively throttling the publisher (separate from `store_backpressure`).

Pipeline memory metrics exclude native transport buffers, uninstrumented metadata, OS socket buffers, and allocator overhead.

### Retention and host context

Retention metrics cover both active and ended streams, broken down into payload, manifest, and disk bytes:

- `rushls_stream_retention_max_seconds`: longest advertised rendition duration.
- `rushls_stream_retention_min_seconds`: shortest current continuous-media rendition duration.
- `rushls_rendition_retention_seconds`: duration per rendition.

`rushls_session_capacity`, `rushls_disk_spill_capacity`, and `rushls_build_info` export node limits and build metadata.
Monitor host CPU, RSS, disk space, file descriptors, and network interfaces with your infrastructure exporter (such as `node_exporter`).

## Queries, dashboards, and alerts

Ready-to-import dashboards and starter rules live in [`examples/monitoring/`](../examples/monitoring/README.md):

- [`dashboard.json`](../examples/monitoring/dashboard.json): fleet and node overview.
- [`diagnostics.json`](../examples/monitoring/diagnostics.json): single-stream stall triage.
- [`alerts.yml`](../examples/monitoring/alerts.yml): starting alert thresholds to tune for your workload.

A 5-second scrape interval is a good starting point. Useful ad-hoc queries:

```promql
# Worst current deadline lateness in each stream.
max by (instance, stream) (rushls_rendition_output_overdue_seconds)

# Media seconds published per wall second, per rendition.
rate(rushls_rendition_media_published_seconds_total[1m])

# Time spent waiting on store backpressure.
rate(rushls_operation_duration_seconds_sum{operation="store_backpressure"}[1m])

# Viewer HTTP body throughput (excluding operator endpoints).
sum by (instance, resource) (
  rate(rushls_http_body_bytes_total{resource!="operator"}[1m])
)
```

Remember to alert on Prometheus `up == 0` for both `/metrics` and `/metrics/streams` scrape jobs.

## Public playback probe

[`tools/probe_playback.py`](../tools/probe_playback.py) performs one read-only fetch through a public HLS URL (for example, through your CDN), loading every active audio/video playlist and its latest published media resource:

```sh
python3 tools/probe_playback.py 'https://playback.example/live/stream.m3u8'
```

- Pass `--bearer-token-file` for JWT-protected streams. The header is sent only to the initial origin and stripped on cross-origin redirects; query tokens follow normal HLS URI resolution and `EXT-X-DEFINE` substitution. Output never prints URLs or tokens.
- The probe outputs Prometheus metrics covering success, total duration, playlist fetch time, media download time, bytes, and available media-end timestamps (note that its rendition labels are index-based, not Rushls's durable rendition IDs).
- It does not fetch preload hints or decode media, and enforces request timeout, size, and rendition-count bounds so broken responses fail loudly.

Run it on a timer and publish the output atomically to a Prometheus textfile collector, keeping failed runs visible and alerting on probe age so a stale success cannot mask a broken cron job. Note that `EXT-X-PROGRAM-DATE-TIME` reflects the origin timeline, not a verified camera clock.

## Timestamp rejections and gap compensation

When input validation rejects a stream or permissive mode repairs a hole (see [Input handling](input-handling.md)):

- `rushls_timestamp_rejections_total{code,media_kind}`: incremented once per failed session, using bounded issue codes (exact track IDs and timestamps appear in logs and `publisher.ended` hooks).
- `rushls_audio_repairs_total{codec,method}` and `rushls_audio_compensation_seconds_total{codec,method}`: count compensated audio holes and missing duration for `method="gap"` during normalization (including sessions that later fail downstream).
- `rushls_video_timestamp_step_seconds{codec}`: histogram of usable source video frame intervals (useful for spotting jitter; does not prove packet loss).
- `rushls_video_cadence_violations_total{codec}`: violations of explicitly declared fixed video cadence (including rejected streams).
- `rushls_video_compensation_seconds_total{codec,method}`: accepted excess duration when `method="gap"`.

## Optional allocator metrics

Binaries compiled with `--features allocation-counting` (omitted from standard release builds) expose three heap counters:

| Metric | Meaning |
| --- | --- |
| `rushls_allocator_calls_total` | Rust allocations and reallocations |
| `rushls_allocator_allocated_bytes_total` | Requested bytes (including full reallocation replacements) |
| `rushls_allocator_freed_bytes_total` | Released bytes (including replaced reallocations) |

`allocated - freed` approximates live Rust heap usage (excluding native allocations and allocator overhead; the three counters are not snapshotted atomically). See [Load testing](development/load-testing.md) for how they are used.
