#!/usr/bin/env python3
"""Measure audio/video sync of Rushls output in real browsers.

Publishes flash/beep streams over RTMP, then plays each one in Chrome
(hls.js and Shaka) and Safari (native HLS). Every second the
source shows one white frame and starts a 50 ms beep at the same instant.
The page timestamps flashes with requestVideoFrameCallback on the media
timeline, and the tool reports beep - flash.

Chrome is measured on the media timeline: an AudioWorklet hears the
element's audio, and requestVideoFrameCallback gives each flash's media time.
Safari is measured on the wall clock, as a viewer perceives it. WebKit
gives WebAudio silence for HLS, so the tool sends system output to a
loopback device for the run (restoring it afterwards) and `loopback.swift`
times beep onsets on the device clock. The page samples the picture on
every animation frame, because Safari stops firing
requestVideoFrameCallback for some streams that it still renders.

A browser adds a constant audio pipeline latency, so cases are compared
with the `base` case in the same player: no B-frames and no audio delay,
so its output needs no edit and no tfdt offset. A case passes when its
median offset is within the tolerance of that baseline.

Requires FFmpeg with libx264, Chrome with a matching ChromeDriver
(tools/install-chrome-driver.py), and, for Safari, `safaridriver --enable`,
the Swift toolchain, and a loopback device such as BlackHole 2ch. macOS may
ask for microphone access the first time the loopback records.
No CI minutes: everything runs locally.

    python3 tools/av-sync/check-av-sync.py --rushls target/debug/rushls \\
        --chromedriver target/chromedriver
"""
import argparse
import http.server
import json
import os
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

# Pinned by Subresource Integrity, so a changed CDN file fails to load
# rather than silently changing what is measured. --hls-js replaces the
# first with a local build, as CI's patched player.
HLS_JS = ('https://cdn.jsdelivr.net/npm/hls.js@1.7.3/dist/hls.min.js',
          'sha384-cciJ0zi8d1uMKC2zJd7jvPY4HQt7W4ByUI/FlMkltvBi31aW61rcpVBhpmW8/NwX')
SHAKA = ('https://cdn.jsdelivr.net/npm/shaka-player@5.2.12/dist/shaka-player.compiled.js',
         'sha384-dC+HTy8lAr0Y9PZngIX8OfKQQ3JnR04Xs2ut3jQ1jxUXT1TZ23BTbPbTjM2cj4H0')

# name -> (B-frames, audio delay in seconds). `base` needs no edit and no
# tfdt offset, so every player should agree on it.
CASES = {
    'base': (0, 0.0),
    'late': (0, 0.133),
    'bframes': (3, 0.0),
    'both': (3, 0.133),
}
# hls.js in Safari is left out: it stalls on these short ended streams
# (bufferSeekOverHole, then bufferStalledError), before and after the tfdt
# change alike. See TODO.md.
PLAYERS = {
    'chrome': ['hls.js', 'shaka'],
    'safari': ['native'],
}

PAGE = f'''<!doctype html><meta charset="utf-8"><title>Rushls A/V sync</title>
HLS_SCRIPT<script src="{SHAKA[0]}" integrity="{SHAKA[1]}" crossorigin="anonymous"></script>
<button id="go">start</button><video id="v" playsinline width="320" height="180"></video>
<script>
const WORKLET = `registerProcessor('beep', class extends AudioWorkletProcessor {{
  constructor() {{ super(); this.quiet = 0; }}
  process(inputs) {{
    const channel = inputs[0][0];
    if (channel) for (let i = 0; i < channel.length; i++) {{
      // An onset is the first loud sample after at least half a second of
      // quiet, so the beep's own oscillation never re-triggers it.
      if (Math.abs(channel[i]) > 0.2) {{
        if (this.quiet > sampleRate / 2) this.port.postMessage(currentTime + i / sampleRate);
        this.quiet = 0;
      }} else this.quiet++;
    }}
    return true;
  }}
}});`;
window.result = {{flashes: [], beeps: [], errors: [], playing: false}};
window.prepare = (mode, url, loopback) => document.getElementById('go').onclick = async () => {{
  const v = document.getElementById('v');
  try {{
    const canvas = Object.assign(document.createElement('canvas'), {{width: 8, height: 8}});
    const draw = canvas.getContext('2d', {{willReadFrequently: true}});
    let lit = false;
    // True on the first bright picture after a dark one.
    const flash = () => {{
      draw.drawImage(v, 0, 0, 8, 8);
      const pixels = draw.getImageData(0, 0, 8, 8).data;
      let sum = 0;
      for (let i = 0; i < pixels.length; i += 4) sum += pixels[i];
      const bright = sum / (pixels.length / 4) > 128, onset = bright && !lit;
      lit = bright;
      return onset;
    }};
    if (loopback) {{
      // Unix seconds of the animation frame that first shows each flash;
      // the loopback times beeps on the same clock.
      const tick = now => {{
        if (v.readyState >= 2 && flash()) result.flashes.push((performance.timeOrigin + now) / 1000);
        requestAnimationFrame(tick);
      }};
      requestAnimationFrame(tick);
    }} else {{
      const context = new AudioContext();
      await context.audioWorklet.addModule(URL.createObjectURL(new Blob([WORKLET], {{type: 'text/javascript'}})));
      const node = new AudioWorkletNode(context, 'beep');
      // Read both clocks together when a beep arrives: the media time of the
      // beep is the element's time less how long ago the worklet heard it.
      node.port.onmessage = ({{data}}) => {{
        if (!v.paused) result.beeps.push(v.currentTime - (context.currentTime - data));
      }};
      context.createMediaElementSource(v).connect(node).connect(context.destination);
      await context.resume();
      const frame = (_, meta) => {{
        if (flash()) result.flashes.push(meta.mediaTime);
        v.requestVideoFrameCallback(frame);
      }};
      v.requestVideoFrameCallback(frame);
    }}
    v.addEventListener('playing', () => result.playing = true);
    v.addEventListener('error', () => result.errors.push(String(v.error?.message || v.error?.code)));
    if (mode === 'hls.js') {{
      const hls = new Hls({{startPosition: 0}});
      hls.on(Hls.Events.ERROR, (_, e) => e.fatal && result.errors.push(e.details));
      hls.loadSource(url); hls.attachMedia(v);
    }} else if (mode === 'shaka') {{
      shaka.polyfill.installAll();
      const player = new shaka.Player();
      await player.attach(v);
      player.addEventListener('error', e => result.errors.push(String(e.detail?.code)));
      await player.load(url, 0);
    }} else v.src = url;
    await v.play();
  }} catch (error) {{ result.errors.push(String(error)); }}
}};
</script>'''


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


class Proxy(http.server.BaseHTTPRequestHandler):
    """Serves the page and relays /live/ to Rushls, so the media is
    same-origin and WebAudio may read it."""
    origin = ''
    page = b''
    hls_bundle = None

    def do_GET(self):
        if self.path == '/hls.js' and self.hls_bundle is not None:
            body, status, kind = self.hls_bundle, 200, 'text/javascript'
        elif not self.path.startswith('/live/'):
            body, status, kind = self.page, 200, 'text/html'
        else:
            try:
                with urllib.request.urlopen(self.origin + self.path, timeout=30) as reply:
                    body, status = reply.read(), reply.status
                    kind = reply.headers.get('Content-Type', 'application/octet-stream')
            except urllib.error.HTTPError as error:
                body, status, kind = error.read(), error.code, 'text/plain'
        self.send_response(status)
        self.send_header('Content-Type', kind)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def wait_for_port(port, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            socket.create_connection(('127.0.0.1', port), timeout=1).close()
            return
        except OSError:
            time.sleep(0.2)
    sys.exit(f'nothing listened on port {port}')


def webdriver(base, path, data=None, method=None):
    request = urllib.request.Request(
        base + path,
        data=None if data is None else json.dumps(data).encode(),
        method=method or ('GET' if data is None else 'POST'),
        headers={'Content-Type': 'application/json'},
    )
    with urllib.request.urlopen(request, timeout=60) as reply:
        return json.load(reply)['value']


def publish(ffmpeg, rtmp, name, bframes, delay, seconds):
    video = "color=c=black:s=320x180:r=30,drawbox=c=white:t=fill:enable='eq(mod(n\\,30)\\,0)'"
    # aevalsrc's t starts at the delayed input's start, so `t + delay` is the
    # absolute time: beeps land on whole seconds, exactly with the flashes.
    audio = f"aevalsrc='if(lt(mod(t+{delay}\\,1)\\,0.05)\\,sin(2*PI*1000*t)\\,0)':s=48000:c=mono"
    return subprocess.Popen(
        [ffmpeg, '-hide_banner', '-loglevel', 'error',
         '-re', '-f', 'lavfi', '-i', video,
         '-re', '-itsoffset', str(delay), '-f', 'lavfi', '-i', audio,
         '-t', str(seconds), '-c:v', 'libx264', '-preset', 'veryfast', '-bf', str(bframes),
         '-g', '30', '-pix_fmt', 'yuv420p', '-c:a', 'aac', '-b:a', '128k',
         '-f', 'flv', f'{rtmp}/live/{name}'],
    )


def measure(driver, page, mode, name, listen, loopback=None):
    session = None
    listener = None
    try:
        if loopback:
            tool, device = loopback
            listener = subprocess.Popen([tool, 'listen', device, str(listen + 4)],
                                        stdout=subprocess.PIPE, text=True)
        capabilities = driver['capabilities']
        session = webdriver(driver['url'], '/session', {'capabilities': {'alwaysMatch': capabilities}})['sessionId']
        base = f"{driver['url']}/session/{session}"
        webdriver(base, '/url', {'url': page})
        webdriver(base, '/execute/sync', {'script': 'prepare(arguments[0], arguments[1], arguments[2])',
                                          'args': [mode, f'/live/{name}/index.m3u8', bool(loopback)]})
        button = webdriver(base, '/element', {'using': 'css selector', 'value': '#go'})
        webdriver(base, f"/element/{next(iter(button.values()))}/click", {})
        time.sleep(listen)
        result = webdriver(base, '/execute/sync', {'script': 'return result', 'args': []})
        if listener:
            result['beeps'] = [float(line) for line in listener.communicate(timeout=30)[0].split()]
        return result
    finally:
        if listener and listener.poll() is None:
            listener.kill()
        if session:
            webdriver(driver['url'], f'/session/{session}', method='DELETE')


def offsets(result):
    """beep - nearest flash, in milliseconds, skipping the first flash: it
    can fall in startup, before audio plays."""
    flashes = result['flashes']
    found = []
    for beep in result['beeps']:
        if not flashes or beep < flashes[0] + 0.5:
            continue
        flash = min(flashes, key=lambda time: abs(time - beep))
        if abs(beep - flash) < 0.4:
            found.append((beep - flash) * 1000)
    return found


def run_browser(browser, driver, page, loopback, args, report):
    """Measures every case in each of the browser's players; True on failure."""
    failed = False
    for mode in PLAYERS[browser]:
        if args.players and mode not in args.players.split(','):
            continue
        rows = {}
        for name in args.cases:
            result = measure(driver, page, mode, name, args.listen, loopback)
            found = offsets(result)
            if len(found) < 3:
                # The first Safari session of a run sometimes plays without
                # being measured; one fresh session is enough to recover.
                result = measure(driver, page, mode, name, args.listen, loopback)
                found = offsets(result)
            rows[name] = {'offsets_ms': found, 'errors': result['errors'],
                          'flashes': result['flashes'], 'beeps': result['beeps'],
                          'playing': result['playing'],
                          'median_ms': statistics.median(found) if found else None}
        report[f'{browser}/{mode}'] = rows
        baseline = rows.get('base', {}).get('median_ms')
        for name, row in rows.items():
            median = row['median_ms']
            if median is None or baseline is None:
                verdict, failed = 'NO DATA', True
            else:
                drift = median - baseline
                ok = abs(drift) <= args.tolerance_ms
                failed |= not ok
                verdict = f"{'ok  ' if ok else 'FAIL'} {drift:+7.1f} ms vs base"
            spread = (f"{min(row['offsets_ms']):+.0f}..{max(row['offsets_ms']):+.0f}"
                      if row['offsets_ms'] else '-')
            median_text = f'{median:+7.1f}' if median is not None else '      -'
            print(f"{browser:7} {mode:7} {name:8} median {median_text} ms "
                  f"(n={len(row['offsets_ms'])}, {spread})  {verdict}"
                  + (f"  errors: {row['errors']}" if row['errors'] else ''), flush=True)
    return failed


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--rushls', type=Path, default=Path('target/debug/rushls'))
    parser.add_argument('--ffmpeg', default='ffmpeg')
    parser.add_argument('--chromedriver', type=Path, default=Path('target/chromedriver'))
    parser.add_argument('--chrome', help='Chrome binary; by default ChromeDriver finds it')
    parser.add_argument('--headless', action='store_true', help='run Chrome headless, as in CI')
    parser.add_argument('--hls-js', type=Path, help='local hls.js build instead of the pinned release')
    parser.add_argument('--browsers', default='chrome,safari')
    parser.add_argument('--players', help='comma-separated subset, e.g. native,shaka')
    parser.add_argument('--cases', default=','.join(CASES),
                        type=lambda text: [name for name in text.split(',') if name in CASES],
                        help='comma-separated subset; keep `base`, every verdict compares to it')
    parser.add_argument('--seconds', type=int, default=14, help='published media per case')
    parser.add_argument('--listen', type=float, default=12, help='playback measured per player')
    parser.add_argument('--tolerance-ms', type=float, default=15)
    parser.add_argument('--loopback-device', default='BlackHole 2ch',
                        help='output device that Safari plays into while it is measured')
    parser.add_argument('--output', type=Path, help='write all measurements as JSON')
    args = parser.parse_args()

    work = Path(tempfile.mkdtemp(prefix='rushls-av-sync-'))
    rtmp_port, srt_port, http_port, chrome_port, safari_port = (free_port() for _ in range(5))
    # A long window keeps the ended streams playable while every player runs.
    # SRT is unused, but would otherwise claim its default public port.
    (work / 'rushls.toml').write_text(
        f'[ingest.rtmp]\nlisten = "127.0.0.1:{rtmp_port}"\n'
        f'[ingest.srt]\nlisten = "127.0.0.1:{srt_port}"\n'
        f'[http]\nlisten = "127.0.0.1:{http_port}"\n'
        '[hls]\nwindow = "10m"\n'
    )
    processes = []
    try:
        processes.append(subprocess.Popen(
            [str(args.rushls), '--config', str(work / 'rushls.toml')],
            stdout=open(work / 'rushls.log', 'w'), stderr=subprocess.STDOUT))
        wait_for_port(rtmp_port)
        wait_for_port(http_port)
        publishers = [publish(args.ffmpeg, f'rtmp://127.0.0.1:{rtmp_port}', name, *CASES[name], args.seconds)
                      for name in args.cases]
        if any(process.poll() is not None for process in processes):
            sys.exit('Rushls exited; see ' + str(work / 'rushls.log'))
        if any(publisher.wait() for publisher in publishers):
            sys.exit('a publisher failed; see ' + str(work / 'rushls.log'))

        Proxy.origin = f'http://127.0.0.1:{http_port}'
        if args.hls_js:
            Proxy.hls_bundle = args.hls_js.read_bytes()
            hls_script = '<script src="/hls.js"></script>'
        else:
            hls_script = f'<script src="{HLS_JS[0]}" integrity="{HLS_JS[1]}" crossorigin="anonymous"></script>'
        Proxy.page = PAGE.replace('HLS_SCRIPT', hls_script).encode()
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Proxy)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        page = f'http://127.0.0.1:{server.server_address[1]}/'

        drivers = {}
        browsers = args.browsers.split(',')
        if 'chrome' in browsers:
            processes.append(subprocess.Popen([str(args.chromedriver), f'--port={chrome_port}'],
                                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
            options = {'args': ['--autoplay-policy=no-user-gesture-required', '--mute-audio']
                       + (['--headless=new'] if args.headless else [])}
            if args.chrome:
                options['binary'] = args.chrome
            drivers['chrome'] = {'url': f'http://127.0.0.1:{chrome_port}', 'capabilities': {
                'browserName': 'chrome', 'goog:chromeOptions': options}}
        if 'safari' in browsers:
            processes.append(subprocess.Popen(['safaridriver', '-p', str(safari_port)],
                                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
            drivers['safari'] = {'url': f'http://127.0.0.1:{safari_port}', 'capabilities': {
                'browserName': 'safari', 'webkit:alwaysAllowAutoplay': True}}
        time.sleep(2)

        loopback, restore = None, None
        if 'safari' in drivers:
            tool = work / 'loopback'
            subprocess.run(['swiftc', '-O', str(Path(__file__).with_name('loopback.swift')), '-o', str(tool)],
                           check=True)
            loopback = (str(tool), args.loopback_device)
            restore = subprocess.run([str(tool), 'get'], check=True, capture_output=True,
                                     text=True).stdout.strip()

        report, failed = {}, False
        for browser, driver in drivers.items():
            if browser != 'safari':
                failed |= run_browser(browser, driver, page, None, args, report)
                continue
            subprocess.run([loopback[0], 'set', args.loopback_device], check=True)
            try:
                failed |= run_browser(browser, driver, page, loopback, args, report)
            finally:
                # Always give the user their audio output back.
                subprocess.run([loopback[0], 'set', restore], check=True)
        if args.output:
            args.output.write_text(json.dumps(report, indent=2))
        sys.exit(1 if failed else 0)
    finally:
        for process in reversed(processes):
            process.terminate()


if __name__ == '__main__':
    main()
