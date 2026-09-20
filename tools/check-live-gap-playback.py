#!/usr/bin/env python3
"""Probe the opt-in live_av_gap_browser_origin Rust test through its real HTTP origin.

Start the ignored Rust test with RUSHLS_GAP_LIVE_READY pointing to --ready.
This checks browser progress and switching, not perceptual audio quality.
"""
import argparse
import functools
import http.server
import importlib.util
import json
import re
import urllib.parse
from pathlib import Path
import threading
import time
import urllib.request

spec = importlib.util.spec_from_file_location('gap_probe', Path(__file__).with_name('check-gap-playback.py'))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)

EXTRA = '''
window.instrument = () => {
 performance.setResourceTimingBufferSize(3000);
 const v=document.querySelector('#video');
 window.frames={count:0,maxClockDifference:0,lastClockDifference:0,over100ms:0,presented:[]};
 const frame=(_,m)=>{frames.count++;if(frames.presented.length<2000)frames.presented.push({mediaTime:m.mediaTime,wall:performance.now(),clock:v.currentTime});frames.lastClockDifference=Math.abs(v.currentTime-m.mediaTime);frames.maxClockDifference=Math.max(frames.maxClockDifference,frames.lastClockDifference);if(frames.lastClockDifference>.1)frames.over100ms++;v.requestVideoFrameCallback(frame);};
 if(v.requestVideoFrameCallback)v.requestVideoFrameCallback(frame);
 window.transitions=[];window.levels=[];
 if(v.audioTracks)v.audioTracks.addEventListener('change',()=>transitions.push({event:'nativeAudioChanged',time:v.currentTime,enabled:Array.from(v.audioTracks).map((t,i)=>t.enabled?i:null).filter(i=>i!==null)}));
 if(window.hls) {
  v.addEventListener('playing',()=>{hls.loadLevel=0;},{once:true});
  for(const event of [Hls.Events.AUDIO_TRACK_SWITCHED,Hls.Events.LEVEL_SWITCHED])
   hls.on(event,(_,data)=>transitions.push({event,time:v.currentTime,id:data.id,level:data.level}));
  hls.on(Hls.Events.LEVEL_LOADED,(_,data)=>levels.push({live:data.details.live,start:data.details.startSN,end:data.details.endSN,parts:data.details.partList?.length}));
 }
};
window.switchTrack = (kind,index) => {
 const v=document.querySelector('#video');
 if(window.hls) {
  if(kind==='audio'){if(hls.audioTracks.length<2)return false;hls.audioTrack=index;}
  else {if(hls.levels.length<2)return false;if(hls.currentLevel===index)index=1-index;hls.nextLevel=index;}
 } else {
  if(kind!=='audio'||!v.audioTracks||v.audioTracks.length<2)return false;
  for(let i=0;i<v.audioTracks.length;i++)v.audioTracks[i].enabled=i===index;
 }
 return {index};
};
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ready', type=Path, required=True)
    parser.add_argument('--hls-js', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--browser', choices=('safari', 'chrome'), default='chrome')
    parser.add_argument('--switches', choices=('all','audio','none'), default='all')
    parser.add_argument('--manual-start', action='store_true', help='Wait for a visible Play-button click instead of WebDriver clicks')
    parser.add_argument('--mode', choices=('native', 'hls.js'), default='hls.js')
    parser.add_argument('--webdriver', default='http://127.0.0.1:4446')
    args = parser.parse_args()
    deadline = time.monotonic() + 60
    while not args.ready.exists():
        if time.monotonic() > deadline:
            raise TimeoutError('Rust origin did not become ready')
        time.sleep(.1)
    url = args.ready.read_text().strip()
    probe.PAGE = probe.PAGE.replace(b'if(result.logs.length<150)result.logs.push([level,...args.map(String)]);',b'if(result.logs.length>=400)result.logs.shift();result.logs.push([level,...args.map(String)]);').replace(b'<audio id="video" controls></audio>', b'<video id="video" playsinline controls width="640" height="360"></video>') + ('<script>'+EXTRA+'</script>').encode()
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(probe.Handler, directory=str(args.ready.parent)))
    server.requests = []
    server.hls_bundle = args.hls_js.read_bytes()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    capabilities = {'browserName': args.browser}
    if args.browser == 'chrome':
        capabilities['goog:chromeOptions'] = {'args': ['--headless=new', '--autoplay-policy=no-user-gesture-required']}
    session = probe.command(args.webdriver, '/session', {'capabilities': {'alwaysMatch': capabilities}})['sessionId']
    base = args.webdriver + '/session/' + session
    report = {'browser': args.browser, 'mode': args.mode, 'samples': [], 'switches': [], 'playlists': {}}
    try:
        args.ready.with_suffix('.start').touch()
        while True:
            try:
                with urllib.request.urlopen(url, timeout=1) as response:
                    master = response.read().decode()
                break
            except Exception:
                if time.monotonic() > deadline:
                    raise TimeoutError('Publication did not become playable')
                time.sleep(.1)
        report['master'] = master
        probe.command(base, '/url', {'url': f'http://127.0.0.1:{server.server_port}/player.html'})
        probe.command(base, '/execute/sync', {'script': 'configure(...arguments);instrument()', 'args': [url,args.mode,0,{'startLevel':0}]})
        args.ready.with_suffix('.browser').write_text('ready')
        if not args.manual_start:
            element = probe.command(base, '/element', {'using': 'css selector', 'value': '#start'})
            probe.command(base, '/element/'+next(iter(element.values()))+'/click', {})
            probe.command(base, '/execute/sync', {'script': "document.querySelector('#start').click()", 'args': []})
        switches = [(7.8,'audio',1),(10,'video',1),(13.2,'audio',0),(16,'video',0)]
        switches = [entry for entry in switches if args.switches == 'all' or (args.switches == 'audio' and entry[1] == 'audio')]
        started = time.monotonic()
        playback_started = None
        # Manual activation must not consume the playback observation budget.
        # A separate startup deadline still catches autoplay/automation failures.
        while time.monotonic() - (playback_started or started) < (60 if playback_started is None else 55 if args.manual_start else 38):
            time.sleep(.25)
            state = probe.command(base, '/execute/sync', {'script': '''return {...snapshot(),controller:window.hls?{main:hls.streamController.state,parts:hls.latestLevelDetails?.partList?.slice(-12).map(p=>({sn:p.fragment.sn,index:p.index,loaded:p.loaded,start:p.start,end:p.end})),last:hls.streamController.fragPrevious?.sn,audio:hls.audioStreamController.state}:null}''' , 'args': []})
            if playback_started is None and not state['paused']:
                playback_started = time.monotonic()
                report['startup_seconds'] = playback_started - started
            report['state'] = state
            args.output.write_text(json.dumps(report,indent=2))
            report['samples'].append({'wall':time.monotonic()-started, **{k:state[k] for k in ('time','paused','ready','buffered','visibility')}})
            if switches and state['time'] >= switches[0][0]:
                _,kind,index = switches.pop(0)
                changed = probe.command(base, '/execute/sync', {'script':'return switchTrack(...arguments)','args':[kind,index]})
                report['switches'].append({'kind':kind,'time':state['time'],'requested':bool(changed),'index':changed['index'] if changed else index})
            if state['ended'] or state.get('fatal') or state.get('error'):
                break
        for line in master.splitlines():
            relative = None
            if line.startswith('#EXT-X-MEDIA:'):
                match = re.search(r'URI="([^"]+)"',line)
                if match: relative=match[1]
            elif line and not line.startswith('#'): relative=line
            if relative:
                with urllib.request.urlopen(urllib.parse.urljoin(url,relative),timeout=3) as response:
                    report['playlists'][relative]=response.read().decode()
        report['state'] = state
        report.update(probe.command(base,'/execute/sync',{'script':'''return {frames,transitions,levels,requests:performance.getEntriesByType('resource').map(e=>e.name),audioTracks:window.hls?.audioTracks.map(t=>({id:t.id,name:t.name})),videoLevels:window.hls?.levels.length}''','args':[]}))
        switched = all(
            any(t['time'] >= request['time'] and (
                (request['kind']=='audio' and t['event']=='hlsAudioTrackSwitched' and t['id']==request['index']) or
                (request['kind']=='video' and t['event']=='hlsLevelSwitched' and t['level']==request['index']) or
                (request['kind']=='audio' and t['event']=='nativeAudioChanged' and request['index'] in t['enabled'])
            ) for t in report['transitions'])
            for request in report['switches'] if request['requested'])
        report['rewinds'] = [{'from':a['time'],'to':b['time']} for a,b in zip(report['samples'],report['samples'][1:]) if b['time'] < a['time']-.1]
        report['ok'] = not report['rewinds'] and switched and state['ended'] and state['time'] >= 23.8 and not state.get('fatal') and not state.get('error') and report['frames']['count'] > 100
        # Hidden or denied playback is a harness failure, not evidence about GAP handling.
        progressed = max((sample['time'] for sample in report['samples']), default=0) > .5
        report['result'] = 'passed' if report['ok'] else 'failed' if progressed else 'inconclusive'
        if not progressed:
            report['reason'] = 'Playback did not advance; inspect visibility and autoplay policy before judging media.'
        args.output.write_text(json.dumps(report,indent=2))
        print(json.dumps({'ok': report['ok'], 'result': report['result'], 'frames': {k:v for k,v in report['frames'].items() if k != 'presented'}, 'switches': report['switches']}))
    finally:
        args.ready.with_suffix('.done').touch()
        server.shutdown()
        probe.command(base,'',method='DELETE')
    return 2 if report['result'] == 'inconclusive' else int(not report['ok'])


if __name__ == '__main__':
    raise SystemExit(main())
