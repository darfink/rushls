#!/usr/bin/env python3
"""Build Safari experiments from ip_video_gap_playback_fixture exports.

These diagnostic playlists are not production output or conformance claims.
Encoded packets and timestamps are retained, except explicitly omitted media.
Use master-full.m3u8; these variants do not model live partial publication.
"""
import argparse
from pathlib import Path
import shutil
import struct


def boxes(data):
    offset = 0
    while offset < len(data):
        size, kind = struct.unpack_from('>I4s', data, offset)
        assert 8 <= size <= len(data) - offset
        yield kind, data[offset + 8:offset + size]
        offset += size


def box(kind, body):
    return struct.pack('>I4s', len(body) + 8, kind) + body


def missing_sample(control):
    """Extract the original picture at 5.04 s into a single-sample fragment."""
    parts = list(boxes((control / 'segment/3.m4s').read_bytes()))
    moofs = [body for kind, body in parts if kind == b'moof']
    mdats = [body for kind, body in parts if kind == b'mdat']
    traf = dict(boxes(dict(boxes(moofs[5]))[b'traf']))
    trun = traf[b'trun']
    # Deliberately accept only this known, unencrypted test fixture layout.
    assert struct.unpack_from('>II', trun) == (0x701, 5)
    assert struct.unpack_from('>Q', traf[b'tfdt'], 4)[0] == 450000
    entries = [struct.unpack_from('>III', trun, 12 + i * 12) for i in range(5)]
    duration, size, flags = entries[1]
    assert duration == 3600 and flags & 0x10000
    payload = mdats[5][entries[0][1]:entries[0][1] + size]

    def fragment(data_offset):
        run = (struct.pack('>III', 0x701, 1, data_offset)
               + struct.pack('>III', duration, size, flags))
        return box(b'moof', box(b'mfhd', struct.pack('>II', 0, 26))
                   + box(b'traf', box(b'tfhd', traf[b'tfhd'])
                         + box(b'tfdt', struct.pack('>IQ', 1 << 24, 453600))
                         + box(b'trun', run)))

    return (box(b'styp', parts[0][1]) + fragment(len(fragment(0)) + 8)
            + box(b'mdat', payload))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixtures', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    candidates = [p for p in args.fixtures.glob('H264-*')
                  if not p.name.endswith('-control')]
    if len(candidates) != 1:
        parser.error('Expected one exported H.264 damaged fixture and its control')
    gap = candidates[0]
    control = gap.with_name(gap.name + '-control')
    base = (gap / 'full.m3u8').read_text()
    gap_tail = '#EXTINF:0.04,\nsegment/4.m4s\n#EXTINF:0.92,\nsegment/5.m4s'
    affected = '#EXTINF:1.04,\nsegment/3.m4s\n#EXT-X-GAP\n' + gap_tail
    assert affected in base
    until_idr = base.replace(gap_tail, '#EXTINF:0.96,\nsegment/4.m4s')
    variants = {
        '00-control': (control, (control / 'full.m3u8').read_text()),
        '01-exact-gap': (gap, base),
        '02-gap-discontinuity': (gap, base.replace(
            '#EXTINF:0.92,', '#EXT-X-DISCONTINUITY\n#EXTINF:0.92,')),
        '03-gap-to-idr': (gap, until_idr),
        '04-gap-to-idr-discontinuity': (gap, until_idr.replace(
            '#EXTINF:2,\nsegment/6.m4s',
            '#EXT-X-DISCONTINUITY\n#EXTINF:2,\nsegment/6.m4s')),
        '05-sparse-parent': (gap, base.replace(
            affected, '#EXTINF:2,\nsegment/merged.m4s')),
        '06-whole-parent-gap': (gap, base.replace(
            affected, '#EXT-X-GAP\n#EXTINF:2,\nsegment/4.m4s')),
        '07-split-control': (gap, base.replace('#EXT-X-GAP\n', '')),
        '08-unmarked-hole': (gap, base.replace(
            '#EXTINF:1.04,\nsegment/3.m4s\n#EXT-X-GAP\n#EXTINF:0.04,\nsegment/4.m4s',
            '#EXTINF:1.08,\nsegment/3.m4s')),
    }
    args.output.mkdir(parents=True, exist_ok=True)
    for name, (source, playlist) in variants.items():
        directory = args.output / name
        for child in ['init', 'segment']:
            shutil.copytree(source / child, directory / child, dirs_exist_ok=True)
        (directory / 'full.m3u8').write_text(playlist)
        (directory / 'master-full.m3u8').write_text(
            (source / 'master-full.m3u8').read_text())
        if name == '05-sparse-parent':
            (directory / 'segment/merged.m4s').write_bytes(
                (gap / 'segment/3.m4s').read_bytes()
                + (gap / 'segment/5.m4s').read_bytes())
        if name == '07-split-control':
            # Restore the missing picture to isolate segmentation from media loss.
            (directory / 'segment/4.m4s').write_bytes(missing_sample(control))
    print(args.output)


if __name__ == '__main__':
    main()
