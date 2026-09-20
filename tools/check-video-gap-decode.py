#!/usr/bin/env python3
"""Compare the I/P-only video GAP fixture with its uninterrupted control.

Concatenate available objects for decoder analysis. This is not an HLS player test.
Requires ffmpeg and fixtures exported by ip_video_gap_playback_fixture.
"""
import argparse
from fractions import Fraction
import json
from pathlib import Path
import subprocess


def decode(directory):
    names = [line for line in (directory / 'full.m3u8').read_text().splitlines()
             if line and not line.startswith('#') and (directory / line).exists()]
    movie = directory / 'available.mp4'
    movie.write_bytes((directory / 'init/1.mp4').read_bytes()
                     + b''.join((directory / name).read_bytes() for name in names))
    result = subprocess.run(
        ['ffmpeg', '-v', 'warning', '-i', str(movie), '-fps_mode', 'passthrough',
         '-f', 'framemd5', '-'], capture_output=True, text=True, check=True)
    (directory / 'decoder.log').write_text(result.stderr)
    (directory / 'frames.md5').write_text(result.stdout)
    timebase = Fraction(next(line.split(':', 1)[1].strip()
                            for line in result.stdout.splitlines() if line.startswith('#tb 0:')))
    frames = {}
    for line in result.stdout.splitlines():
        if line and not line.startswith('#'):
            fields = line.split(',')
            frames[int(fields[2]) * timebase] = fields[-1].strip()
    packets = subprocess.run(
        ['ffprobe', '-v', 'error', '-select_streams', 'v', '-show_packets',
         '-show_data_hash', 'sha256', '-show_entries', 'packet=pts,dts,duration,data_hash',
         '-of', 'json', str(movie)], capture_output=True, text=True, check=True)
    return frames, json.loads(packets.stdout)['packets']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixtures', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    results = []
    for directory in sorted(args.fixtures.glob('H264-*')):
        if directory.name.endswith('-control'):
            continue
        control, control_packets = decode(directory.with_name(directory.name + '-control'))
        damaged, damaged_packets = decode(directory)
        missing = sorted(set(control) - set(damaged))
        changed = sorted(t for t in damaged if damaged[t] != control.get(t))
        # The fixed test removes frame 126 and has its next IDR at six seconds.
        packets_preserved = damaged_packets == control_packets[:126] + control_packets[127:]
        ok = (packets_preserved and len(control) == 200 and len(damaged) == 199
              and missing == [Fraction(126, 25)]
              and all(damaged[t] == control[t] for t in damaged if t >= 6))
        results.append(dict(fixture=directory.name, control_frames=len(control),
                            decoded_frames=len(damaged), packets_preserved=packets_preserved, missing_seconds=list(map(float, missing)),
                            changed_seconds=list(map(float, changed)),
                            exact_match_from_next_idr=all(damaged[t] == control[t] for t in damaged if t >= 6),
                            ok=ok))
    args.output.write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))
    return int(not results or any(not result['ok'] for result in results))


if __name__ == '__main__':
    raise SystemExit(main())
