#!/usr/bin/env python3
"""Check exported Rushls GAP fixtures with native Safari and hls.js.

Requires a Safari or Chrome WebDriver and a local hls.js bundle.
This probes completed audio playlists. It does not certify live LL-HLS or A/V switching.
Safari uses a fresh session per case to avoid permission state leaking across tests.
"""
import argparse
import functools
import http.server
import json
from pathlib import Path
import threading
import time
import urllib.error
import urllib.request

PAGE = b'''<!doctype html><meta charset="utf-8"><title>Rushls GAP test</title>
<script src="/hls.min.js"></script><button id="start">Start playback</button>
<audio id="video" controls></audio>
<script>
window.configure = (url, mode, seek, config) => {
 const v=document.querySelector('#video'); v.volume=0.01;
 const result=window.result={mode,version:mode==='native'?navigator.userAgent:Hls.version,events:[],logs:[],eventCount:0,fatal:null};
 const record=(event)=>{result.eventCount++;if(result.events.length<300)result.events.push(event);};
 const logger=Object.fromEntries(['log','debug','info','warn','error'].map(level=>[level,(...args)=>{if(result.logs.length<150)result.logs.push([level,...args.map(String)]);} ]));
 for(const name of ['playing','pause','waiting','stalled','seeking','seeked','ended','error'])
  v.addEventListener(name,()=>record({event:name,time:v.currentTime}));
 if(mode==='native') v.src=url;
 else {
  const hls=window.hls=new Hls({startPosition:seek,lowLatencyMode:true,debug:logger,...config});
  hls.on(Hls.Events.ERROR,(_,e)=>{record({event:e.details,time:v.currentTime});if(e.fatal)result.fatal=e.details;});
  hls.on(Hls.Events.BUFFER_CODECS,(_,tracks)=>{result.codecs=Object.fromEntries(Object.entries(tracks).map(([k,t])=>[k,{codec:t.codec,levelCodec:t.levelCodec,container:t.container}]));});
  hls.on(Hls.Events.FRAG_LOADED,(_,data)=>record({event:'fragment',sn:data.frag.sn,start:data.frag.start,url:data.frag.url}));
  hls.attachMedia(v); hls.loadSource(url);
 }
 // hls.js owns its configured start seek; issuing a second seek can race it.
 if(seek && mode==='native') v.addEventListener('loadedmetadata',()=>v.currentTime=seek,{once:true});
 document.querySelector('#start').onclick=()=>v.play().catch(e=>result.fatal=String(e));
};
window.snapshot = () => {
 const v=document.querySelector('#video');
 return {...result,time:v.currentTime,duration:v.duration,ended:v.ended,paused:v.paused,
 visibility:document.visibilityState,ready:v.readyState,error:v.error?.message,buffered:Array.from({length:v.buffered.length},(_,i)=>[v.buffered.start(i),v.buffered.end(i)])};
};
</script>'''


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map,
                      '.m3u8': 'application/vnd.apple.mpegurl', '.m4s': 'video/mp4'}

    def do_GET(self):
        self.server.requests.append({'path': self.path, 'range': self.headers.get('Range'), 'wall': time.monotonic()})
        if self.path == '/hls.min.js':
            self.send_response(200)
            self.send_header('Content-Type', 'text/javascript')
            self.send_header('Content-Length', str(len(self.server.hls_bundle)))
            self.end_headers()
            self.wfile.write(self.server.hls_bundle)
        elif self.path == '/player.html':
            self.send_response(200)
            self.send_header('Content-Type', 'text/html')
            self.send_header('Content-Length', str(len(PAGE)))
            self.end_headers()
            self.wfile.write(PAGE)
        else:
            super().do_GET()

    def log_message(self, *_):
        pass


def command(base, path, data=None, method=None):
    request = urllib.request.Request(base + path,
        data=None if data is None else json.dumps(data).encode(),
        headers={'Content-Type': 'application/json'}, method=method)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)['value']
    except urllib.error.HTTPError as error:
        raise RuntimeError(error.read().decode()) from error


def gap_end(playlist):
    total = 0.0
    gap = False
    for line in playlist.read_text().splitlines():
        if line == '#EXT-X-GAP':
            gap = True
        elif line.startswith('#EXTINF:'):
            total += float(line.split(':')[1].split(',')[0])
            if gap:
                return total
    raise ValueError(f'No gap in {playlist}')


def main():
    global PAGE
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manual-start', action='store_true', help='Wait for a visible Start playback click')
    parser.add_argument('--video', action='store_true', help='Render video and record presented frame timestamps')
    parser.add_argument('--fixtures', type=Path, required=True)
    parser.add_argument('--hls-js', type=Path, required=True)
    parser.add_argument('--webdriver', default='http://127.0.0.1:4445')
    parser.add_argument('--session', help='Reuse a dedicated test session')
    parser.add_argument('--browser', choices=('safari', 'chrome'), default='safari')
    parser.add_argument('--modes', nargs='+', choices=('native', 'hls.js'))
    parser.add_argument('--start', type=float, action='append', help='Explicit start position; repeat for multiple positions')
    parser.add_argument('--seek-at', type=float, help='Seek after playback reaches this position')
    parser.add_argument('--seek-to', type=float, help='Target for --seek-at')
    parser.add_argument('--expect-premature-end', action='store_true', help='Track a known failure; require playback to advance and end before the advertised endpoint')
    parser.add_argument('--repeat', type=int, default=1)
    parser.add_argument('--hls-config', type=json.loads, default={})
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--filter', default='*')
    parser.add_argument('--deadline', type=float, default=12)
    parser.add_argument('--playlist', default='index.m3u8')
    args = parser.parse_args()
    if (args.seek_at is None) != (args.seek_to is None):
        parser.error('--seek-at and --seek-to must be provided together')
    if args.video:
        PAGE = PAGE.replace(b'<audio id="video" controls></audio>', b'<video id="video" controls width="640" muted></video>')
        PAGE = PAGE.replace(b" const record=(event)", b""" result.frames=[];
 const frame=(wall, metadata)=>{
   if(result.frames.length<2000) result.frames.push({wall,mediaTime:metadata.mediaTime,currentTime:v.currentTime});
   v.requestVideoFrameCallback(frame);
 };
 v.requestVideoFrameCallback(frame);
 const record=(event)""")
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(Handler, directory=str(args.fixtures)))
    server.requests = []
    server.hls_bundle = args.hls_js.read_bytes()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f'http://127.0.0.1:{server.server_port}'
    capabilities = {'browserName': args.browser}
    if args.browser == 'chrome':
        capabilities['goog:chromeOptions'] = {'args': ['--headless=new', '--autoplay-policy=no-user-gesture-required']}
    session = args.session or command(args.webdriver, '/session', {'capabilities': {'alwaysMatch': capabilities}})['sessionId']
    base = args.webdriver + '/session/' + session
    results = []
    try:
        for playlist in sorted(args.fixtures.glob(args.filter + '/' + args.playlist)):
            timeline = playlist
            if '#EXTINF:' not in timeline.read_text():
                child = next(line for line in timeline.read_text().splitlines() if line and not line.startswith('#'))
                timeline = playlist.parent / child
            modes = args.modes or (('native', 'hls.js') if args.browser == 'safari' else ('hls.js',))
            positions = args.start or (0, gap_end(timeline) + .01)
            for mode in modes:
                for attempt, seek in ((attempt, seek) for attempt in range(args.repeat) for seek in positions):
                    # Reused Safari windows can revoke playback permission after
                    # navigation. Do not mistake that state for a decoder stall.
                    if results and args.browser == 'safari' and not args.session:
                        command(base, '', method='DELETE')
                        session = command(args.webdriver, '/session', {'capabilities': {'alwaysMatch': capabilities}})['sessionId']
                        base = args.webdriver + '/session/' + session
                    requests_start = len(server.requests)
                    command(base, '/url', {'url': origin + '/player.html'})
                    command(base, '/execute/sync', {'script': 'configure(...arguments)', 'args': [origin + '/' + playlist.parent.name + '/' + args.playlist, mode, seek, args.hls_config]})
                    if args.manual_start:
                        print('Ready for Start playback: ' + playlist.parent.name, flush=True)
                    elif args.video and args.browser == 'safari':
                        # Muted video can start without Safari's native WebDriver
                        # click, which can hang behind its automation overlay.
                        command(base, '/execute/sync', {'script': "document.querySelector('#start').click()", 'args': []})
                    else:
                        element = command(base, '/element', {'using': 'css selector', 'value': '#start'})
                        command(base, '/element/' + next(iter(element.values())) + '/click', {})
                        command(base, '/execute/sync', {'script': "document.querySelector('#start').click()", 'args': []})
                    started = time.monotonic()
                    playback_started = None
                    seek_performed = False
                    while True:
                        time.sleep(.25)
                        state = command(base, '/execute/sync', {'script': 'return snapshot()', 'args': []})
                        if playback_started is None and not state['paused']:
                            playback_started = time.monotonic()
                        if args.seek_at is not None and not seek_performed and state['time'] >= args.seek_at:
                            command(base, '/execute/sync', {'script': "document.querySelector('#video').currentTime=arguments[0]", 'args': [args.seek_to]})
                            seek_performed = True
                        budget_start = playback_started if playback_started is not None else started
                        budget = args.deadline if playback_started is not None else max(60, args.deadline)
                        if state['ended'] or state.get('fatal') or state.get('error') or time.monotonic() - budget_start >= budget:
                            break
                    expected_end = sum(float(line.split(':')[1].split(',')[0]) for line in timeline.read_text().splitlines() if line.startswith('#EXTINF:'))
                    state.update(expected_end=expected_end, fixture=playlist.parent.name, seek=seek, attempt=attempt, browser=args.browser, elapsed=time.monotonic()-started,
                                 ok=state['ended'] and state['time'] >= expected_end - .08 and state['time'] > seek + .1 and any(event['event'] == 'playing' for event in state['events']) and not state.get('fatal') and not state.get('error'))
                    state['seek_performed'] = seek_performed
                    state['ok'] = state['ok'] and (args.seek_at is None or seek_performed)
                    if args.video:
                        frames = state.get('frames', [])
                        state['presented_frames'] = len(frames)
                        state['maximum_frame_step'] = max((b['mediaTime'] - a['mediaTime'] for a, b in zip(frames, frames[1:])), default=None)
                        state['ok'] = state['ok'] and len(frames) > 10
                    state['requests'] = server.requests[requests_start:]
                    state['classification'] = ('passed' if state['ok'] else
                        'playback_permission_denied' if 'NotAllowedError' in str(state.get('fatal')) else
                        'startup_inconclusive' if not any(event['time'] > seek + .1 for event in state['events']) and state['time'] <= seek + .1 else
                        'paused_before_end' if state['paused'] and not state['ended'] else
                        'playback_failed')
                    state['expected_failure'] = args.expect_premature_end
                    state['expectation_met'] = state['ok'] if not args.expect_premature_end else (
                        state['ended'] and state['time'] > seek + .5
                        and state['time'] < expected_end - .5
                        and not state.get('fatal') and not state.get('error'))
                    results.append(state)
                    args.output.write_text(json.dumps(results, indent=2))
                    print(json.dumps(state), flush=True)
    finally:
        server.shutdown()
        if not args.session:
            command(base, '', method='DELETE')
    return int(not results or any(not result['expectation_met'] for result in results))


if __name__ == '__main__':
    raise SystemExit(main())
