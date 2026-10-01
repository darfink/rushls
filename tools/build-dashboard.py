#!/usr/bin/env python3
"""Generate the Rushls Grafana dashboard, examples/monitoring/dashboard.json.

Edit this file, not the JSON, then run:

    python3 tools/build-dashboard.py > examples/monitoring/dashboard.json

`tools/test_dashboard.py` fails when the two differ, or when a query names a
metric Rushls does not export.

Every query filters on `$job` and on `$node_label=~"$node"`, so the dashboard
works with plain Prometheus (`instance`) and with Kubernetes service discovery
(`pod`, or OpenTelemetry's `k8s_pod_name`). Panels that would otherwise draw one
line per stream show the worst case and the median instead: a busy origin sees
hundreds of short broadcasts a day, and a line each is unreadable. The live
streams table and the stream variable cover per-stream detail.
"""
import json
import sys

DS = {"type": "prometheus", "uid": "${datasource}"}
F = 'job=~"$job", $node_label=~"$node"'
FS = F + ', stream=~"$stream"'
RI = "$__rate_interval"

panels = []
next_id = [1]
y = [0]

def pid():
    next_id[0] += 1
    return next_id[0]

def target(expr, legend="", ref="A", instant=False, fmt=None):
    t = {"datasource": DS, "expr": expr, "legendFormat": legend, "refId": ref, "range": not instant, "instant": instant}
    if fmt:
        t["format"] = fmt
    return t

def row(title, collapsed=False):
    panels.append({"type": "row", "title": title, "id": pid(), "collapsed": collapsed,
                   "gridPos": {"h": 1, "w": 24, "x": 0, "y": y[0]}, "panels": []})
    y[0] += 1

def thresholds(*steps):
    out = [{"color": steps[0], "value": None}]
    for value, color in zip(steps[1::2], steps[2::2]):
        out.append({"color": color, "value": value})
    return {"mode": "absolute", "steps": out}

def stat(title, desc, targets, x, w, unit="short", th=None, decimals=None, color_mode="value", text_mode="auto", graph=True):
    defaults = {"unit": unit, "color": {"mode": "thresholds"},
                "thresholds": th or thresholds("text")}
    if decimals is not None:
        defaults["decimals"] = decimals
    panels.append({"type": "stat", "title": title, "description": desc, "id": pid(), "datasource": DS,
                   "gridPos": {"h": 4, "w": w, "x": x, "y": y[0]}, "targets": targets,
                   "fieldConfig": {"defaults": defaults, "overrides": []},
                   "options": {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
                               "colorMode": color_mode, "graphMode": "area" if graph else "none",
                               "textMode": text_mode, "justifyMode": "center", "orientation": "auto",
                               "wideLayout": True, "showPercentChange": False}})

def series(title, desc, targets, x, w, h=8, unit="short", stack=False, bars=False, th=None,
           min_=None, max_=None, overrides=None, legend_calcs=("mean", "max"), fill=10, decimals=None,
           threshold_style=None, no_value=None, soft_max=None):
    custom = {"drawStyle": "bars" if bars else "line", "lineWidth": 1 if bars else 2,
              "fillOpacity": 80 if bars else fill, "gradientMode": "opacity" if not bars else "none",
              "showPoints": "never", "spanNulls": False, "lineInterpolation": "smooth",
              "stacking": {"mode": "normal" if stack else "none", "group": "A"},
              "axisSoftMin": 0, "barAlignment": 0}
    if threshold_style:
        custom["thresholdsStyle"] = {"mode": threshold_style}
    defaults = {"unit": unit, "custom": custom, "color": {"mode": "palette-classic"},
                "thresholds": th or thresholds("green")}
    if min_ is not None: defaults["min"] = min_
    if max_ is not None: defaults["max"] = max_
    if decimals is not None: defaults["decimals"] = decimals
    if no_value is not None: defaults["noValue"] = no_value
    # A healthy fault panel is all zeros; without a soft maximum Grafana scales
    # that to 0-100, which reads as if something happened.
    if soft_max is not None: custom["axisSoftMax"] = soft_max
    # Bars of counter increases read as "events per bar"; a minimum interval
    # keeps them from thinning into slivers at long time ranges.
    extra = {"interval": "1m", "maxDataPoints": 120} if bars else {}
    panels.append({**extra, "type": "timeseries", "title": title, "description": desc, "id": pid(), "datasource": DS,
                   "gridPos": {"h": h, "w": w, "x": x, "y": y[0]}, "targets": targets,
                   "fieldConfig": {"defaults": defaults, "overrides": overrides or []},
                   "options": {"legend": {"displayMode": "table", "placement": "bottom", "showLegend": True,
                                          "calcs": list(legend_calcs), "sortBy": "Max", "sortDesc": True},
                               "tooltip": {"mode": "multi", "sort": "desc"}}})

def bargauge(title, desc, targets, x, w, h=8, unit="short"):
    panels.append({"type": "bargauge", "title": title, "description": desc, "id": pid(), "datasource": DS,
                   "gridPos": {"h": h, "w": w, "x": x, "y": y[0]}, "targets": targets,
                   "fieldConfig": {"defaults": {"unit": unit, "min": 0, "color": {"mode": "continuous-BlYlRd"},
                                                "thresholds": thresholds("green")}, "overrides": []},
                   "options": {"displayMode": "gradient", "orientation": "horizontal", "showUnfilled": True,
                               "valueMode": "color", "namePlacement": "left", "sizing": "auto",
                               "reduceOptions": {"calcs": ["mean"], "fields": "", "values": False},
                               "minVizHeight": 16, "maxVizHeight": 24}})

def state_timeline(title, desc, targets, x, w, h=6):
    panels.append({"type": "state-timeline", "title": title, "description": desc, "id": pid(), "datasource": DS,
                   "gridPos": {"h": h, "w": w, "x": x, "y": y[0]}, "targets": targets,
                   "fieldConfig": {"defaults": {"color": {"mode": "thresholds"},
                                                "thresholds": thresholds("transparent", 1, "green"),
                                                "mappings": [{"type": "value", "options": {
                                                    "0": {"text": "Idle", "color": "transparent", "index": 0},
                                                    "1": {"text": "Publishing", "color": "green", "index": 1}}}],
                                                "custom": {"fillOpacity": 80, "lineWidth": 0}},
                                   "overrides": []},
                   "options": {"showValue": "never", "mergeValues": True, "rowHeight": 0.8, "alignValue": "left",
                               "legend": {"showLegend": False}, "tooltip": {"mode": "single"}}})

def table(title, desc, targets, x, w, h, transformations, overrides=None):
    panels.append({"type": "table", "title": title, "description": desc, "id": pid(), "datasource": DS,
                   "gridPos": {"h": h, "w": w, "x": x, "y": y[0]}, "targets": targets,
                   "transformations": transformations,
                   "fieldConfig": {"defaults": {"custom": {"align": "auto", "cellOptions": {"type": "auto"}},
                                                "thresholds": thresholds("green")},
                                   "overrides": overrides or []},
                   "options": {"showHeader": True, "cellHeight": "sm", "footer": {"show": False}}})

def dashed(name, color="red"):
    """A capacity line: dashed and unfilled, so the series it bounds stays readable."""
    return {"matcher": {"id": "byName", "options": name}, "properties": [
        {"id": "custom.lineStyle", "value": {"fill": "dash", "dash": [10, 10]}},
        {"id": "custom.fillOpacity", "value": 0},
        {"id": "custom.stacking", "value": {"mode": "none", "group": "A"}},
        {"id": "color", "value": {"mode": "fixed", "fixedColor": color}}]}

def color(name, c):
    return {"matcher": {"id": "byName", "options": name},
            "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": c}}]}

def advance(h):
    y[0] += h

# ── Overview ────────────────────────────────────────────────────────────────
row("Overview")
stat("Live streams", "Streams with an active publisher.",
     [target(f"sum(rushls_published_streams{{{F}}})")], 0, 3, th=thresholds("blue"), decimals=0, graph=False)
stat("Viewer connections", "Open HTTP connections from players, CDNs, and scrapers.",
     [target(f"sum(rushls_http_connections{{{F}}})")], 3, 3, th=thresholds("blue"), decimals=0, graph=False)
stat("Ingest", "Media payload received from publishers, excluding transport overhead.",
     [target(f"sum(rate(rushls_source_payload_bytes_total{{{F}}}[{RI}])) * 8")], 6, 3, unit="bps", th=thresholds("purple"))
stat("Egress", "HTTP body bytes handed to the network stack for playlists, media, and operator paths.",
     [target(f"sum(rate(rushls_http_body_bytes_total{{{F}}}[{RI}])) * 8")], 9, 3, unit="bps", th=thresholds("purple"))
stat("Memory budget used", "Retained media in memory as a share of the memory budget. New streams are refused when it is full.",
     [target(f'sum(rushls_retained_memory_bytes{{{F}}}) / sum(rushls_retention_capacity_bytes{{{F}, tier="memory"}})')],
     12, 3, unit="percentunit", th=thresholds("green", 0.75, "orange", 0.9, "red"), decimals=1)
stat("Late renditions", "Renditions past their output deadline right now. Above zero, viewers are waiting for media that has not been published.",
     [target(f"count(rushls_rendition_output_overdue_seconds{{{F}}} > 0) or vector(0)")], 15, 3,
     th=thresholds("green", 1, "red"), decimals=0, graph=False)
stat("Session failures", "Publisher sessions that ended abnormally in the selected time range.",
     [target(f"sum(increase(rushls_sessions_failed_total{{{F}}}[$__range])) or vector(0)", instant=True)], 18, 3,
     th=thresholds("green", 1, "orange"), decimals=0, graph=False)
stat("HTTP 5xx", "Share of HTTP responses with a server error status.",
     [target(f'sum(rate(rushls_http_responses_total{{{F}, status=~"5.."}}[{RI}])) / sum(rate(rushls_http_responses_total{{{F}}}[{RI}])) or vector(0)')],
     21, 3, unit="percentunit", th=thresholds("green", 0.01, "orange", 0.05, "red"), decimals=2, graph=False)
advance(4)

# ── Ingest ──────────────────────────────────────────────────────────────────
row("Ingest")
series("Streams", "Streams with a publisher, streams kept for DVR after their publisher left, and connected publishers.",
       [target(f"sum(rushls_published_streams{{{F}}})", "live", "A"),
        target(f"sum(rushls_idle_streams{{{F}}})", "idle (retained)", "B"),
        target(f"sum(rushls_active_sessions{{{F}}})", "publishers", "C")],
       0, 8, legend_calcs=("max",), decimals=0,
       overrides=[color("live", "green"), color("idle (retained)", "blue"), color("publishers", "purple")])
series("Ingest bitrate", "Media payload received: the total, and the ten busiest streams. A stream at zero while its publisher is connected is a stalled encoder.",
       [target(f"sum(rate(rushls_source_payload_bytes_total{{{F}}}[{RI}])) * 8", "total", "A"),
        target(f"topk(10, sum by (stream) (rate(rushls_session_source_payload_bytes_total{{{FS}}}[{RI}])) * 8)", "{{stream}}", "B")],
       8, 8, unit="bps", fill=0, legend_calcs=("mean", "max"),
       overrides=[{"matcher": {"id": "byName", "options": "total"}, "properties": [
           {"id": "custom.fillOpacity", "value": 15}, {"id": "custom.lineWidth", "value": 1},
           {"id": "color", "value": {"mode": "fixed", "fixedColor": "purple"}}]}])
series("Publisher sessions", "Session lifecycle events per interval. Failures end a session abnormally; rejections are refused by admission, limits, or capacity.",
       [target(f"sum(increase(rushls_sessions_started_total{{{F}}}[{RI}]))", "started", "A"),
        target(f"sum(increase(rushls_sessions_completed_total{{{F}}}[{RI}]))", "completed", "B"),
        target(f"sum(increase(rushls_sessions_replaced_total{{{F}}}[{RI}]))", "replaced", "C"),
        target(f"sum(increase(rushls_sessions_failed_total{{{F}}}[{RI}]))", "failed", "D"),
        target(f"sum(increase(rushls_publishers_rejected_total{{{F}}}[{RI}]))", "rejected", "E"),
        target(f"sum(increase(rushls_publishers_address_limited_total{{{F}}}[{RI}]))", "address limited", "F")],
       16, 8, bars=True, stack=True, soft_max=4, legend_calcs=("sum",), decimals=0,
       overrides=[color("started", "blue"), color("completed", "green"), color("replaced", "yellow"),
                  color("failed", "red"), color("rejected", "orange"), color("address limited", "purple")])
advance(8)
# Every column is restricted to streams with a publisher, so ended streams
# kept for DVR do not appear as rows.
LIVE = f"and on (stream) (max by (stream) (rushls_stream_publisher_active{{{FS}}}) == 1)"
table("Live streams", "Streams with a publisher right now. Late is the longest any rendition has gone past its output deadline; lag is the worst rendition's drift behind real time.",
      [target(f"(sum by (stream) (rate(rushls_session_source_payload_bytes_total{{{FS}}}[1m])) * 8) {LIVE}", "", "A", instant=True, fmt="table"),
       target(f"count by (stream) (rushls_rendition_output_expected{{{FS}}} == 1) {LIVE}", "", "B", instant=True, fmt="table"),
       target(f"max by (stream) (rushls_rendition_output_overdue_seconds{{{FS}}}) {LIVE}", "", "C", instant=True, fmt="table"),
       target(f"max by (stream) (rushls_rendition_output_lag_seconds{{{FS}}}) {LIVE}", "", "D", instant=True, fmt="table"),
       target(f"min by (stream) (rushls_stream_retention_min_seconds{{{FS}}}) {LIVE}", "", "E", instant=True, fmt="table"),
       target(f"(sum by (stream) (rate(rushls_stream_http_body_bytes_total{{{FS}}}[1m])) * 8) {LIVE}", "", "F", instant=True, fmt="table")],
      0, 24, 8,
      [{"id": "merge", "options": {}},
       {"id": "organize", "options": {"excludeByName": {"Time": True},
                                      "indexByName": {"stream": 0, "Value #A": 1, "Value #B": 2, "Value #C": 3,
                                                      "Value #D": 4, "Value #E": 5, "Value #F": 6},
                                      "renameByName": {"stream": "Stream", "Value #A": "Ingest", "Value #B": "Renditions",
                                                       "Value #C": "Late", "Value #D": "Lag", "Value #E": "DVR window",
                                                       "Value #F": "Egress"}}},
       {"id": "sortBy", "options": {"sort": [{"field": "Egress", "desc": True}]}}],
      overrides=[{"matcher": {"id": "byName", "options": "Ingest"}, "properties": [{"id": "unit", "value": "bps"}]},
                 {"matcher": {"id": "byName", "options": "Egress"}, "properties": [
                     {"id": "unit", "value": "bps"},
                     {"id": "custom.cellOptions", "value": {"type": "gauge", "mode": "basic", "valueDisplayMode": "text"}},
                     {"id": "color", "value": {"mode": "continuous-BlPu"}}]},
                 {"matcher": {"id": "byName", "options": "Renditions"}, "properties": [{"id": "decimals", "value": 0}]},
                 {"matcher": {"id": "byName", "options": "Late"}, "properties": [
                     {"id": "unit", "value": "s"}, {"id": "decimals", "value": 1},
                     {"id": "thresholds", "value": thresholds("text", 0.001, "red")},
                     {"id": "custom.cellOptions", "value": {"type": "color-text"}}]},
                 {"matcher": {"id": "byName", "options": "Lag"}, "properties": [
                     {"id": "unit", "value": "s"}, {"id": "decimals", "value": 1},
                     {"id": "thresholds", "value": thresholds("green", 10, "orange", 30, "red")},
                     {"id": "custom.cellOptions", "value": {"type": "color-text"}}]},
                 {"matcher": {"id": "byName", "options": "DVR window"}, "properties": [{"id": "unit", "value": "s"}]}])
advance(8)

# ── Output ──────────────────────────────────────────────────────────────────
row("Output health")
# Worst case across the selected streams, so the panels stay readable with
# hundreds of streams a day. Pick a stream, or use the table above, to drill in.
EXPECTED = f"and on (stream, rendition) rushls_rendition_output_expected{{{FS}}} == 1"
series("Time past output deadline", "How long the slowest rendition has gone without publishing beyond its planned interval plus tolerance. Zero is healthy; anything above means viewers are waiting.",
       [target(f"max(rushls_rendition_output_overdue_seconds{{{FS}}})", "worst", "A")],
       0, 8, unit="s", th=thresholds("green", 1, "red"), threshold_style="dashed", legend_calcs=("max",),
       overrides=[color("worst", "red")], soft_max=5)
series("Output lag", "Wall-clock progress minus published media progress since each publication started. A few seconds either way is normal; steady growth means an encoder delivers slower than real time.",
       [target(f"max(rushls_rendition_output_lag_seconds{{{FS}}} {EXPECTED})", "worst", "A"),
        target(f"quantile(0.5, max by (stream) (rushls_rendition_output_lag_seconds{{{FS}}} {EXPECTED}))", "median stream", "B")],
       8, 8, unit="s", th=thresholds("green", 10, "orange", 30, "red"), threshold_style="dashed",
       legend_calcs=("mean", "max"), overrides=[color("worst", "orange"), color("median stream", "blue")])
series("Media seconds per second", "Media published per second of wall time while a publisher is connected. Steady at 1 is real time; below it, viewers drift behind.",
       [target(f"min(rate(rushls_rendition_media_published_seconds_total{{{FS}}}[{RI}]) {EXPECTED})", "slowest", "A"),
        target(f"quantile(0.5, min by (stream) (rate(rushls_rendition_media_published_seconds_total{{{FS}}}[{RI}]) {EXPECTED}))", "median stream", "B")],
       16, 8, unit="none", decimals=2, min_=0, max_=1.5, legend_calcs=("mean", "min"),
       th=thresholds("red", 0.9, "green"), threshold_style="dashed",
       overrides=[color("slowest", "orange"), color("median stream", "blue")])
advance(8)
series("Output faults", "Deadline misses, advertised gaps, and timeline breaks, summed over the selected streams.",
       [target(f"sum(increase(rushls_rendition_output_deadline_misses_total{{{FS}}}[{RI}]))", "deadline misses", "A"),
        target(f"sum(increase(rushls_rendition_gaps_total{{{FS}}}[{RI}]))", "gaps", "B"),
        target(f"sum(increase(rushls_rendition_timeline_breaks_total{{{FS}}}[{RI}]))", "timeline breaks", "C")],
       0, 12, bars=True, stack=True, soft_max=4, legend_calcs=("sum",), decimals=0,
       overrides=[color("deadline misses", "red"), color("gaps", "orange"), color("timeline breaks", "purple")])
series("Rendition skew", "Largest media-time difference between renditions that should advance together, against the part duration. Skew beyond one part makes rendition switches wait.",
       [target(f"max(rushls_stream_rendition_media_skew_seconds{{{FS}}})", "worst", "A"),
        target(f"max(rushls_rendition_part_target_seconds{{{FS}}})", "part duration", "B")],
       12, 12, unit="s", legend_calcs=("mean", "max"),
       overrides=[color("worst", "purple"), dashed("part duration", "orange")])
advance(8)

# ── Viewers ─────────────────────────────────────────────────────────────────
row("Viewers")
series("Egress by resource", "HTTP body throughput by resource class.",
       [target(f"sum by (resource) (rate(rushls_http_body_bytes_total{{{F}}}[{RI}])) * 8", "{{resource}}")],
       0, 8, unit="bps", stack=True, fill=25)
series("Responses by status", "HTTP responses per second by status code.",
       [target(f"sum by (status) (rate(rushls_http_responses_total{{{F}}}[{RI}]))", "{{status}}")],
       8, 8, unit="reqps", stack=True, fill=25,
       overrides=[{"matcher": {"id": "byRegexp", "options": "^2.."}, "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": "green"}}]},
                  {"matcher": {"id": "byRegexp", "options": "^3.."}, "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": "blue"}}]},
                  {"matcher": {"id": "byRegexp", "options": "^4.."}, "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": "orange"}}]},
                  {"matcher": {"id": "byRegexp", "options": "^5.."}, "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": "red"}}]}])
series("Low-latency waits", "How long LL-HLS requests wait for media that is not published yet: blocking playlist reloads and preloaded parts (p95). Waiting about one part is the protocol working; much longer means output is slow.",
       [target(f'histogram_quantile(0.95, sum by (le, operation) (rate(rushls_operation_duration_seconds_bucket{{{F}, operation=~"blocking_reload|hinted_part"}}[{RI}])))', "{{operation}}", "A"),
        target(f"max(rushls_rendition_part_target_seconds{{{F}}})", "part duration", "B")],
       16, 8, unit="s", legend_calcs=("mean", "max"),
       overrides=[color("blocking_reload", "blue"), color("hinted_part", "purple"), dashed("part duration", "orange"),
                  {"matcher": {"id": "byName", "options": "blocking_reload"}, "properties": [{"id": "displayName", "value": "playlist reload"}]},
                  {"matcher": {"id": "byName", "options": "hinted_part"}, "properties": [{"id": "displayName", "value": "preloaded part"}]}])
advance(8)
bargauge("Egress by stream", "Viewer egress per stream, averaged over the selected time range.",
         [target(f"topk(10, sum by (stream) (rate(rushls_stream_http_body_bytes_total{{{FS}}}[{RI}])) * 8)", "{{stream}}")],
         0, 8, unit="bps")
series("Failures by reason", "Requests that could not be served, by reason. `unknown_stream` and `unknown_resource` are usually players polling after a broadcast ended.",
       [target(f"sum by (reason) (rate(rushls_http_failures_total{{{F}}}[{RI}]))", "{{reason}}")],
       8, 8, unit="reqps", stack=True, fill=25)
series("HTTP capacity used", "Open connections and requests in flight as a share of their budgets (`http.max_connections`, `http.max_requests`). New ones are refused at 100%; refusals are the bars, on the right axis.",
       [target(f"sum(rushls_http_connections{{{F}}}) / sum(rushls_http_connection_capacity{{{F}}})", "connections", "A"),
        target(f"sum(rushls_http_requests_in_flight{{{F}}}) / sum(rushls_http_request_capacity{{{F}}})", "requests", "B"),
        target(f"sum(increase(rushls_http_admission_refusals_total{{{F}}}[{RI}]))", "refused", "C")],
       16, 8, unit="percentunit", legend_calcs=("max",), th=thresholds("green", 0.8, "red"), threshold_style="dashed",
       overrides=[color("connections", "blue"), color("requests", "purple"),
                  {"matcher": {"id": "byName", "options": "refused"}, "properties": [
                      {"id": "custom.drawStyle", "value": "bars"}, {"id": "custom.fillOpacity", "value": 80},
                      {"id": "unit", "value": "short"}, {"id": "decimals", "value": 0},
                      {"id": "custom.axisPlacement", "value": "right"},
                      {"id": "color", "value": {"mode": "fixed", "fixedColor": "red"}}]}])
advance(8)

# ── Memory and storage ──────────────────────────────────────────────────────
row("Retention and storage")
series("Retained media", "Live-window and DVR media held in memory and spilled to disk. Memory budget use is in the overview.",
       [target(f'sum(rushls_retained_memory_bytes{{{F}}})', "memory", "A"),
        # Absent rather than a line at zero when the disk tier is off.
        target(f'sum(rushls_retained_disk_bytes{{{F}}}) and sum(rushls_retention_capacity_bytes{{{F}, tier="disk"}}) > 0', "disk", "B")],
       0, 8, unit="bytes", stack=True, fill=25, legend_calcs=("max",),
       overrides=[color("memory", "blue"), color("disk", "purple")])
series("DVR window", "How far back viewers can seek: the shortest and longest current window across live streams.",
       [target(f"min(rushls_stream_retention_min_seconds{{{FS}}} and on (stream) rushls_stream_publisher_active{{{FS}}} == 1)", "shortest", "A"),
        target(f"max(rushls_stream_retention_max_seconds{{{FS}}} and on (stream) rushls_stream_publisher_active{{{FS}}} == 1)", "longest", "B")],
       8, 8, unit="s", legend_calcs=("min", "max"), overrides=[color("shortest", "orange"), color("longest", "blue")])
series("Publisher pipeline memory", "Accounted ingest buffers across publishers: the total, and the largest single publisher. Exhaustion ends a session.",
       [target(f"sum(rushls_session_pipeline_used_bytes{{{FS}}})", "total", "A"),
        target(f"max(rushls_session_pipeline_used_bytes{{{FS}}})", "largest publisher", "B")],
       16, 8, unit="bytes", legend_calcs=("max",), overrides=[color("total", "blue"), color("largest publisher", "orange")],
       no_value="No publishers, or a build before 0.1.0")
advance(8)
series("Storage faults", "Failed disk spills, lost recording segments, and exhausted publisher pipelines. Each one loses media or a session. The line is disk writes waiting, on the right axis.",
       [target(f"sum(increase(rushls_disk_spills_failed_total{{{F}}}[{RI}]))", "disk spills failed", "A"),
        target(f"sum(increase(rushls_recording_segments_lost_total{{{F}}}[{RI}]))", "recording segments lost", "B"),
        target(f"sum(increase(rushls_pipeline_exhaustions_total{{{F}}}[{RI}]))", "pipeline exhaustions", "C"),
        target(f"sum(rushls_disk_spill_pending{{{F}}})", "disk writes waiting", "D")],
       0, 24, h=7, bars=True, stack=True, soft_max=4, legend_calcs=("sum", "max"), decimals=0,
       overrides=[color("disk spills failed", "red"), color("recording segments lost", "orange"),
                  color("pipeline exhaustions", "purple"),
                  {"matcher": {"id": "byName", "options": "disk writes waiting"}, "properties": [
                      {"id": "custom.drawStyle", "value": "line"}, {"id": "custom.fillOpacity", "value": 0},
                      {"id": "custom.stacking", "value": {"mode": "none", "group": "A"}},
                      {"id": "custom.axisPlacement", "value": "right"},
                      {"id": "color", "value": {"mode": "fixed", "fixedColor": "blue"}}]}])
advance(7)

# ── Hooks ───────────────────────────────────────────────────────────────────
row("Lifecycle hooks")
series("Hook deliveries", "Delivered events and retries per destination.",
       [target(f"sum by (hook) (increase(rushls_hook_deliveries_total{{{F}}}[{RI}]))", "{{hook}} delivered", "A"),
        target(f"sum by (hook) (increase(rushls_hook_retries_total{{{F}}}[{RI}]))", "{{hook}} retried", "B")],
       0, 8, bars=True, legend_calcs=("sum",), decimals=0)
series("Hook events dropped", "Events that never reached a destination, by reason. Delivery is best-effort, so drops are reported rather than retried forever.",
       [target(f"sum(increase(rushls_hook_dropped_{reason}_total{{{F}}}[{RI}]))", label, chr(ord("A") + i))
        for i, (reason, label) in enumerate([("overflow", "queue full"), ("exhausted", "out of attempts"),
                                             ("rejected", "rejected by endpoint"), ("ingress", "ingress full"),
                                             ("shutdown", "shutdown")])],
       8, 8, bars=True, stack=True, soft_max=4, legend_calcs=("sum",), decimals=0)
series("Hook queue", "Events waiting per destination, and requests in flight. Each destination holds 1,000 events before the oldest is dropped.",
       [target(f"sum by (hook) (rushls_hook_queue_depth{{{F}}})", "{{hook}} queued", "A"),
        target(f"sum by (hook) (rushls_hook_in_flight{{{F}}})", "{{hook}} in flight", "B")],
       16, 8, soft_max=4, legend_calcs=("max",), decimals=0)
advance(8)

# ── Nodes ───────────────────────────────────────────────────────────────────
row("Nodes")
table("Nodes", "One row per origin. A node missing here is not being scraped.",
      [target(f"sum by ($node_label) (rushls_active_sessions{{{F}}})", "", "A", instant=True, fmt="table"),
       target(f"sum by ($node_label) (rushls_published_streams{{{F}}})", "", "B", instant=True, fmt="table"),
       target(f"sum by ($node_label) (rushls_http_connections{{{F}}})", "", "C", instant=True, fmt="table"),
       target(f"sum by ($node_label) (rate(rushls_source_payload_bytes_total{{{F}}}[5m])) * 8", "", "D", instant=True, fmt="table"),
       target(f"sum by ($node_label) (rate(rushls_http_body_bytes_total{{{F}}}[5m])) * 8", "", "E", instant=True, fmt="table"),
       target(f'sum by ($node_label) (rushls_retained_memory_bytes{{{F}}}) / sum by ($node_label) (rushls_retention_capacity_bytes{{{F}, tier="memory"}})', "", "F", instant=True, fmt="table"),
       target(f"max by ($node_label, version) (rushls_build_info{{{F}}})", "", "G", instant=True, fmt="table")],
      0, 24, 5,
      # `merge` joins on whichever label column the queries share, so it works
      # for any `$node_label`; a by-field join would need the field's name.
      [{"id": "merge", "options": {}},
       {"id": "organize", "options": {"excludeByName": {"Time": True, "Value #G": True},
                                      "renameByName": {"instance": "Node", "pod": "Node", "k8s_pod_name": "Node",
                                                       "host": "Node", "version": "Version", "Value #A": "Publishers",
                                                       "Value #B": "Live streams", "Value #C": "Connections",
                                                       "Value #D": "Ingest", "Value #E": "Egress",
                                                       "Value #F": "Memory budget used"}}}],
      overrides=[{"matcher": {"id": "byName", "options": "Ingest"}, "properties": [{"id": "unit", "value": "bps"}]},
                 {"matcher": {"id": "byName", "options": "Egress"}, "properties": [{"id": "unit", "value": "bps"}]},
                 {"matcher": {"id": "byName", "options": "Memory budget used"}, "properties": [
                     {"id": "unit", "value": "percentunit"}, {"id": "min", "value": 0}, {"id": "max", "value": 1},
                     {"id": "thresholds", "value": thresholds("green", 0.75, "orange", 0.9, "red")},
                     {"id": "custom.cellOptions", "value": {"type": "gauge", "mode": "lcd", "valueDisplayMode": "text"}}]}])
advance(5)


templating = [
    {"name": "datasource", "label": "Data source", "type": "datasource", "query": "prometheus",
     "current": {}, "hide": 0, "refresh": 1, "regex": "", "includeAll": False, "multi": False},
    {"name": "node_label", "label": "Node label", "type": "custom",
     "description": "The label that tells origins apart: `instance` for static targets, `pod` for Kubernetes service discovery, `k8s_pod_name` for OpenTelemetry.",
     "query": "instance,pod,k8s_pod_name,host", "current": {"text": "instance", "value": "instance"},
     "options": [{"text": v, "value": v, "selected": v == "instance"} for v in ["instance", "pod", "k8s_pod_name", "host"]],
     "hide": 0, "includeAll": False, "multi": False},
    {"name": "job", "label": "Job", "type": "query", "datasource": DS,
     "description": "Scrape jobs. Scraping /metrics and /metrics/streams as two jobs is common; keep both selected.",
     "query": {"query": 'label_values({__name__=~"rushls_build_info|rushls_stream_publisher_active"}, job)', "refId": "job"},
     "definition": 'label_values({__name__=~"rushls_build_info|rushls_stream_publisher_active"}, job)', "refresh": 2, "sort": 1,
     "includeAll": True, "allValue": ".*", "multi": True, "current": {"text": "All", "value": "$__all"}, "hide": 0},
    {"name": "node", "label": "Node", "type": "query", "datasource": DS,
     "query": {"query": 'label_values(rushls_build_info{job=~"$job"}, $node_label)', "refId": "node"},
     "definition": 'label_values(rushls_build_info{job=~"$job"}, $node_label)', "refresh": 2, "sort": 1,
     "includeAll": True, "allValue": ".*", "multi": True, "current": {"text": "All", "value": "$__all"}, "hide": 0},
    {"name": "stream", "label": "Stream", "type": "query", "datasource": DS,
     "query": {"query": 'label_values(rushls_stream_publisher_active{job=~"$job"}, stream)', "refId": "stream"},
     "definition": 'label_values(rushls_stream_publisher_active{job=~"$job"}, stream)', "refresh": 2, "sort": 1,
     "includeAll": True, "allValue": ".*", "multi": True, "current": {"text": "All", "value": "$__all"}, "hide": 0},
]

dashboard = {
    "title": "Rushls",
    "uid": "rushls-overview",
    "description": "Ingest, output health, viewer delivery, retention, and hooks for Rushls origins. Scrape both /metrics and /metrics/streams.",
    "tags": ["rushls", "hls", "streaming"],
    "timezone": "browser", "editable": True, "graphTooltip": 1, "fiscalYearStartMonth": 0,
    "liveNow": False, "refresh": "30s", "schemaVersion": 39, "version": 1,
    "time": {"from": "now-6h", "to": "now"},
    "timepicker": {},
    "links": [{"title": "Metrics reference", "type": "link", "icon": "doc", "targetBlank": True,
               "url": "https://github.com/darfink/rushls/blob/main/docs/metrics.md"}],
    "annotations": {"list": [{"builtIn": 1, "datasource": {"type": "grafana", "uid": "-- Grafana --"},
                              "enable": True, "hide": True, "iconColor": "rgba(0, 211, 255, 1)",
                              "name": "Annotations & Alerts", "type": "dashboard"}]},
    "templating": {"list": templating},
    "panels": panels,
}


def render():
    return json.dumps(dashboard, indent=2, ensure_ascii=False) + "\n"


if __name__ == "__main__":
    sys.stdout.write(render())
