#!/usr/bin/env bash

# Sustained local ingest/load test for Rushls. The publishers generate clean,
# monotonic H.264/AAC media directly, so the run does not depend on a fixture
# file or on a file-loop timestamp discontinuity.

set -Eeuo pipefail

publisher_count=5
duration_seconds=3600
sample_interval=30
post_stop_interval=5
retention_wait_seconds=40
viewer_count=2
viewer_interval=2
slow_viewer_count=1
metrics_port=18081
memory_per_stream="16MiB"
disk_per_stream="64MiB"
dvr_dir=""
rtmp_port=11935
srt_port=19000
http_port=18080
build_requested=true
release_build=false
output_dir=""
server_pid=""
publisher_pids=()
viewer_pids=()
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
cd "$repo_root"

usage() {
  cat <<'EOF'
Usage: tools/soak.sh [options]

Runs concurrent synthetic RTMP publishers plus playlist viewers against a
local Rushls process, records RSS/CPU, playlist latency, and Rushls metrics,
then observes cleanup after publishers stop.
Runs concurrent synthetic RTMP publishers plus playlist viewers against a
local Rushls process, records RSS/CPU, playlist latency, and Rushls metrics,
then observes cleanup after publishers stop.

Options:
  --publishers N          Number of concurrent publishers (default: 5)
  --duration DURATION     Soak duration, such as 30s, 15m, or 2h (default: 1h)
  --sample-interval N     Seconds between steady-state samples (default: 30)
  --post-stop-interval N  Seconds between teardown samples (default: 5)
  --retention-wait N      Seconds to observe after publishers stop (default: 40)
  --viewers N             Playlist viewers polling index.m3u8 (default: 2, 0 disables)
  --slow-viewers N        Extra viewers throttled to 32kB/s (default: 1, 0 disables)
  --viewer-interval N     Seconds between playlist polls per viewer (default: 2)
  --rtmp-port N           Local RTMP port (default: 11935)
  --srt-port N            Local SRT port (default: 19000)
  --http-port N           Local HTTP port (default: 18080)
  --metrics-port N        Local metrics port (default: 18081)
  --memory-per-stream S   Retained-media cap per stream (default: 16MiB)
  --disk-per-stream S     DVR spill cap per stream (default: 64MiB)
  --dvr-dir PATH          Spill directory (default: <output-dir>/dvr)
  --output-dir PATH       Directory for CSV, logs, and publisher output
  --no-build              Use the existing target/debug binary
  --release               Build and use target/release/rushls
  -h, --help              Show this help

The default run is one hour with five publishers, two playlist viewers, and one
slow viewer. Use --release for a more
representative memory measurement, and set SOAK_OUTPUT_DIR or --output-dir to
keep results in a known location.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

parse_duration() {
  local value="$1"
  if [[ "$value" =~ ^[0-9]+$ ]]; then
    printf '%s\n' "$value"
  elif [[ "$value" =~ ^([0-9]+)(s|m|h)$ ]]; then
    local amount="${BASH_REMATCH[1]}"
    case "${BASH_REMATCH[2]}" in
      s) printf '%s\n' "$amount" ;;
      m) printf '%s\n' "$((amount * 60))" ;;
      h) printf '%s\n' "$((amount * 3600))" ;;
    esac
  else
    die "invalid duration '$value'; use seconds or a value like 30s, 15m, or 2h"
  fi
}

require_positive_integer() {
  local name="$1"
  local value="$2"
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || die "$name must be a positive integer"
}

require_non_negative_integer() {
  local name="$1"
  local value="$2"
  [[ "$value" =~ ^[0-9]+$ ]] || die "$name must be a non-negative integer"
}

while (($# > 0)); do
  case "$1" in
    --publishers)
      (($# >= 2)) || die "--publishers requires a value"
      publisher_count="$2"
      shift 2
      ;;
    --duration)
      (($# >= 2)) || die "--duration requires a value"
      duration_seconds="$(parse_duration "$2")"
      shift 2
      ;;
    --sample-interval)
      (($# >= 2)) || die "--sample-interval requires a value"
      sample_interval="$2"
      shift 2
      ;;
    --post-stop-interval)
      (($# >= 2)) || die "--post-stop-interval requires a value"
      post_stop_interval="$2"
      shift 2
      ;;
    --retention-wait)
      (($# >= 2)) || die "--retention-wait requires a value"
      retention_wait_seconds="$(parse_duration "$2")"
      shift 2
      ;;
    --viewers)
      (($# >= 2)) || die "--viewers requires a value"
      viewer_count="$2"
      shift 2
      ;;
    --slow-viewers)
      (($# >= 2)) || die "--slow-viewers requires a value"
      slow_viewer_count="$2"
      shift 2
      ;;
    --viewer-interval)
      (($# >= 2)) || die "--viewer-interval requires a value"
      viewer_interval="$2"
      shift 2
      ;;
    --rtmp-port)
      (($# >= 2)) || die "--rtmp-port requires a value"
      rtmp_port="$2"
      shift 2
      ;;
    --srt-port)
      (($# >= 2)) || die "--srt-port requires a value"
      srt_port="$2"
      shift 2
      ;;
    --http-port)
      (($# >= 2)) || die "--http-port requires a value"
      http_port="$2"
      shift 2
      ;;
    --metrics-port)
      (($# >= 2)) || die "--metrics-port requires a value"
      metrics_port="$2"
      shift 2
      ;;
    --memory-per-stream)
      (($# >= 2)) || die "--memory-per-stream requires a value"
      memory_per_stream="$2"
      shift 2
      ;;
    --disk-per-stream)
      (($# >= 2)) || die "--disk-per-stream requires a value"
      disk_per_stream="$2"
      shift 2
      ;;
    --dvr-dir)
      (($# >= 2)) || die "--dvr-dir requires a value"
      dvr_dir="$2"
      shift 2
      ;;
    --output-dir)
      (($# >= 2)) || die "--output-dir requires a value"
      output_dir="$2"
      shift 2
      ;;
    --no-build)
      build_requested=false
      shift
      ;;
    --release)
      release_build=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option '$1'; use --help for usage"
      ;;
  esac
done

require_positive_integer publishers "$publisher_count"
require_positive_integer duration "$duration_seconds"
require_positive_integer sample-interval "$sample_interval"
require_positive_integer post-stop-interval "$post_stop_interval"
require_positive_integer retention-wait "$retention_wait_seconds"
require_non_negative_integer viewers "$viewer_count"
require_non_negative_integer slow-viewers "$slow_viewer_count"
require_positive_integer viewer-interval "$viewer_interval"
require_positive_integer metrics-port "$metrics_port"
require_positive_integer rtmp-port "$rtmp_port"
require_positive_integer srt-port "$srt_port"
require_positive_integer http-port "$http_port"

if [[ -z "$output_dir" ]]; then
  output_dir="${SOAK_OUTPUT_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/rushls-soak.XXXXXX")}"
fi
mkdir -p "$output_dir"

if [[ "$release_build" == true ]]; then
  rushls_binary="${RUSHLS_BIN:-target/release/rushls}"
else
  rushls_binary="${RUSHLS_BIN:-target/debug/rushls}"
fi
ffmpeg_binary="${FFMPEG_BIN:-ffmpeg}"

for command_name in curl awk ps; do
  command -v "$command_name" >/dev/null 2>&1 || die "required command not found: $command_name"
done
command -v "$ffmpeg_binary" >/dev/null 2>&1 || die "required command not found: $ffmpeg_binary"

if [[ "$build_requested" == true ]]; then
  if [[ "$release_build" == true ]]; then
    echo "building Rushls (release)"
    cargo build --release --bin rushls
  else
    echo "building Rushls (debug)"
    cargo build --bin rushls
  fi
fi
[[ -x "$rushls_binary" ]] || die "Rushls binary not found: $rushls_binary"

csv_path="$output_dir/samples.csv"
server_log="$output_dir/rushls.log"
publisher_log_prefix="$output_dir/publisher"
printf 'phase,timestamp_utc,elapsed_seconds,rss_kib,physical_footprint,active_sessions,published_streams,idle_streams,ready_playlists,failed_sessions,live_publishers\n' >"$csv_path"
printf 'phase,timestamp_utc,elapsed_seconds,rss_kib,physical_footprint,active_sessions,published_streams,idle_streams,ready_playlists,failed_sessions,live_publishers,cpu_percent,playlist_latency_max_s,live_viewers,retained_memory_bytes,retained_disk_bytes,disk_spill_pending,spills_failed_total,playlists_served_total\n' >"$csv_path"

cleanup() {
  local exit_status=$?
  trap - EXIT INT TERM
  set +e

  for publisher_pid in "${publisher_pids[@]}"; do
    kill "$publisher_pid" 2>/dev/null
  done
  for publisher_pid in "${publisher_pids[@]}"; do
    wait "$publisher_pid" 2>/dev/null
  done
  for viewer_pid in "${viewer_pids[@]}"; do
    kill "$viewer_pid" 2>/dev/null
  done
  for viewer_pid in "${viewer_pids[@]}"; do
    wait "$viewer_pid" 2>/dev/null
  done

  if [[ -n "$server_pid" ]]; then
    kill -INT "$server_pid" 2>/dev/null
    wait "$server_pid" 2>/dev/null
  fi

  if ((exit_status != 0)); then
    echo "soak failed; artifacts: $output_dir" >&2
  else
    echo "soak complete; artifacts: $output_dir"
  fi
  exit "$exit_status"
}

trap 'exit 130' INT TERM
trap cleanup EXIT

echo "starting Rushls on RTMP :$rtmp_port, SRT :$srt_port, HTTP :$http_port, metrics :$metrics_port"
if [[ -z "$dvr_dir" ]]; then
  dvr_dir="$output_dir/dvr"
fi
mkdir -p "$dvr_dir"
RUSHLS_RTMP_LISTEN="127.0.0.1:$rtmp_port" \
RUSHLS_SRT_LISTEN="127.0.0.1:$srt_port" \
RUSHLS_HTTP_LISTEN="127.0.0.1:$http_port" \
RUSHLS_METRICS_LISTEN="127.0.0.1:$metrics_port" \
RUSHLS_CAPACITY_PUBLISHERS="${RUSHLS_CAPACITY_PUBLISHERS:-$((publisher_count + 3))}" \
RUSHLS_CAPACITY_STREAMS="${RUSHLS_CAPACITY_STREAMS:-$((publisher_count + 3))}" \
RUSHLS_CAPACITY_MEMORY_PER_STREAM="$memory_per_stream" \
RUSHLS_CAPACITY_DISK_PER_STREAM="$disk_per_stream" \
RUSHLS_CAPACITY_DIR="$dvr_dir" \
  "$rushls_binary" >"$server_log" 2>&1 &
server_pid=$!

ready_url="http://127.0.0.1:$http_port/health/ready"
metrics_url="http://127.0.0.1:$metrics_port/metrics"
for attempt in {1..30}; do
  if curl -fsS --max-time 2 -o /dev/null "$ready_url" 2>/dev/null; then
    break
  fi
  if ((attempt == 30)); then
    tail -n 80 "$server_log" >&2
    die "Rushls did not become ready"
  fi
  sleep 1
done

echo "starting $publisher_count synthetic publishers"
for ((index = 1; index <= publisher_count; index++)); do
  log_path="${publisher_log_prefix}-${index}.log"
  "$ffmpeg_binary" -hide_banner -loglevel error \
    -re -f lavfi -i 'testsrc2=size=320x180:rate=30' \
    -re -f lavfi -i 'sine=frequency=1000:sample_rate=48000' \
    -map 0:v:0 -map 1:a:0 \
    -c:v libx264 -preset ultrafast -tune zerolatency \
    -pix_fmt yuv420p -profile:v baseline -level:v 3.0 \
    -g 60 -keyint_min 60 -sc_threshold 0 -bf 0 -b:v 300k \
    -c:a aac -b:a 64k -f flv \
    "rtmp://127.0.0.1:$rtmp_port/live/soak-$index" >"$log_path" 2>&1 &
  publisher_pids+=("$!")
done

echo "starting $viewer_count playlist viewers and $slow_viewer_count slow viewers"
for ((v = 1; v <= viewer_count; v++)); do
  (
  while :; do
    curl -fsS --max-time 5 -o /dev/null \
      "http://127.0.0.1:$http_port/live/soak-$((v % publisher_count + 1))/index.m3u8" 2>/dev/null || true
    sleep "$viewer_interval"
  done
  ) >"$output_dir/viewer-$v.log" 2>&1 &
  viewer_pids+=("$!")
done
for ((v = 1; v <= slow_viewer_count; v++)); do
  (
  while :; do
    curl -fsS --max-time 20 --limit-rate 32k -o /dev/null \
      "http://127.0.0.1:$http_port/live/soak-$((v % publisher_count + 1))/index.m3u8" 2>/dev/null || true
    sleep "$viewer_interval"
  done
  ) >"$output_dir/slow-viewer-$v.log" 2>&1 &
  viewer_pids+=("$!")
done

metric_value() {
  local metrics="$1"
  local metric_name="$2"
  awk -v name="$metric_name" '$1 == name { print $2; exit }' <<<"$metrics"
}

ready_playlists() {
  local ready=0
  for ((index = 1; index <= publisher_count; index++)); do
    if curl -fsS --max-time 2 -o /dev/null \
      "http://127.0.0.1:$http_port/live/soak-$index/index.m3u8" 2>/dev/null; then
      ((ready += 1))
    fi
  done
  printf '%s\n' "$ready"
}

live_publishers() {
  local live=0
  local publisher_state
  for publisher_pid in "${publisher_pids[@]}"; do
    publisher_state="$(ps -o stat= -p "$publisher_pid" 2>/dev/null)"
    if [[ -n "$publisher_state" && "$publisher_state" != Z* ]]; then
      ((live += 1))
    fi
  done
  printf '%s\n' "$live"
}

live_viewers() {
  local live=0
  local viewer_state
  for viewer_pid in "${viewer_pids[@]}"; do
    viewer_state="$(ps -o stat= -p "$viewer_pid" 2>/dev/null)"
    if [[ -n "$viewer_state" && "$viewer_state" != Z* ]]; then
      ((live += 1))
    fi
  done
  printf '%s\n' "$live"
}

playlist_latency_max() {
  local max=0
  local t
  for ((index = 1; index <= publisher_count; index++)); do
    t="$(curl -fsS --max-time 5 -o /dev/null -w '%{time_total}' \
      "http://127.0.0.1:$http_port/live/soak-$index/index.m3u8" 2>/dev/null)" || continue
    max="$(awk -v a="$max" -v b="$t" 'BEGIN { if (b+0 > a+0) print b; else print a }')"
  done
  printf '%s\n' "$max"
}

sample() {
  local phase="$1"
  local elapsed="$2"
  local metrics
  local rss
  local physical_footprint=""
  local active
  local published
  local idle
  local ready
  local failed
  local live
  local cpu
  local latency
  local viewers
  local retained_mem
  local retained_disk
  local spill_pending
  local spills_failed
  local playlists_served

  metrics="$(curl -fsS --max-time 2 "$metrics_url")"
  rss="$(ps -o rss= -p "$server_pid" | tr -d ' ')"
  active="$(metric_value "$metrics" rushls_active_sessions)"
  published="$(metric_value "$metrics" rushls_published_streams)"
  idle="$(metric_value "$metrics" rushls_idle_streams)"
  failed="$(metric_value "$metrics" rushls_sessions_failed_total)"
  ready="$(ready_playlists)"
  live="$(live_publishers)"
  cpu="$(ps -o %cpu= -p "$server_pid" | tr -d ' ')"
  latency="$(playlist_latency_max)"
  viewers="$(live_viewers)"
  retained_mem="$(metric_value "$metrics" rushls_retained_memory_bytes)"
  retained_disk="$(metric_value "$metrics" rushls_retained_disk_bytes)"
  spill_pending="$(metric_value "$metrics" rushls_disk_spill_pending)"
  spills_failed="$(metric_value "$metrics" rushls_disk_spills_failed_total)"
  playlists_served="$(metric_value "$metrics" rushls_hls_playlists_resolved_total)"

  if [[ "$phase" == steady-state && "$live" != "$publisher_count" ]]; then
    echo "a publisher exited during the soak (live=$live expected=$publisher_count)" >&2
    return 1
  fi
  if [[ "$phase" == steady-state && "$failed" != 0 ]]; then
    echo "Rushls reported failed sessions during the soak: $failed" >&2
    return 1
  fi

  if command -v vmmap >/dev/null 2>&1; then
    physical_footprint="$(vmmap -summary "$server_pid" 2>/dev/null | awk '/^Physical footprint:/ { print $3; exit }')"
  fi

  printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
    "$phase" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$elapsed" "$rss" "$physical_footprint" \
    "$active" "$published" "$idle" "$ready" "$failed" "$live" \
    "$cpu" "$latency" "$viewers" "$retained_mem" "$retained_disk" "$spill_pending" "$spills_failed" "$playlists_served" | tee -a "$csv_path"
}

echo "waiting for all publishers and HLS playlists"
for attempt in {1..60}; do
  metrics="$(curl -fsS --max-time 2 "$metrics_url")"
  active_sessions="$(metric_value "$metrics" rushls_active_sessions)"
  ready_count="$(ready_playlists)"
  if [[ "$active_sessions" == "$publisher_count" && "$ready_count" == "$publisher_count" ]]; then
    break
  fi
  if ((attempt == 60)); then
    for log_path in "${publisher_log_prefix}"-*.log; do
      echo "--- $log_path ---" >&2
      tail -n 40 "$log_path" >&2
    done
    tail -n 80 "$server_log" >&2
    die "not all publishers became active and playable"
  fi
  sleep 1
done

echo "all $publisher_count publishers are active; soaking for ${duration_seconds}s"
soak_started_epoch="$(date +%s)"
sample steady-state 0
while :; do
  now_epoch="$(date +%s)"
  elapsed=$((now_epoch - soak_started_epoch))
  ((elapsed >= duration_seconds)) && break
  sleep "$sample_interval"
  now_epoch="$(date +%s)"
  elapsed=$((now_epoch - soak_started_epoch))
  ((elapsed > duration_seconds)) && elapsed="$duration_seconds"
  sample steady-state "$elapsed"
done

echo "soak duration reached; stopping publishers"
for publisher_pid in "${publisher_pids[@]}"; do
  kill "$publisher_pid" 2>/dev/null || true
done
for publisher_pid in "${publisher_pids[@]}"; do
  wait "$publisher_pid" 2>/dev/null || true
done

post_stop_started_epoch="$(date +%s)"
sample post-stop 0
while :; do
  now_epoch="$(date +%s)"
  elapsed=$((now_epoch - post_stop_started_epoch))
  ((elapsed >= retention_wait_seconds)) && break
  sleep "$post_stop_interval"
  now_epoch="$(date +%s)"
  elapsed=$((now_epoch - post_stop_started_epoch))
  ((elapsed > retention_wait_seconds)) && elapsed="$retention_wait_seconds"
  sample post-stop "$elapsed"
done

echo "post-stop observation reached ${retention_wait_seconds}s"
