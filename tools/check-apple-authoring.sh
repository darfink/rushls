#!/usr/bin/env bash
# Publish a synthetic multitrack ladder over RTMP, then audit its live DVR.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
report_dir="${RUSHLS_TEST_REPORT_DIR:-$PWD/target/apple-authoring}"
mkdir -p "$report_dir"
report_dir="$(cd "$report_dir" && pwd)"
command -v mediastreamvalidator >/dev/null
command -v hlsreport >/dev/null
command -v ffmpeg >/dev/null
# The full ladder is real retained media, not just a two-hour playlist.
# Fail before encoding if the origin cannot safely keep its DVR window.
check_space() {
python3 - <<'CHECK_SPACE'
import shutil, tempfile
required = 6 * 1024**3
directory = tempfile.gettempdir()
free = shutil.disk_usage(directory).free
if free < required:
    raise SystemExit(f"Apple authoring audit needs 6 GiB free at {directory}; found {free / 1024**3:.1f} GiB. Free space before running the audit.")
CHECK_SPACE
}
check_space
fixture="$report_dir/synthetic-rtmp-v2"
mkdir -p "$fixture"
# Encode one exact 32-second period. The publisher repeats compressed samples,
# with timestamps derived from frame/sample counts rather than FLV rounding.
# Keep the small templates for repeatable local audits; never store two hours
# of duplicate source media. HLS output uses the origin's bounded disk tier.
# Keep a video-bearing cellular fallback: AirPlay 2 forbids audio-only variants.
# Its 48 kb/s video plus 96 kb/s AAC leaves headroom below the 192 kb/s cap.
profiles=('960x540:2000' '416x234:145' '640x360:365' '768x432:730' '768x432:1100' '416x234:48')
for index in "${!profiles[@]}"; do
  file="$fixture/video-$index.flv"
  if test -f "$file"; then continue; fi
  size="${profiles[$index]%:*}"
  bitrate="${profiles[$index]#*:}"
  ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=$size:rate=30" -t 32 -an \
    -c:v libx264 -preset fast -threads 2 -pix_fmt yuv420p -profile:v high \
    -b:v "${bitrate}k" -minrate "${bitrate}k" -maxrate "${bitrate}k" -bufsize "$((bitrate * 2))k" \
    -g 60 -keyint_min 60 -sc_threshold 0 -bf 0 \
    -x264-params 'nal-hrd=cbr:force-cfr=1:open-gop=0' \
    -color_primaries bt709 -color_trc bt709 -colorspace bt709 \
    -f flv "$file.tmp"
  mv "$file.tmp" "$file"
done
for index in 0 1; do
  file="$fixture/audio-$index-96.flv"
  if test -f "$file"; then continue; fi
  # Extra samples allow the sender to discard encoder priming before retaining
  # exactly 1500 AAC frames. The second language uses a distinguishable tone.
  ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "sine=frequency=$((440 + index * 220)):sample_rate=48000" \
    -t 33 -vn -c:a aac -b:a 96k -ac 2 -f flv "$file.tmp"
  mv "$file.tmp" "$file"
done
# Build before the final space check: compiler output shares runner storage.
cargo test --locked --test apple_hls --no-run
check_space
RUSHLS_TEST_AUTHORING_FIXTURE="$fixture" RUSHLS_TEST_REQUIRE_APPLE_TOOLS=1 \
  RUSHLS_TEST_HLSREPORT=strict RUSHLS_TEST_REPORT_DIR="$report_dir" \
  RUSHLS_TEST_TRACE_DIR="$report_dir" \
  cargo test --locked --test apple_hls authoring::two_hour_live_authoring -- --ignored --exact --nocapture
