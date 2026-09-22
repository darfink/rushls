#!/usr/bin/env python3
"""Probe the opt-in live_av_gap_browser_origin Rust test through its real HTTP origin.

Start the ignored Rust test with RUSHLS_GAP_LIVE_READY pointing to --ready.
This checks browser progress and switching, not perceptual audio quality.
"""
import argparse
import functools
import http.server
import hashlib
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
window.traceConfig = (fixedLevel, trace) => {
 window.endlistTrace=[];
 window.traceDropped=0;
 window.traceRecord=row=>{if(endlistTrace.length<6000)endlistTrace.push({wall:performance.now(),...row});else traceDropped++;};
 const config={startLevel:fixedLevel===null?0:fixedLevel};
 if(trace){
  const Loader=Hls.DefaultConfig.loader;
  config.loader=class extends Loader {
   load(context,config,callbacks){
    const success=callbacks.onSuccess;
    super.load(context,config,{...callbacks,onSuccess:(response,...rest)=>{
     if(typeof response.data==='string'&&response.data.startsWith('#EXTM3U'))
      traceRecord({event:'playlist',url:response.url||context.url,text:response.data});
     success(response,...rest);
    }});
   }
  };
 }
 return config;
};
window.instrument = (fixedLevel, trace) => {
 performance.setResourceTimingBufferSize(3000);
 const v=document.querySelector('#video');
 window.frames={count:0,maxClockDifference:0,lastClockDifference:0,over100ms:0,presented:[]};
 const frame=(_,m)=>{frames.count++;if(frames.presented.length<2000)frames.presented.push({mediaTime:m.mediaTime,wall:performance.now(),clock:v.currentTime});frames.lastClockDifference=Math.abs(v.currentTime-m.mediaTime);frames.maxClockDifference=Math.max(frames.maxClockDifference,frames.lastClockDifference);if(frames.lastClockDifference>.1)frames.over100ms++;v.requestVideoFrameCallback(frame);};
 if(v.requestVideoFrameCallback)v.requestVideoFrameCallback(frame);
 window.transitions=[];window.levels=[];
 if(v.audioTracks)v.audioTracks.addEventListener('change',()=>transitions.push({event:'nativeAudioChanged',time:v.currentTime,selected:Array.from(v.audioTracks).map((t,i)=>t.enabled?i:null).filter(i=>i!==null)}));
 if(window.hls) {
  if(fixedLevel!==null){
   hls.on(Hls.Events.MANIFEST_PARSED,()=>{hls.loadLevel=fixedLevel;hls.autoLevelCapping=fixedLevel;});
  } else v.addEventListener('playing',()=>{hls.loadLevel=0;},{once:true});
  if(trace){
   for(const event of [Hls.Events.FRAG_LOADING,Hls.Events.FRAG_LOADED,Hls.Events.FRAG_BUFFERED])
    hls.on(event,(_,data)=>traceRecord({event,type:data.frag.type,sn:data.frag.sn,part:data.part?.index,url:data.part?.url||data.frag.url,start:data.frag.start,end:data.frag.end,streams:structuredClone(data.frag.elementaryStreams),tracker:hls.streamController.fragmentTracker.getState(data.frag),next:(data.frag.type==='audio'?hls.audioStreamController:hls.streamController).nextLoadPosition,loadingParts:(data.frag.type==='audio'?hls.audioStreamController:hls.streamController).loadingParts}));
   for(const [kind,controller] of [['video',hls.streamController],['audio',hls.audioStreamController]]){
    const original=controller.getFragmentAtPosition;
    controller.getFragmentAtPosition=function(bufferEnd,end,details){
     const before={kind,event:'selection',bufferEnd,end,live:details.live,previous:this.fragPrevious?.sn,loadingParts:this.loadingParts,
      hint:details.fragmentHint?.sn,parts:details.partList?.slice(-12).map(p=>({sn:p.fragment.sn,index:p.index,start:p.start,end:p.end,loaded:p.loaded,gap:p.gap,url:p.url}))};
     const result=original.call(this,bufferEnd,end,details);
     if(end>=23)traceRecord({...before,selected:result?.sn??null,trackerState:result?this.fragmentTracker.getState(result):null});
     return result;
    };
   }
  }
  for(const event of [Hls.Events.AUDIO_TRACK_SWITCHED,Hls.Events.LEVEL_SWITCHED])
   hls.on(event,(_,data)=>transitions.push({event,time:v.currentTime,id:data.id,level:data.level,selected:event===Hls.Events.AUDIO_TRACK_SWITCHED?hls.audioTrack:hls.currentLevel}));
  hls.on(Hls.Events.LEVEL_LOADED,(_,data)=>levels.push({live:data.details.live,start:data.details.startSN,end:data.details.endSN,parts:data.details.partList?.length}));
 }
};
// Media-element ranges intersect audio and video. Retain each SourceBuffer
// separately so a missing append cannot be mistaken for missing input media.
window.bufferState = () => {
 if(!window.hls)return null;
 const controller=hls.bufferController;
 const ranges=buffer=>Array.from({length:buffer.length},(_,i)=>[buffer.start(i),buffer.end(i)]);
 return {readyState:controller?.mediaSource?.readyState,duration:controller?.mediaSource?.duration,
  tracks:Object.fromEntries(Object.entries(controller?.tracks||{}).map(([kind,track])=>{
   const b=track.buffer;
   return [kind,b?{buffered:ranges(b.buffered),updating:b.updating,timestampOffset:b.timestampOffset,
    appendWindowStart:b.appendWindowStart,appendWindowEnd:b.appendWindowEnd}:null];
  }))};
};
window.switchTrack = (kind,index) => {
 const v=document.querySelector('#video');
 const transitionStart=transitions.length;
 let previous;
 if(window.hls) {
  const tracks=kind==='audio'?hls.audioTracks:hls.levels;
  previous=kind==='audio'?hls.audioTrack:hls.currentLevel;
  // ABR can select the planned video target before the scheduled request.
  // Select the other fixture variant so this still exercises a real switch.
  if(kind==='video'&&previous===index)index=1-index;
  if(index<0||index>=tracks.length)return {accepted:false,reason:'unavailable track',transitionStart};
 } else {
  if(kind!=='audio'||!v.audioTracks||index<0||index>=v.audioTracks.length)
   return {accepted:false,reason:'unavailable track',transitionStart};
  previous=Array.from(v.audioTracks).findIndex(t=>t.enabled);
 }
 if(previous===index)return {accepted:false,reason:'target already selected',previous,transitionStart};
 if(window.hls) {
  if(kind==='audio')hls.audioTrack=index;
  else hls.nextLevel=index;
 } else {
  for(let i=0;i<v.audioTracks.length;i++)v.audioTracks[i].enabled=i===index;
 }
 return {accepted:true,previous,transitionStart,index};
};
'''


def switch_schedule(mode):
    entries = [(7.8, 'audio', 1), (10, 'video', 1), (13.2, 'audio', 0), (16, 'video', 0)]
    return [dict(at=at, kind=kind, index=index, attempted=False)
            for at, kind, index in entries
            if mode == 'all' or (mode == 'audio' and kind == 'audio')]


def validate_switches(requests, transitions):
    """Require every scheduled request and a subsequent matching selection event.

    Event indices prevent initial selection or an earlier switch from satisfying
    a later request, even when the media clock rewinds.
    """
    for position, request in enumerate(requests):
        request['observed'] = False
        if not request.get('attempted'):
            request['failure'] = 'scheduled switch not attempted'
            continue
        if not request.get('accepted'):
            request['failure'] = request.get('reason', 'switch rejected')
            continue
        if request.get('previous') in (None, -1, request['index']):
            request['failure'] = 'no distinct previous selection'
            continue
        start = request.get('transitionStart')
        if not isinstance(start, int) or start < 0:
            request['failure'] = 'missing transition boundary'
            continue
        end = next((later['transitionStart'] for later in requests[position + 1:]
                    if later['kind'] == request['kind'] and later.get('attempted')
                    and isinstance(later.get('transitionStart'), int)), len(transitions))
        for event in transitions[start:end]:
            if request['kind'] == 'audio':
                matched = (event['event'] == 'hlsAudioTrackSwitched'
                           and event.get('id') == request['index']
                           and event.get('selected') == request['index']) or (
                               event['event'] == 'nativeAudioChanged'
                               and event.get('selected') == [request['index']])
            else:
                matched = (event['event'] == 'hlsLevelSwitched'
                           and event.get('level') == request['index']
                           and event.get('selected') == request['index'])
            if matched:
                request['observed'] = True
                request.pop('failure', None)
                break
        if not request['observed']:
            request['failure'] = 'requested selection not observed'
    return all(request['observed'] for request in requests)


def validate_fixed_level(level, transitions):
    if level is None:
        return True
    observed = [t['level'] for t in transitions if t['event'] == 'hlsLevelSwitched']
    return bool(observed) and all(selected == level for selected in observed)


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
    parser.add_argument('--fixed-level', type=int, choices=(0, 1), help='Pin the fixture video variant; excludes scheduled video switches')
    parser.add_argument('--trace-endlist', action='store_true', help='Capture playlist responses and player fragment-selection decisions')
    parser.add_argument('--trace-appends', action='store_true', help='Capture bounded MSE append bytes and buffer changes for replay')
    args = parser.parse_args()
    if args.fixed_level is not None and (args.mode != 'hls.js' or args.switches == 'all'):
        parser.error('--fixed-level requires hls.js and --switches none or audio')
    if args.trace_appends and args.mode != 'hls.js':
        parser.error('--trace-appends requires hls.js')
    if args.trace_endlist and args.mode != 'hls.js':
        parser.error('--trace-endlist requires hls.js')
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
    session_info = probe.command(args.webdriver, '/session', {'capabilities': {'alwaysMatch': capabilities}})
    session = session_info['sessionId']
    base = args.webdriver + '/session/' + session
    report = {'browser': args.browser, 'mode': args.mode, 'samples': [],
              'switches': switch_schedule(args.switches), 'playlists': {},
              'hls_sha256': hashlib.sha256(server.hls_bundle).hexdigest(),
              'capabilities': session_info.get('capabilities', {}),
              'fixed_level': args.fixed_level, 'trace_endlist': args.trace_endlist}
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
        if args.trace_appends:
            script = Path(__file__).with_name('trace-mse-appends.js').read_text()
            probe.command(base, '/execute/sync', {'script': script + '\ninstallAppendTrace();', 'args': []})
        probe.command(base, '/execute/sync', {'script': 'configure(arguments[0],arguments[1],0,traceConfig(arguments[2],arguments[3]));instrument(arguments[2],arguments[3])', 'args': [url,args.mode,args.fixed_level,args.trace_endlist]})
        args.ready.with_suffix('.browser').write_text('ready')
        if not args.manual_start:
            element = probe.command(base, '/element', {'using': 'css selector', 'value': '#start'})
            probe.command(base, '/element/'+next(iter(element.values()))+'/click', {})
            probe.command(base, '/execute/sync', {'script': "document.querySelector('#start').click()", 'args': []})
        switches = list(report['switches'])
        started = time.monotonic()
        playback_started = None
        # Manual activation must not consume the playback observation budget.
        # A separate startup deadline still catches autoplay/automation failures.
        while time.monotonic() - (playback_started or started) < (60 if playback_started is None else 55 if args.manual_start else 38):
            time.sleep(.25)
            state = probe.command(base, '/execute/sync', {'script': '''return {...snapshot(),sourceBuffers:bufferState(),quality:document.querySelector('#video').getVideoPlaybackQuality?.(),selection:window.hls?{current:hls.currentLevel,load:hls.loadLevel,auto:hls.autoLevelEnabled}:null,controller:window.hls?{main:hls.streamController.state,parts:hls.latestLevelDetails?.partList?.slice(-12).map(p=>({sn:p.fragment.sn,index:p.index,loaded:p.loaded,start:p.start,end:p.end})),last:hls.streamController.fragPrevious?.sn,audio:hls.audioStreamController.state}:null}''' , 'args': []})
            if playback_started is None and not state['paused']:
                playback_started = time.monotonic()
                report['startup_seconds'] = playback_started - started
            report['state'] = state
            args.output.write_text(json.dumps(report,indent=2))
            report['samples'].append({'wall':time.monotonic()-started, **{k:state[k] for k in ('time','paused','ready','buffered','visibility')}})
            if switches and state['time'] >= switches[0]['at']:
                request = switches.pop(0)
                changed = probe.command(base, '/execute/sync', {
                    'script': 'return switchTrack(...arguments)',
                    'args': [request['kind'], request['index']]})
                request.update(attempted=True, planned_index=request['index'], time=state['time'], **changed)
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
        report.update(probe.command(base,'/execute/sync',{'script':'''return {frames,transitions,levels,endlistTrace,traceDropped,requests:performance.getEntriesByType('resource').map(e=>e.name),audioTracks:window.hls?.audioTracks.map(t=>({id:t.id,name:t.name})),videoLevels:window.hls?.levels.length}''','args':[]}))
        if args.trace_appends:
            report['appendTrace'] = probe.command(base, '/execute/sync', {'script': 'return window.appendTrace', 'args': []})
        switched = validate_switches(report['switches'], report['transitions'])
        report['switches_ok'] = switched
        report['fixed_level_ok'] = validate_fixed_level(args.fixed_level, report['transitions'])
        report['trace_complete'] = not report['traceDropped']
        report['rewinds'] = [{'from':a['time'],'to':b['time']} for a,b in zip(report['samples'],report['samples'][1:]) if b['time'] < a['time']-.1]
        report['ok'] = not report['rewinds'] and switched and report['fixed_level_ok'] and state['ended'] and state['time'] >= 23.8 and not state.get('fatal') and not state.get('error') and report['frames']['count'] > 100
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
