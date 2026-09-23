#!/usr/bin/env python3
"""Decode Rushls live control and GAP streams with GStreamer's HLS client."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time
import urllib.request


def playback(Gst, url):
    pipeline = Gst.ElementFactory.make('playbin3')
    if pipeline is None:
        raise RuntimeError('GStreamer playbin3 is unavailable')
    tracks = {kind: {'buffers': 0, 'first_pts': None, 'last_pts': None, 'caps': None}
              for kind in ('audio', 'video')}
    demuxers = set()

    def element_added(_bin, _sub_bin, element):
        factory = element.get_factory()
        if factory and factory.get_name().startswith('hlsdemux'):
            demuxers.add(factory.get_name())

    def handoff(_sink, buffer, pad, kind):
        track = tracks[kind]
        track['buffers'] += 1
        track['caps'] = pad.get_current_caps().to_string()
        if buffer.pts != Gst.CLOCK_TIME_NONE:
            pts = buffer.pts / Gst.SECOND
            if track['first_pts'] is None:
                track['first_pts'] = pts
            track['last_pts'] = pts

    pipeline.connect('deep-element-added', element_added)
    for kind in tracks:
        sink = Gst.ElementFactory.make('fakesink')
        sink.set_property('sync', True)
        sink.set_property('signal-handoffs', True)
        sink.connect('handoff', handoff, kind)
        pipeline.set_property(f'{kind}-sink', sink)
    pipeline.set_property('uri', url)
    result = {'gstreamer': Gst.version_string(), 'url': url, 'tracks': tracks,
              'eos': False, 'errors': [], 'warnings': []}
    bus = pipeline.get_bus()
    try:
        pipeline.set_state(Gst.State.PLAYING)
        deadline = time.monotonic() + 75
        while time.monotonic() < deadline:
            message = bus.timed_pop_filtered(Gst.SECOND,
                Gst.MessageType.ERROR | Gst.MessageType.WARNING | Gst.MessageType.EOS)
            if message is None:
                continue
            if message.type == Gst.MessageType.EOS:
                result['eos'] = True
                break
            if message.type == Gst.MessageType.ERROR:
                error, debug = message.parse_error()
                result['errors'].append(f'{error}: {debug}')
                break
            warning, debug = message.parse_warning()
            result['warnings'].append(f'{warning}: {debug}')
    finally:
        pipeline.set_state(Gst.State.NULL)
    result['demuxers'] = sorted(demuxers)
    # EOS alone can pass an empty or undecoded stream. Require raw output from
    # both tracks over a substantial part of this 24-second live fixture.
    result['ok'] = result['eos'] and not result['errors'] and 'hlsdemux2' in demuxers
    for kind, track in tracks.items():
        result['ok'] = result['ok'] and (
            track['buffers'] >= 100
            and track['caps'] is not None and track['caps'].startswith(f'{kind}/x-raw')
            and track['first_pts'] is not None and track['last_pts'] - track['first_pts'] >= 12
        )
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--case', choices=('control', 'gaps', 'all'), default='control',
                        help='GAP recovery is diagnostic until the upstream GAP parser is corrected')
    args = parser.parse_args()
    import gi
    gi.require_version('Gst', '1.0')
    from gi.repository import Gst
    Gst.init(None)
    for plugin in ('playbin3', 'hlsdemux2', 'avdec_h264', 'avdec_aac'):
        if Gst.ElementFactory.find(plugin) is None:
            raise RuntimeError(f'Missing required GStreamer plugin: {plugin}')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    subprocess.run(['cargo', 'test', '--locked', '--lib', '--no-run'], check=True)
    failures = []
    cases = ('control', 'gaps') if args.case == 'all' else (args.case,)
    for case in cases:
        ready = output / f'{case}.ready'
        for suffix in ('.ready', '.start', '.done', '.outcome', '.events', '.browser'):
            ready.with_suffix(suffix).unlink(missing_ok=True)
        env = {**os.environ, 'RUSHLS_GAP_LIVE_READY': str(ready)}
        env.pop('RUSHLS_GAP_LIVE_CONTROL', None)
        if case == 'control':
            env['RUSHLS_GAP_LIVE_CONTROL'] = '1'
        with (output / f'{case}.log').open('w') as log:
            origin = subprocess.Popen(['cargo', 'test', '--locked', '--lib',
                'live_av_gap_browser_origin', '--', '--ignored', '--nocapture'],
                env=env, stdout=log, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 60
                while not ready.exists():
                    if origin.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError('Rust origin did not become ready')
                    time.sleep(.1)
                url = ready.read_text().strip()
                ready.with_suffix('.start').touch()
                while True:
                    try:
                        with urllib.request.urlopen(url, timeout=1) as response:
                            (output / f'{case}.m3u8').write_bytes(response.read())
                        break
                    except OSError:
                        if time.monotonic() >= deadline:
                            raise RuntimeError('Live playlist did not become available')
                        time.sleep(.1)
                report = playback(Gst, url)
                (output / f'{case}.json').write_text(json.dumps(report, indent=2) + '\n')
                ready.with_suffix('.done').touch()
                origin.wait(timeout=45)
                if not report['ok'] or origin.returncode != 0:
                    raise RuntimeError('Playback or Rust origin failed; inspect the report and log')
                print(f'{case}: decoded audio and video through EOS', flush=True)
            except (RuntimeError, subprocess.SubprocessError) as error:
                failures.append(f'{case}: {error}')
            finally:
                ready.with_suffix('.done').touch()
                if origin.poll() is None:
                    origin.terminate()
                    origin.wait(timeout=15)
    if failures:
        raise RuntimeError('\n'.join(failures))


if __name__ == '__main__':
    main()
