#!/usr/bin/env python3
"""Extract tagged documentation commands, publish them, and verify their HLS output."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request

DOCUMENTS = ('README.md', 'docs/publishing.md')
REQUIRED = {'fixture', 'quickstart', 'ffmpeg-rtmp', 'ffmpeg-srt', 'gstreamer-rtmp',
            'gstreamer-srt', 'ladder-srt', 'ladder-rtmp', 'alternate-audio', 'captions'}
MARKER = re.compile(r'<!-- verify: (.*?) -->\s*```sh\r?\n(.*?)^```', re.M | re.S)


def extract_examples(documents):
    examples = {}
    for name, text in documents:
        matches = list(MARKER.finditer(text))
        if text.count('<!-- verify:') != len(matches):
            raise ValueError(f'{name}: verification marker must precede a closed sh fence')
        for match in matches:
            spec = json.loads(match[1])
            identifier = spec['id']
            if not re.fullmatch(r'[a-z][a-z0-9-]*', identifier) or identifier in examples:
                raise ValueError(f'Invalid or duplicate example ID: {identifier}')
            if identifier != 'fixture':
                for key in ('stream', 'video', 'audio'):
                    if key not in spec:
                        raise ValueError(f'{identifier}: missing {key}')
                for key in ('video', 'audio', 'subtitles'):
                    if type(spec.get(key, 0)) is not int or spec.get(key, 0) < 0:
                        raise ValueError(f'{identifier}: invalid {key} count')
            examples[identifier] = dict(spec, command=match[2], source=name,
                                        line=text[:match.start()].count('\n') + 1)
    return examples


def attributes(line):
    return dict((key, value.strip('"')) for key, value in
                re.findall(r'([A-Z0-9-]+)=("[^"]*"|[^,]*)', line.partition(':')[2]))


def rendition_urls(master, base):
    streams, media = [], []
    pending = None
    for line in master.splitlines():
        if line.startswith('#EXT-X-STREAM-INF:'):
            pending = attributes(line)
        elif line.startswith('#EXT-X-MEDIA:'):
            entry = attributes(line)
            if 'URI' in entry:
                media.append((entry['TYPE'].lower(), urllib.parse.urljoin(base, entry['URI']), entry))
        elif line and not line.startswith('#') and pending is not None:
            streams.append(('video', urllib.parse.urljoin(base, line), pending))
            pending = None
    return streams, media


def verify_master(spec, master, base):
    streams, media = rendition_urls(master, base)
    counts = {'video': len(streams), 'audio': sum(kind == 'audio' for kind, _, _ in media),
              'subtitles': sum(kind == 'subtitles' for kind, _, _ in media)}
    for kind, actual in counts.items():
        if actual != spec.get(kind, 0):
            raise ValueError(f'{kind}: expected {spec.get(kind, 0)}, got {actual}')
    if 'languages' in spec:
        languages = sorted(entry.get('LANGUAGE') for kind, _, entry in media if kind == 'audio')
        if languages != sorted(spec['languages']):
            raise ValueError(f'Wrong audio languages: {languages}')
    return streams + media, counts


def decoded(progress, kind):
    # Exit status alone permits a decoder that receives no samples to pass.
    times = [int(value) for value in re.findall(r'^out_time_us=(\d+)$', progress, re.M)]
    frames = [int(value) for value in re.findall(r'^frame=(\d+)$', progress, re.M)]
    return bool(times and max(times) >= 1_000_000 and
                (kind != 'video' or (frames and max(frames) > 0)))


def has_caption(initialization, segments, expected):
    # Rushls uses EXT-X-MAP for the WebVTT header; media segments hold cue bodies.
    return initialization.startswith('WEBVTT') and any(
        ' --> ' in body and expected in body for body in segments)


def port(udp=False):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM if udp else socket.SOCK_STREAM) as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def stop(process):
    # Shell recipes can contain pipelines. Stop the whole group, including children
    # after the shell itself exits, so a failed example cannot leave publishers alive.
    if process is None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        if process.poll() is None:
            raise
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        if process.poll() is None:
            raise
    process.wait(timeout=5)


def fetch(url):
    with urllib.request.urlopen(url, timeout=3) as response:
        return response.read().decode()


def run_recipe(spec, working, output, ports):
    identifier = spec['id']
    folder = output / identifier
    folder.mkdir(exist_ok=True)
    command = spec['command'].replace(f'live/{spec["stream"]}', f'live/{identifier}')
    for original, actual in ports.items():
        command = command.replace(f'127.0.0.1:{original}', f'127.0.0.1:{actual}')
    (folder / 'command.sh').write_text(command)
    base = f'http://127.0.0.1:{ports[8080]}/live/{identifier}/index.m3u8'
    process = None
    result = {'id': identifier, 'source': spec['source'], 'line': spec['line'], 'ok': False}
    try:
        with (folder / 'publisher.log').open('w') as log:
            process = subprocess.Popen(['bash', '-eo', 'pipefail', '-c', command], cwd=working,
                                       stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            deadline = time.monotonic() + 25
            while True:
                if process.poll() not in (None, 0):
                    raise RuntimeError(f'Publisher exited with {process.returncode}')
                try:
                    master = fetch(base)
                    if '#EXT-X-STREAM-INF:' in master:
                        break
                except OSError:
                    pass
                if time.monotonic() >= deadline:
                    raise RuntimeError('Timed out waiting for the multivariant playlist')
                time.sleep(.2)
            (folder / 'master.m3u8').write_text(master)
            renditions, result['counts'] = verify_master(spec, master, base)
            for index, (kind, url, _) in enumerate(renditions):
                if kind == 'subtitles':
                    deadline = time.monotonic() + 20
                    while True:
                        playlist = fetch(url)
                        (folder / 'subtitles.m3u8').write_text(playlist)
                        segments = [line for line in playlist.splitlines() if line and not line.startswith('#')]
                        bodies = [fetch(urllib.parse.urljoin(url, line)) for line in segments]
                        maps = [attributes(line)['URI'] for line in playlist.splitlines() if line.startswith('#EXT-X-MAP:')]
                        initialization = fetch(urllib.parse.urljoin(url, maps[0])) if maps else ''
                        (folder / 'captions.vtt').write_text(initialization + '\n'.join(bodies))
                        if has_caption(initialization, bodies, spec['text']):
                            break
                        if time.monotonic() >= deadline:
                            raise RuntimeError('Expected caption text did not reach WebVTT')
                        time.sleep(.2)
                else:
                    with (folder / f'decode-{index}.log').open('w') as log:
                        decoder = subprocess.Popen(['ffmpeg', '-v', 'error', '-xerror', '-i', url,
                            '-t', '2', '-progress', 'pipe:1', '-f', 'null', '-'],
                            stdout=subprocess.PIPE, stderr=log, text=True, start_new_session=True)
                        try:
                            progress, _ = decoder.communicate(timeout=35)
                            (folder / f'decode-{index}.progress').write_text(progress)
                            if decoder.returncode or not decoded(progress, kind):
                                raise RuntimeError(f'{kind} rendition {index} did not decode media')
                        finally:
                            stop(decoder)
            if process.poll() not in (None, 0):
                raise RuntimeError(f'Publisher exited with {process.returncode}')
            result['ok'] = True
    except Exception as error:
        result['error'] = str(error)
    finally:
        stop(process)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=Path('target/doc-examples'))
    parser.add_argument('--binary', type=Path, default=Path('target/debug/rushls'))
    parser.add_argument('--case', action='append', help='Run selected example IDs locally; CI runs all')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    report = {'ok': False, 'versions': {}, 'cases': []}
    server = None
    try:
        examples = extract_examples([(name, (root / name).read_text()) for name in DOCUMENTS])
        if not REQUIRED <= examples.keys():
            raise ValueError(f'Missing required examples: {sorted(REQUIRED - examples.keys())}')
        selected = args.case or [key for key in examples if key != 'fixture']
        if any(key not in examples or key == 'fixture' for key in selected):
            raise ValueError('Unknown publishing example selected')
        for command in (['ffmpeg', '-version'], ['gst-launch-1.0', '--version']):
            report['versions'][command[0]] = subprocess.check_output(command, text=True).splitlines()[0]
        if 'captions' in selected:
            subprocess.run(['gst-inspect-1.0', 'captionsflvmux'], check=True,
                           stdout=(output / 'caption-plugin.txt').open('w'))
        ports = {1935: port(), 9000: port(True), 8080: port()}
        with tempfile.TemporaryDirectory(prefix='recipes-', dir=output) as temporary:
            working = Path(temporary)
            fixture = examples['fixture']['command']
            (output / 'fixture.sh').write_text(fixture)
            with (output / 'fixture.log').open('w') as log:
                generator = subprocess.Popen(['bash', '-eo', 'pipefail', '-c', fixture], cwd=working,
                    stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    if generator.wait(timeout=60):
                        raise RuntimeError('Media fixture command failed')
                finally:
                    stop(generator)
            config = working / 'rushls.toml'
            config.write_text(f'[rtmp]\nlisten="127.0.0.1:{ports[1935]}"\n'
                              f'[srt]\nlisten="127.0.0.1:{ports[9000]}"\n'
                              f'[http]\nlisten="127.0.0.1:{ports[8080]}"\n')
            env = {key: value for key, value in os.environ.items() if not key.startswith('RUSHLS_')}
            with (output / 'origin.log').open('w') as log:
                server = subprocess.Popen([str(args.binary.resolve()), '--config', str(config)],
                    env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                deadline = time.monotonic() + 15
                while True:
                    if server.poll() is not None:
                        raise RuntimeError('Rushls exited during startup')
                    try:
                        fetch(f'http://127.0.0.1:{ports[8080]}/health/ready')
                        break
                    except OSError:
                        if time.monotonic() >= deadline:
                            raise RuntimeError('Rushls did not become ready')
                        time.sleep(.1)
                for key in selected:
                    result = run_recipe(examples[key], working, output, ports)
                    report['cases'].append(result)
                    print(json.dumps(result), flush=True)
            report['ok'] = all(case['ok'] for case in report['cases'])
    except Exception as error:
        report['error'] = str(error)
    finally:
        stop(server)
        (output / 'results.json').write_text(json.dumps(report, indent=2) + '\n')
        lines = ['## Documentation examples', '', '| Example | Result |', '| --- | --- |']
        for case in report['cases']:
            lines.append(f'| {case["id"]} | {"Passed" if case["ok"] else "Failed"} |')
        if 'error' in report:
            lines.extend(['', report['error']])
        lines.extend(['', 'Extracted commands, publisher/decoder logs, playlists, and caption text are in the documentation-examples artifact.'])
        summary = '\n'.join(lines) + '\n'
        (output / 'summary.md').write_text(summary)
        if os.environ.get('GITHUB_STEP_SUMMARY'):
            with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as handle:
                handle.write(summary)
    return 0 if report['ok'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
