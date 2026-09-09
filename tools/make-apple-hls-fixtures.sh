#!/usr/bin/env bash
# Regenerates the Apple HLS integration fixtures.
#
# Every fixture is synthetic: FFmpeg test patterns and tones, never third-party
# media. Each is about ten seconds long, which is five segments at the two-second
# cadence the suite runs, and encoded at a low bitrate so the repository does not
# grow by a megabyte per case.
#
# The fixtures exist to make one input property unusual at a time. Where a
# comment below names a property, that is what the fixture is for; everything
# else about it is deliberately ordinary so a failure has one candidate cause.
#
# Requires: ffmpeg with libx264, libx265, libsvtav1, libopus and macOS aac_at,
# and GStreamer for the AV1 MPEG-TS mapping FFmpeg cannot write.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
out="${1:-$here/../tests/apple_hls/fixtures}"
mkdir -p "$out"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

seconds=8
size=320x180

# One shared source: a moving pattern with enough detail that a rate-controlled
# encoder produces varied frame sizes, and a tone that is easy to recognise.
pattern="testsrc2=size=$size:rate=30:duration=$seconds"
tone="sine=frequency=997:sample_rate=48000:duration=$seconds"

say() { printf '\n== %s\n' "$1"; }

# --- High B-frame reordering -------------------------------------------------
# Eight consecutive B-frames with a normal pyramid and adaptive placement, so
# the composition offsets are large and DTS trails PTS by several frames. This
# is the input that exercises reorder handling end to end: pre-roll boundary
# selection, CMAF composition offsets, and the part cut that has to land on a
# presentation instant rather than a decode one.
say "h264_bpyramid_aac.flv"
ffmpeg -y -loglevel error \
  -f lavfi -i "$pattern" -f lavfi -i "$tone" \
  -c:v libx264 -profile:v high -preset veryfast -b:v 140k -g 60 -keyint_min 60 \
  -bf 8 -x264-params "bframes=8:b-pyramid=normal:b-adapt=2:ref=5:scenecut=0:open-gop=0" \
  -pix_fmt yuv420p \
  -c:a aac -b:a 48k -ac 2 \
  -f flv "$out/h264_bpyramid_aac.flv"

# --- HE-AAC (SBR) ------------------------------------------------------------
# The audio object type changes the RFC 6381 string to mp4a.40.5 and makes the
# decoder output rate twice the encoded frame rate, which is exactly the pair a
# manifest gets wrong when it reads only one of them.
say "h264_he_aac.flv"
ffmpeg -y -loglevel error -f lavfi -i "$tone" \
  -c:a aac_at -profile:a 4 -b:a 32k -ac 2 "$work/he.m4a"
ffmpeg -y -loglevel error \
  -f lavfi -i "$pattern" -i "$work/he.m4a" \
  -c:v libx264 -profile:v high -preset veryfast -b:v 120k -g 60 -keyint_min 60 -pix_fmt yuv420p \
  -c:a copy -shortest -f flv "$out/h264_he_aac.flv"

# --- Fractional frame rate ---------------------------------------------------
# 24000/1001 with a two-second key frame interval: the GOP is 48048/24000 of a
# second, so no whole number of milliseconds is the right segment period and the
# cadence has to carry the exact rational.
say "h264_2398_aac.ts"
ffmpeg -y -loglevel error \
  -f lavfi -i "testsrc2=size=$size:rate=24000/1001:duration=$seconds" -f lavfi -i "$tone" \
  -c:v libx264 -profile:v high -preset veryfast -b:v 120k -g 48 -keyint_min 48 \
  -x264-params "scenecut=0:open-gop=0" -pix_fmt yuv420p \
  -c:a aac -b:a 48k -ac 2 -shortest \
  -f mpegts "$out/h264_2398_aac.ts"

# --- Three-rendition ladder --------------------------------------------------
# Aligned key frames across three resolutions, which is what makes the renditions
# switchable and what the multivariant projection has to agree about.
#
# `-s` is a per-output option, so scaling has to happen in the filter graph:
# one `-s` before the format would resize every mapped copy to the same size and
# produce three identical renditions, which is not a ladder.
say "h264_ladder3_aac.ts"
ffmpeg -y -loglevel error \
  -f lavfi -i "$pattern" -f lavfi -i "$tone" \
  -filter_complex "[0:v]split=3[hi][mid][lo];\
[hi]scale=480:270[vhi];[mid]scale=320:180[vmid];[lo]scale=160:90[vlo]" \
  -map "[vhi]" -b:v:0 240k \
  -map "[vmid]" -b:v:1 120k \
  -map "[vlo]" -b:v:2 60k \
  -c:v libx264 -profile:v high -preset veryfast \
  -g 60 -keyint_min 60 -x264-params "scenecut=0:open-gop=0" -pix_fmt yuv420p \
  -map 1:a -c:a aac -b:a 48k -ac 2 -shortest \
  -f mpegts "$out/h264_ladder3_aac.ts"

# --- HDR10 -------------------------------------------------------------------
# BT.2020 primaries with the PQ transfer function plus mastering-display and
# content-light SEI. A multivariant playlist must answer VIDEO-RANGE=PQ for it.
say "hevc_hdr10_aac.ts"
ffmpeg -y -loglevel error \
  -f lavfi -i "$pattern" -f lavfi -i "$tone" \
  -c:v libx265 -preset veryfast -b:v 140k -pix_fmt yuv420p10le \
  -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc \
  -x265-params "keyint=60:min-keyint=60:scenecut=0:open-gop=0:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50):max-cll=1000,400" \
  -c:a aac -b:a 48k -ac 2 -shortest -tag:v hvc1 \
  -f mpegts "$out/hevc_hdr10_aac.ts"

# --- Two audio languages -----------------------------------------------------
# Distinct ISO 639 codes in the PMT, so LANGUAGE comes from the input rather
# than from a test stamping one on. Autoselected languages must stay distinct.
say "h264_multilang_aac.ts"
ffmpeg -y -loglevel error \
  -f lavfi -i "$pattern" \
  -f lavfi -i "$tone" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$seconds" \
  -map 0:v -c:v libx264 -profile:v high -preset veryfast -b:v 120k -g 60 -keyint_min 60 \
  -x264-params "scenecut=0:open-gop=0" -pix_fmt yuv420p \
  -map 1:a -map 2:a -c:a aac -b:a 48k -ac 2 \
  -metadata:s:a:0 language=eng -metadata:s:a:1 language=swe -shortest \
  -f mpegts "$out/h264_multilang_aac.ts"

# --- 44.1 kHz mono -----------------------------------------------------------
# Neither the sample rate nor the channel count is the common case, and the rate
# does not divide any video timescale evenly, so the audio grid never lands on a
# video boundary exactly.
say "h264_aac_441_mono.flv"
ffmpeg -y -loglevel error \
  -f lavfi -i "$pattern" \
  -f lavfi -i "sine=frequency=997:sample_rate=44100:duration=$seconds" \
  -c:v libx264 -profile:v high -preset veryfast -b:v 120k -g 60 -keyint_min 60 \
  -x264-params "scenecut=0:open-gop=0" -pix_fmt yuv420p \
  -c:a aac -b:a 40k -ac 1 -ar 44100 -shortest \
  -f flv "$out/h264_aac_441_mono.flv"

# --- 5.1 surround ------------------------------------------------------------
# CHANNELS="6" on the audio rendition, and a channel layout the muxer has to
# carry into the sample entry rather than assume.
say "h264_aac_51.flv"
ffmpeg -y -loglevel error \
  -f lavfi -i "$pattern" \
  -f lavfi -i "sine=frequency=997:sample_rate=48000:duration=$seconds" \
  -c:v libx264 -profile:v high -preset veryfast -b:v 120k -g 60 -keyint_min 60 \
  -x264-params "scenecut=0:open-gop=0" -pix_fmt yuv420p \
  -filter:a "pan=5.1|c0=c0|c1=c0|c2=c0|c3=c0|c4=c0|c5=c0" \
  -c:a aac -b:a 128k -shortest \
  -f flv "$out/h264_aac_51.flv"

# --- Key frame interval longer than the segment target ------------------------
# A five-second GOP against a two-second desired cadence. Segments cannot be cut
# where they were planned, so the policy's maximum has to decide instead.
say "h264_longgop_aac.ts"
ffmpeg -y -loglevel error \
  -f lavfi -i "testsrc2=size=$size:rate=30:duration=12" \
  -f lavfi -i "sine=frequency=997:sample_rate=48000:duration=12" \
  -c:v libx264 -profile:v high -preset veryfast -b:v 120k -g 150 -keyint_min 150 \
  -x264-params "scenecut=0:open-gop=0" -pix_fmt yuv420p \
  -c:a aac -b:a 48k -ac 2 -shortest \
  -f mpegts "$out/h264_longgop_aac.ts"

# --- Anamorphic pixels -------------------------------------------------------
# SAR 4:3, so the encoded and displayed resolutions differ. RESOLUTION in a
# multivariant playlist is the encoded one; getting it from the display size
# would advertise a rendition that does not exist.
say "h264_anamorphic_aac.ts"
ffmpeg -y -loglevel error \
  -f lavfi -i "$pattern" -f lavfi -i "$tone" \
  -vf "setsar=4/3" \
  -c:v libx264 -profile:v high -preset veryfast -b:v 120k -g 60 -keyint_min 60 \
  -x264-params "scenecut=0:open-gop=0" -pix_fmt yuv420p \
  -c:a aac -b:a 48k -ac 2 -shortest \
  -f mpegts "$out/h264_anamorphic_aac.ts"

# --- Opus beside H.264 -------------------------------------------------------
# Opus is a codec this origin accepts and Apple's HLS profile does not list.
# Packaging it correctly and being told so by the validator are different
# questions, and the fixture exists to keep them separate.
say "h264_opus.ts"
ffmpeg -y -loglevel error \
  -f lavfi -i "$pattern" -f lavfi -i "$tone" \
  -c:v libx264 -profile:v high -preset veryfast -b:v 120k -g 60 -keyint_min 60 \
  -x264-params "scenecut=0:open-gop=0" -pix_fmt yuv420p \
  -c:a libopus -b:a 48k -ac 2 -shortest \
  -f mpegts "$out/h264_opus.ts"

# --- AV1 ---------------------------------------------------------------------
# FFmpeg cannot write AV1 into MPEG-TS, so the OBU stream is remuxed by
# GStreamer's custom AV1G mapping, which is the mapping this origin's demuxer
# implements.
say "av1_long.ts"
ffmpeg -y -loglevel error \
  -f lavfi -i "testsrc2=size=160x96:rate=10:duration=$seconds" \
  -c:v libsvtav1 -preset 12 -g 20 -b:v 80k -pix_fmt yuv420p \
  -f obu "$work/av1.obu"
if command -v gst-launch-1.0 >/dev/null; then
  gst-launch-1.0 -q filesrc "location=$work/av1.obu" ! av1parse ! \
    mpegtsmux enable-custom-mappings=true ! filesink "location=$out/av1_long.ts"
else
  printf 'skipping av1_long.ts: gst-launch-1.0 is not installed\n' >&2
fi

say "done"
ls -l "$out"
