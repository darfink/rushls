#!/usr/bin/env python3
"""Check and decode a live browser ladder. Requires ffprobe and ffmpeg on PATH."""

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("url", nargs="?", default="http://localhost:18080/ladder/index.m3u8")
    parser.add_argument("--seconds", type=int, default=30)
    args = parser.parse_args()
    if args.seconds < 1:
        parser.error("--seconds must be positive")

    deadline = time.monotonic() + 30
    while True:
        try:
            with urllib.request.urlopen(args.url, timeout=3) as response:
                if not response.read().startswith(b"#EXTM3U"):
                    raise ValueError("The origin did not return an HLS playlist")
            break
        except (urllib.error.URLError, TimeoutError):
            if time.monotonic() >= deadline:
                raise RuntimeError("No HLS ladder appeared within 30 seconds") from None
            time.sleep(0.2)

    probe = subprocess.run(
        ["ffprobe", "-v", "error", "-show_streams", "-of", "json", args.url],
        check=True, capture_output=True, text=True, timeout=30,
    )
    streams = json.loads(probe.stdout)["streams"]
    videos = [stream for stream in streams if stream["codec_type"] == "video"]
    audio = [stream for stream in streams if stream["codec_type"] == "audio"]
    expected = {(1920, 1080), (1280, 720), (640, 360)}
    actual = {(stream["width"], stream["height"]) for stream in videos}
    if len(videos) != 3 or actual != expected or any(stream["codec_name"] != "h264" for stream in videos):
        raise ValueError(f"Expected three H.264 ladder renditions, found: {actual}")
    if len(audio) != 1 or audio[0]["codec_name"] not in {"aac", "opus"}:
        raise ValueError("Expected one AAC or Opus audio track")
    if int(audio[0]["sample_rate"]) != 48000 or audio[0]["channels"] != 2:
        raise ValueError("Expected 48 kHz stereo audio")

    # Decode and hash every output stream. Hash records prove each rendition
    # produced frames, rather than merely trusting the master playlist labels.
    # Allow two GOPs for different live rendition join points (the bench uses 2s GOPs).
    result = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-xerror",
         "-i", args.url, "-t", str(args.seconds + 4), "-map", "0:v", "-map", "0:a:0",
         "-fps_mode:v", "passthrough", "-enc_time_base:v", "demux",
         "-c:v", "rawvideo", "-c:a", "pcm_s16le", "-f", "framehash", "-hash", "adler32", "-"],
        check=True, capture_output=True, text=True, timeout=args.seconds + 60,
    )
    counts = [0] * 4
    first = [None] * 4
    end = [0] * 4
    timebases = {}
    for line in result.stdout.splitlines():
        if line.startswith("#tb "):
            index, ratio = line[4:].split(":", 1)
            numerator, denominator = ratio.strip().split("/")
            timebases[int(index)] = int(numerator) / int(denominator)
        elif line and not line.startswith("#"):
            index, _, pts, duration, *_ = line.split(",")
            index, pts, duration = int(index), int(pts), int(duration)
            counts[index] += 1
            if first[index] is None:
                first[index] = pts
            end[index] = max(end[index], pts + duration)
    for index, count in enumerate(counts):
        if count == 0 or (end[index] - first[index]) * timebases[index] < args.seconds - 0.1:
            raise ValueError(f"Output stream {index} did not decode {args.seconds}s ({count} frames)")
    print(f"PASS: 1080p, 720p, 360p and stereo {audio[0]['codec_name'].upper()}, {args.seconds}s each")
    print(f"Decoded frame/packet counts: {counts}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr, file=sys.stderr)
        sys.exit(1)
