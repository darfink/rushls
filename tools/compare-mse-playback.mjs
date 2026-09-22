#!/usr/bin/env node
// Requires Node.js with built-in WebSocket and a ChromeDriver on localhost.
// Replays a captured, completed origin fixture without changing its media bytes.
import fs from 'node:fs/promises';
import path from 'node:path';
import http from 'node:http';
import crypto from 'node:crypto';

const [fixture, bundle, output, driver = 'http://127.0.0.1:4446'] = process.argv.slice(2);
if (!output) throw new Error('Usage: compare-mse-playback.mjs FIXTURE HLS_JS OUTPUT [WEBDRIVER]');
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function command(url, data, method = data === undefined ? 'GET' : 'POST') {
  const response = await fetch(url, { method, headers: {'Content-Type':'application/json'}, body: data === undefined ? undefined : JSON.stringify(data) });
  const result = await response.json();
  if (!response.ok || result.value?.error) throw new Error(JSON.stringify(result));
  return result.value;
}
function player() {
  const v = document.querySelector('video');
  const ranges = b => Array.from({length:b.length}, (_,i)=>[b.start(i),b.end(i)]);
  const result = window.result = { events:[], frames:[], appends:[], error:null, buffers:{}, initialized:false };
  for (const event of ['playing','waiting','stalled','ended','error','seeking','seeked'])
    v.addEventListener(event,()=>result.events.push({event,time:v.currentTime,wall:performance.now(),error:v.error?.message}));
  const frame = (_,m) => {result.frames.push({time:m.mediaTime,wall:performance.now()});v.requestVideoFrameCallback(frame);};
  v.requestVideoFrameCallback(frame);
  window.snapshot = () => ({...result,time:v.currentTime,duration:v.duration,ended:v.ended,ready:v.readyState,paused:v.paused,
    quality:v.getVideoPlaybackQuality(),buffers:Object.fromEntries(Object.entries(result.buffers).map(([k,b])=>[k,ranges(b.buffered)])),
    mediaSource:window.ms?.readyState || window.hls?.bufferController?.mediaSource?.readyState});
  const text = url=>fetch(url).then(r=>{if(!r.ok)throw Error(`${url}: ${r.status}`);return r.text();});
  window.run = async mode => {
    try {
      if (mode.startsWith('hls')) {
        const hls=window.hls=new Hls({startPosition:0,startLevel:0,lowLatencyMode:mode==='hls-parts'});
        hls.on(Hls.Events.BUFFER_CREATED,(_,d)=>{for(const [k,t] of Object.entries(d.tracks))result.buffers[k]=t.buffer;});
        hls.on(Hls.Events.ERROR,(_,e)=>{result.events.push({event:e.details,time:v.currentTime,fatal:e.fatal});if(e.fatal)result.error=e.details;});
        hls.on(Hls.Events.LEVEL_SWITCHED,(_,d)=>result.events.push({event:'level',level:d.level,time:v.currentTime}));
        hls.on(Hls.Events.MANIFEST_PARSED,()=>{hls.loadLevel=0;hls.autoLevelCapping=0;v.play().catch(e=>result.error=String(e));});
        hls.attachMedia(v);hls.loadSource('/index.m3u8');result.initialized=true;return;
      }
      if (mode === 'mse-trace') {
        const trace = JSON.parse(await text('/append-trace.json'));
        if (trace.truncated) throw Error('Incomplete append capture');
        const ms = window.ms = new MediaSource(); v.src = URL.createObjectURL(ms);
        await new Promise(resolve => ms.addEventListener('sourceopen', resolve, {once:true}));
        const buffers = trace.buffers.map(({id,mime}) => result.buffers[id] = ms.addSourceBuffer(mime));
        for (const row of trace.operations) {
          if (!['appendBuffer','remove','abort'].includes(row.event) || row.error) continue;
          const sb = buffers[row.id];
          if (sb.updating) await new Promise(resolve => sb.addEventListener('updateend', resolve, {once:true}));
          sb.timestampOffset = row.offset; sb.appendWindowStart = row.start; sb.appendWindowEnd = row.end ?? Infinity;
          if (row.event === 'abort') { sb.abort(); continue; }
          const args = row.event === 'appendBuffer' ? [Uint8Array.from(atob(row.data), c => c.charCodeAt(0))] : row.args;
          await new Promise((resolve,reject) => {
            const done=()=>{sb.removeEventListener('error',fail);resolve();};
            const fail=()=>{sb.removeEventListener('updateend',done);reject(Error('Replay SourceBuffer error'));};
            sb.addEventListener('updateend',done,{once:true});sb.addEventListener('error',fail,{once:true});sb[row.event](...args);
          });
          result.appends.push({id:row.id,event:row.event,buffered:ranges(sb.buffered)});
        }
        ms.endOfStream();result.initialized=true;await v.play();return;
      }
      const master=(await text('/index.m3u8')).split('\n');
      const variants=[];let audio;
      for(let i=0;i<master.length;i++){
        if(master[i].startsWith('#EXT-X-STREAM-INF:'))variants.push({line:master[i],uri:master[i+1],bw:Number(/(?:^|,)BANDWIDTH=(\d+)/.exec(master[i].split(':').slice(1).join(':'))?.[1])});
        if(!audio&&master[i].startsWith('#EXT-X-MEDIA:TYPE=AUDIO'))audio=/URI="([^"]+)"/.exec(master[i])?.[1];
      }
      variants.sort((a,b)=>a.bw-b.bw);const variant=variants[0];
      const codecs=/CODECS="([^"]+)"/.exec(variant.line)[1].split(',');
      const videoCodec=codecs.find(c=>c.startsWith('avc'));const audioCodec=codecs.find(c=>c.startsWith('mp4a'));
      const ms=window.ms=new MediaSource();v.src=URL.createObjectURL(ms);
      await new Promise(resolve=>ms.addEventListener('sourceopen',resolve,{once:true}));
      await Promise.all([['video',variant.uri,videoCodec],['audio',audio,audioCodec]].map(async([kind,uri,codec])=>{
        const url=new URL(uri,location.href);const lines=(await text(url)).split('\n');
        const init=/URI="([^"]+)"/.exec(lines.find(l=>l.startsWith('#EXT-X-MAP:')))[1];
        const media=[];let parts=[];
        for(const line of lines){
          if(line.startsWith('#EXT-X-PART:'))parts.push(/URI="([^"]+)"/.exec(line)[1]);
          else if(line&&!line.startsWith('#')){media.push(...(mode==='mse-parts'&&parts.length?parts:[line]));parts=[];}
        }
        if(mode==='mse-parts')media.push(...parts);
        const sb=ms.addSourceBuffer(`${kind}/mp4; codecs="${codec}"`);result.buffers[kind]=sb;
        for(const item of [init,...media]){
          const target=new URL(item,url);const response=await fetch(target);if(!response.ok)throw Error(`${target}: ${response.status}`);
          const data=await response.arrayBuffer();
          await new Promise((resolve,reject)=>{const done=()=>{sb.removeEventListener('error',fail);resolve();};const fail=()=>{sb.removeEventListener('updateend',done);reject(Error('SourceBuffer error'));};sb.addEventListener('updateend',done,{once:true});sb.addEventListener('error',fail,{once:true});sb.appendBuffer(data);});
          result.appends.push({kind,url:target.pathname,bytes:data.byteLength,buffered:ranges(sb.buffered)});
        }
      }));
      for(const [kind,sb] of Object.entries(result.buffers))if(!sb.buffered.length||sb.buffered.start(0)>.05)throw Error(`${kind} replay omitted stream start`);
      ms.endOfStream();result.initialized=true;await v.play();
    }catch(e){result.error=String(e);}
  };
}
const requests=[];
const page=`<!doctype html><script src="/hls.js"></script><video controls></video><script>(${player.toString()})()</script>`;
const root=path.resolve(fixture);
const server=http.createServer(async(req,res)=>{
  try {
    const pathname=decodeURIComponent(new URL(req.url,'http://localhost').pathname);
    const file=path.resolve(root,'.'+pathname);
    if(file!==root&&!file.startsWith(root+path.sep))throw Error('outside fixture');
    const data=pathname==='/player.html'?Buffer.from(page):pathname==='/hls.js'?await fs.readFile(bundle):await fs.readFile(file);
    requests.push({url:pathname,bytes:data.length,sha256:crypto.createHash('sha256').update(data).digest('hex')});
    res.setHeader('Content-Type',pathname.endsWith('.m3u8')?'application/vnd.apple.mpegurl':pathname.endsWith('.js')?'text/javascript':pathname.endsWith('.html')?'text/html':'video/mp4');res.end(data);
  }catch(e){res.writeHead(404);res.end(String(e));}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const results=[];
try {
  for(const mode of (process.env.RUSHLS_COMPARE_MODES || 'mse-parts,mse-segments,hls-parts,hls-segments').split(',')){
    if(!['mse-parts','mse-segments','hls-parts','hls-segments','mse-trace'].includes(mode))throw Error('Unknown mode: '+mode);
    const session=await command(driver+'/session',{capabilities:{alwaysMatch:{browserName:'chrome','goog:chromeOptions':{args:['--headless=new','--autoplay-policy=no-user-gesture-required']}}}});
    const base=driver+'/session/'+session.sessionId;
    const events=[];let ws;const startRequest=requests.length;
    try{
      const tabs=await fetch('http://'+session.capabilities['goog:chromeOptions'].debuggerAddress+'/json/list').then(r=>r.json());
      ws=new WebSocket(tabs.find(t=>t.type==='page').webSocketDebuggerUrl);
      await new Promise((resolve,reject)=>{ws.addEventListener('open',resolve,{once:true});ws.addEventListener('error',reject,{once:true});});
      let resolveEnable;const enabled=new Promise(resolve=>resolveEnable=resolve);
      ws.addEventListener('message',e=>{const event=JSON.parse(e.data);if(event.id===1)resolveEnable(event);else if(event.method?.startsWith('Media.'))events.push(event);});
      ws.send(JSON.stringify({id:1,method:'Media.enable'}));
      const enabledResult=await enabled;if(enabledResult.error)throw Error(JSON.stringify(enabledResult));
      await command(base+'/url',{url:`http://127.0.0.1:${server.address().port}/player.html`});
      await command(base+'/execute/sync',{script:'run(arguments[0]);return true',args:[mode]});
      const samples=[];let state;const started=Date.now();
      while(Date.now()-started<45000){
        await sleep(250);state=await command(base+'/execute/sync',{script:'return snapshot()',args:[]});
        samples.push({wall:(Date.now()-started)/1000,time:state.time,ready:state.ready,ended:state.ended});
        if(state.ended||state.error)break;
      }
      await sleep(200);
      // An ended event can hide missing video, because the longer audio track
      // can carry playback to the end. This fixture contains exactly 600 frames.
      const complete = state.quality.totalVideoFrames === 600 && Object.values(state.buffers).every(r => r.length === 1 && r[0][0] <= .05 && r[0][1] >= 24);
      const row={mode,complete,ok:complete&&state.ended&&state.time>=23.8&&!state.error,capabilities:session.capabilities,state,samples,mediaEvents:events,requests:requests.slice(startRequest)};
      results.push(row);await fs.writeFile(output,JSON.stringify(results,null,2));
      console.log(mode,row.ok,state.time,state.error,'media events',events.length);
    }finally{ws?.close();await command(base,undefined,'DELETE');}
  }
}finally{server.close();}
