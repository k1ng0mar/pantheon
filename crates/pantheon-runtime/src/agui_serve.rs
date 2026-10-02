//! AG-UI routes, mounted on the gateway's single HTTP serve surface.
//!
//! This module owns the pure AG-UI handler logic; the gateway's
//! `pantheon_gateway::http` server owns the socket, the accept loop, and
//! the auth/origin layer. Route behavior is unchanged from the old
//! runtime-owned shim:
//! - `GET /`, `/agui`, `/agui/` - the token-injected web client
//! - `GET /agui/stream` - SSE replay + 25 s long-poll, connection-close
//!   terminated (non-chunked), delivered as `Response::RawStream`
//! - `POST /agui/rpc` - JSON-RPC
//! - `GET /agui/blob/<task>` - signed generative-UI bytes
//! - `GET /agui/health` - `{"ok":true}`
//! - `POST /agui/voice/transcribe`, `POST /agui/voice/speak` - speech edge
//! - `GET /agui/voice/live` - websocket takeover into live-voice mode
//!
//! OPERATING NOTES (group-C audit, carried over verbatim):
//! - Every RPC method opens a fresh `Supervisor` (3 SQLite connections +
//!   migrations) and drops it. Milliseconds for a local single-user
//!   server; a multi-user server needs a SupervisorPool (not built).
//! - SSE streams poll the ledger every 500 ms for 25 s max, then close.
//!   Terminal runs (completed/failed/canceled) close early; an
//!   awaiting_approval run stays open for the window and the WEB CLIENT
//!   is expected to reconnect (Last-Event-ID / ?after= supported).
use pantheon_gateway::http::{AuthGroup, HttpMount, Request, Response};
use pantheon_gateway::{
    frames_for_entries, live_voice, parse_last_event_id, valid_task_id,
    voice::{VoiceEdge, SPEAK_PATH, TRANSCRIBE_PATH},
    GenUiSigner, SseEncoder, UiFrame, UiFrameKind,
};
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AguiServeConfig {
    pub data_dir: PathBuf,
    pub host: String,
    pub port: u16,
    pub genui_base: String,
    /// Bearer token for every /agui route except /agui/health. Enforced by
    /// the gateway's auth layer (`AuthGroup::Agui`): `Authorization: Bearer`,
    /// `X-Pantheon-Token`, or `?token=` on `/agui/stream` (EventSource
    /// cannot set headers). `None` means the embedder authenticates some
    /// other way - the old shim generated a one-time token here, which is
    /// now the gateway's job.
    pub auth_token: Option<String>,
    /// Speech edge for the mobile app (`/agui/voice/*`), built from the
    /// `[tools]` voice toggle and the `[stt]` / `[tts]` config sections.
    /// Disabled or unset sections make the routes 400 with
    /// `voice_not_configured` rather than failing silently.
    pub voice: VoiceEdge,
    /// Live voice mode (`GET /agui/voice/live`): `[voice]` limits plus the
    /// VoicePipes double-gated STT/TTS backends. The per-session gate
    /// (`LiveVoiceConfig::gate`) decides refusal - a disabled config
    /// refuses every session with an `error` + `end` frame pair, never a
    /// silent hang.
    pub live_voice: Arc<live_voice::LiveVoiceConfig>,
}

/// Opens a Session for one turn. Installed by `pantheon serve` through
/// `agui::set_session_factory` rather than threaded through every
/// constructor, so this crate does not depend on pantheon-tui, which owns
/// config.toml and the key-name conventions.
pub type SessionFactory = std::sync::Arc<
    dyn Fn(&PathBuf) -> Result<crate::session::Session, pantheon_api::error::PantheonError>
        + Send
        + Sync,
>;

impl AguiServeConfig {
    fn effective_genui_base(&self) -> String {
        self.genui_base
            .replace("http://0.0.0.0", "http://127.0.0.1")
            .replace("http://[::]", "http://[::1]")
    }
    pub fn signer(&self) -> GenUiSigner {
        let secret = std::env::var("PANTHEON_GENUI_SECRET")
            .map(|s| s.into_bytes())
            .unwrap_or_else(|_| b"pantheon-dev-genui-secret".to_vec());
        GenUiSigner::new(self.effective_genui_base(), secret)
    }
    pub fn dispatcher(&self) -> crate::rpc::Dispatcher {
        crate::agui::dispatcher_for_with_hint_and_host(
            self.data_dir.clone(),
            self.port,
            self.effective_genui_base(),
            &self.host,
        )
    }
}
pub const WEB_UI: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Pantheon AG-UI</title>
<style>
body{font:16px system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem}#log{white-space:pre-wrap;border:1px solid #ddd;padding:1rem;min-height:18rem}button{padding:.45rem .8rem;margin:.2rem}
#voice{position:fixed;inset:0;background:rgba(0,0,0,.88);display:flex;align-items:center;justify-content:center;z-index:10}
#voice[hidden]{display:none}
#voice-card{display:flex;flex-direction:column;align-items:center;gap:.75rem;background:#1e1e1e;color:#eee;padding:1.5rem;border-radius:1rem;width:min(92vw,420px)}
#voice-status{font-size:.95rem;color:#bbb;min-height:1.4em;text-align:center}
#voice-meter{width:100%;height:24px;background:#121212;border-radius:.4rem}
#voice-transcript{width:100%;max-height:28vh;overflow:auto;font-size:.9rem;white-space:pre-wrap;border-top:1px solid #333;padding-top:.5rem;min-height:4rem}
#voice-transcript .who{color:#888}
#voice-mic{width:96px;height:96px;border-radius:50%;font-size:2.5rem;background:#2a2a2a;border:2px solid #555;cursor:pointer;color:#fff}
#voice-mic:disabled{opacity:.35;cursor:default}
#voice-mic.on{background:#a31621;border-color:#ff6b6b}
#voice-miclabel{font-size:.85rem;color:#bbb}
#voice-end{background:#333;color:#fff;border:1px solid #555;border-radius:.5rem}
</style>
<h1>Pantheon</h1>
<form id="send"><input id="text" required placeholder="Ask something" style="width:70%"><button>Send</button></form>
<button id="cancel" disabled>Cancel run</button><button id="live">Live</button><div id="actions"></div><pre id="log"></pre>
<div id="voice" hidden>
<div id="voice-card">
<div id="voice-status">Connecting...</div>
<canvas id="voice-meter" width="280" height="24"></canvas>
<div id="voice-transcript"></div>
<button id="voice-mic" disabled>🎤</button>
<div id="voice-miclabel">Tap to talk</div>
<button id="voice-end">End</button>
</div>
</div>
<script>
const $=s=>document.querySelector(s), log=s=>{$('#log').textContent+=s+'\n'};
let rpcId=1, run='', thread='', cursor=0, es;
const TOKEN=__PANTHEON_TOKEN__;
async function rpc(method,params={}){let r=await fetch('/agui/rpc',{method:'POST',headers:{'content-type':'application/json','x-pantheon-token':TOKEN},body:JSON.stringify({jsonrpc:'2.0',id:rpcId++,method,params})});let j=await r.json();if(j.error)throw Error(j.error.message);return j.result}
function showActions(f){if(f.name!=='requested')return;const actions=$('#actions');actions.innerHTML='';for(const [label,answer] of [['Grant','grant'],['Deny','deny']]){const b=document.createElement('button');b.textContent=label;b.onclick=async()=>{try{await rpc('agui.'+answer,{run_id:run,scope:f.text});actions.innerHTML=''}catch(e){log(e.message)}};actions.appendChild(b)}}
function openStream(){if(es)es.close();es=new EventSource('/agui/stream?run='+encodeURIComponent(run)+'&thread='+encodeURIComponent(thread)+'&after='+cursor+(TOKEN?'&token='+encodeURIComponent(TOKEN):''));for(const kind of ['run','text','tool','state','genui'])es.addEventListener(kind,e=>{const f=JSON.parse(e.data);cursor=Math.max(cursor,f.id||0);log(kind+': '+f.text)});es.addEventListener('approval',e=>showActions(JSON.parse(e.data)))}
$('#cancel').onclick=async()=>{if(!run)return;try{await rpc('agui.cancel',{run_id:run});$('#cancel').disabled=true;log('run canceled')}catch(e){log(e.message)}};
$('#send').onsubmit=async e=>{e.preventDefault();const text=$('#text').value;$('#text').value='';try{const r=await rpc('agui.send',run?{run_id:run,text}:{text});run=r.run_id;thread=r.thread_id;$('#cancel').disabled=false;openStream()}catch(e){log(e.message)}};
/* Live voice mode: WS /agui/voice/live. Client -> server: text {start,end,stop}
   frames and binary 16 kHz mono 16-bit PCM chunks (~100 ms). Server -> client:
   ready, transcript, reply_text, binary PCM reply, audio_end, busy,
   approval_needed, error, end. Reply audio is buffered and played on audio_end
   (simpler and more reliable than gapless chunk chaining). No barge-in in v1:
   the mic is disabled while a turn is in flight. */
let lv={ws:null,ctx:null,stream:null,analyser:null,proc:null,zero:null,recording:false,busy:false,pending:null,playbackChunks:[],meterTimer:null,closed:false};
const lvEl=s=>document.querySelector(s);
function lvMsg(who,text){const t=lvEl('#voice-transcript');const d=document.createElement('div');const w=document.createElement('span');w.className='who';w.textContent=who+': ';d.appendChild(w);d.appendChild(document.createTextNode(text));t.appendChild(d);t.scrollTop=t.scrollHeight}
function lvStatus(s){lvEl('#voice-status').textContent=s}
function lvSend(o){if(lv.ws&&lv.ws.readyState===1)lv.ws.send(JSON.stringify(o))}
function lvSetBusy(b){lv.busy=b;const m=lvEl('#voice-mic');if(b){m.disabled=true;if(lv.recording===false)lvStatus('Agent is replying...')}else if(!lv.closed){m.disabled=false;if(lv.recording===false)lvStatus('Tap the mic to talk')}}
function lvEmitPCM(samples){
  let buf;
  if(lv.pending.length){const m=new Float32Array(lv.pending.length+samples.length);m.set(lv.pending);m.set(samples,lv.pending.length);buf=m}else buf=samples;
  const CH=1600;let off=0;
  while(buf.length-off>=CH){
    const pcm=new Int16Array(CH);
    for(let i=0;i<CH;i++){let v=buf[off+i];v=v>1?1:v<-1?-1:v;pcm[i]=v<0?v*32768:v*32767}
    if(lv.ws&&lv.ws.readyState===1)lv.ws.send(pcm.buffer);
    off+=CH;
  }
  lv.pending=off?buf.slice(off):buf;
}
function lvMeterLoop(){
  const c=lvEl('#voice-meter'),g=c.getContext('2d');
  const data=new Uint8Array(lv.analyser.fftSize);
  (function tick(){
    if(lv.closed)return;
    lv.analyser.getByteTimeDomainData(data);
    let peak=0;
    for(let i=0;i<data.length;i+=4){const v=Math.abs(data[i]-128)/128;if(v>peak)peak=v}
    g.clearRect(0,0,c.width,c.height);
    g.fillStyle=peak>0.6?'#e5484d':'#3fb950';
    g.fillRect(0,0,c.width*Math.min(1,peak*1.5),c.height);
    lv.meterTimer=requestAnimationFrame(tick);
  })();
}
function lvPlayQueued(){
  const chunks=lv.playbackChunks;lv.playbackChunks=[];
  if(!chunks.length||!lv.ctx)return;
  let total=0;for(const c of chunks)total+=c.length;
  const buf=lv.ctx.createBuffer(1,total,16000),ch=buf.getChannelData(0);
  let off=0;
  for(const c of chunks){for(let i=0;i<c.length;i++)ch[off+i]=c[i]/32768;off+=c.length}
  const src=lv.ctx.createBufferSource();src.buffer=buf;src.connect(lv.ctx.destination);src.start();
}
function lvClose(msg){
  if(lv.closed)return;lv.closed=true;
  if(msg)lvStatus(msg);
  if(lv.recording){lv.recording=false;try{lvSend({type:'end'})}catch(e){}}
  try{lvSend({type:'stop'})}catch(e){}
  try{if(lv.ws)lv.ws.close()}catch(e){}
  if(lv.meterTimer)cancelAnimationFrame(lv.meterTimer);
  try{if(lv.proc)lv.proc.disconnect()}catch(e){}
  try{if(lv.zero)lv.zero.disconnect()}catch(e){}
  try{if(lv.analyser)lv.analyser.disconnect()}catch(e){}
  if(lv.stream)lv.stream.getTracks().forEach(t=>t.stop());
  if(lv.ctx)lv.ctx.close().catch(()=>{});
  lv={ws:null,ctx:null,stream:null,analyser:null,proc:null,zero:null,recording:false,busy:false,pending:null,playbackChunks:[],meterTimer:null,closed:true};
  setTimeout(()=>{lvEl('#voice').hidden=true},msg?1800:0);
}
function lvOnMsg(e){
  if(typeof e.data!=='string'){if(e.data instanceof ArrayBuffer)lv.playbackChunks.push(new Int16Array(e.data));return}
  let f;try{f=JSON.parse(e.data)}catch(err){return}
  const m=lvEl('#voice-mic');
  switch(f.type){
    case 'ready':lvStatus('Ready - tap the mic to talk');if(!lv.closed)m.disabled=false;break;
    case 'transcript':if(f.final&&f.text)lvMsg('You',f.text);break;
    case 'reply_text':if(f.text)lvMsg('Agent',f.text);break;
    case 'audio_end':lvPlayQueued();lvSetBusy(false);break;
    case 'busy':lvSetBusy(true);break;
    case 'approval_needed':lvMsg('System','Approval needed - answer it in the text chat, then End this call.');m.disabled=true;lvStatus('Paused for approval');break;
    case 'error':lvClose('Error: '+(f.code||'unknown'));break;
    case 'end':lvClose('Session ended by server');break;
  }
}
async function lvStart(){
  if(lv.ws&&lv.ws.readyState<2)return; // a session is already up or connecting
  lv.closed=false;lv.recording=false;lv.busy=false;lv.pending=new Float32Array(0);lv.playbackChunks=[];
  lvEl('#voice-transcript').innerHTML='';
  const m=lvEl('#voice-mic');m.disabled=true;m.classList.remove('on');
  lvEl('#voice-miclabel').textContent='Tap to talk';
  lvEl('#voice').hidden=false;lvStatus('Connecting...');
  const proto=location.protocol==='https:'?'wss:':'ws:';
  const ws=new WebSocket(proto+'//'+location.host+'/agui/voice/live?token='+encodeURIComponent(TOKEN));
  lv.ws=ws;ws.binaryType='arraybuffer';
  ws.onerror=()=>{if(!lv.closed)lvClose('Connection failed')};
  ws.onclose=()=>{if(!lv.closed)lvClose('Connection closed')};
  ws.onmessage=e=>lvOnMsg(e);
  try{
    lv.stream=await navigator.mediaDevices.getUserMedia({audio:true});
  }catch(e){lvClose('Microphone unavailable');return}
  if(lv.closed)return;
  const ctx=new (window.AudioContext||window.webkitAudioContext)();
  lv.ctx=ctx;ctx.resume().catch(()=>{});
  const src=ctx.createMediaStreamSource(lv.stream);
  lv.analyser=ctx.createAnalyser();lv.analyser.fftSize=2048;src.connect(lv.analyser);
  lv.proc=ctx.createScriptProcessor(4096,1,1);
  lv.zero=ctx.createGain();lv.zero.gain.value=0; // keep the ScriptProcessor alive without feedback
  lv.proc.connect(lv.zero);lv.zero.connect(ctx.destination);
  const ratio=ctx.sampleRate/16000;
  lv.proc.onaudioprocess=ev=>{
    if(lv.recording&&!lv.busy&&!lv.closed){
      const inb=ev.inputBuffer.getChannelData(0),n=Math.floor(inb.length/ratio);
      const out=new Float32Array(n);
      for(let i=0;i<n;i++){
        const s0=Math.floor(i*ratio),s1=Math.min(Math.floor((i+1)*ratio),inb.length);
        let acc=0;for(let j=s0;j<s1;j++)acc+=inb[j];
        out[i]=s1>s0?acc/(s1-s0):0;
      }
      lvEmitPCM(out);
    }
  };
  src.connect(lv.proc);
  lvMeterLoop();
}
lvEl('#live').onclick=()=>lvStart();
lvEl('#voice-end').onclick=()=>lvClose('');
lvEl('#voice-mic').onclick=()=>{
  if(lv.closed||lv.busy||!lv.ws||lv.ws.readyState!==1)return;
  const m=lvEl('#voice-mic');
  if(!lv.recording){
    lv.recording=true;lvSend({type:'start'});
    m.classList.add('on');lvEl('#voice-miclabel').textContent='Tap to stop';lvStatus('Listening...');
  }else{
    lv.recording=false;lvSend({type:'end'});
    m.classList.remove('on');lvEl('#voice-miclabel').textContent='Tap to talk';lvSetBusy(true);
  }
};
</script>"##;

/// Pure: inject `token` into the served page as a JS string literal.
/// serde_json produces the quoted literal (escaping quotes, backslashes,
/// control chars); `<` is additionally escaped as `\u003c` so a token can
/// never contain a literal `</script>` that would break out of the script
/// block.
fn inject_token(page: &str, token: Option<&str>) -> String {
    let lit = serde_json::to_string(token.unwrap_or("")).unwrap_or_else(|_| "\"\"".into());
    let lit = lit.replace('<', "\\u003c");
    page.replace("__PANTHEON_TOKEN__", &lit)
}
fn thread_for(data_dir: &Path, run_id: &str, fallback: &str) -> String {
    if !fallback.is_empty() {
        return fallback.to_string();
    }
    let p = data_dir.join("threads.json");
    if let Ok(raw) = std::fs::read_to_string(&p) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw) {
            if let Some(t) = map.get(run_id) {
                return t.clone();
            }
        }
    }
    format!("cli:{run_id}")
}
/// Serializes the threads.json read-modify-write: two concurrent
/// agui.send calls (different runs) must not lose each other's entries.
pub static THREADS_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

pub fn remember_thread(data_dir: &PathBuf, run_id: &str, thread_id: &str) {
    let _guard = THREADS_LOCK.lock().unwrap();
    let p = data_dir.join("threads.json");
    let mut map: HashMap<String, String> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default();
    map.insert(run_id.to_string(), thread_id.to_string());
    if let Ok(raw) = serde_json::to_string(&map) {
        let _ = std::fs::create_dir_all(data_dir);
        let _ = std::fs::write(&p, raw);
    }
}
pub fn snapshot_frames(data_dir: &Path, run_id: &str, thread: &str, after: i64) -> Vec<UiFrame> {
    let sup = match crate::Supervisor::open(data_dir.to_path_buf()) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let entries = sup.replay(run_id).unwrap_or_default();
    let thread_id = thread_for(data_dir, run_id, thread);
    let mut frames = frames_for_entries(&entries, &thread_id);
    frames.retain(|f| f.id > after || f.id == 0);
    frames
}
fn route_is(path: &str, route: &str) -> bool {
    path == route || path.starts_with(&format!("{route}?"))
}
/// Case-insensitive header lookup. The gateway parser stores header names
/// as received; the old shim lowercased them at parse time, so lookups
/// stay case-insensitive to keep behavior identical.
fn header<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find_map(|(k, v)| k.eq_ignore_ascii_case(name).then_some(v.as_str()))
}
/// The SSE replay + 25 s long-poll, connection-close terminated. Runs as
/// the `Response::RawStream` closure: the gateway writes the 200 head
/// first, then calls this, then closes the connection. Wire behavior is
/// exactly the old `handle_stream`: non-chunked, no trailing framing.
/// Callers must reject an empty `run` query before building the stream
/// (the 400 for a missing `?run=` is a buffered response, not a stream).
fn handle_stream(
    stream: &TcpStream,
    cfg: &AguiServeConfig,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
) {
    let run_id = query.get("run").cloned().unwrap_or_default();
    let thread = query.get("thread").cloned().unwrap_or_default();
    let mut after: i64 = query.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
    if after == 0 {
        if let Some(h) = header(headers, "last-event-id") {
            if let Some(n) = parse_last_event_id(h) {
                after = n;
            }
        }
    }
    let enc = SseEncoder;
    let mut s = stream;
    let _ = s.write_all(enc.head().as_bytes());
    let mut sent = after;
    for f in snapshot_frames(&cfg.data_dir, &run_id, &thread, after) {
        if f.id > sent {
            sent = f.id;
        }
        let _ = s.write_all(enc.frame(&f).as_bytes());
    }
    let _ = s.flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    let _ = s.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let fresh = snapshot_frames(&cfg.data_dir, &run_id, &thread, sent);
        let mut progressed = false;
        for f in fresh {
            if f.id == 0 {
                continue;
            }
            if f.id > sent {
                sent = f.id;
                progressed = true;
                if s.write_all(enc.frame(&f).as_bytes()).is_err() {
                    return;
                }
            }
        }
        if progressed && s.flush().is_err() {
            return;
        }
        if let Ok(sup) = crate::Supervisor::open(cfg.data_dir.clone()) {
            if let Ok(Some(s)) = sup.ledger_status(&run_id) {
                if s == "completed" || s == "failed" || s == "canceled" {
                    break;
                }
            }
        }
    }
}
fn json(status: u16, body: &[u8]) -> Response {
    Response::Buffered {
        status,
        content_type: "application/json",
        body: body.to_vec(),
        extra_headers: Vec::new(),
    }
}
fn handle_rpc(cfg: &AguiServeConfig, body: &str) -> Response {
    let d = cfg.dispatcher();
    let mut out = Vec::new();
    for resp in d.handle_line(body.trim()) {
        out.push(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()));
    }
    json(200, out.join("\n").as_bytes())
}
/// `Response::Buffered` needs a `&'static str` content type; artifact mimes
/// are caller-supplied at `put_artifact` time, so map the generative-UI
/// set to static literals and fall back to octet-stream.
/// Intern an artifact MIME as `&'static str`: `Response` needs a static
/// content type, but artifact mimes are caller-supplied (genui RPC) and the
/// old server passed them through verbatim. The cache bounds the leak to
/// one entry per distinct mime ever served.
fn blob_content_type(mime: &str) -> &'static str {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, &'static str>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut cache = cache.lock().unwrap();
    if let Some(&s) = cache.get(mime) {
        return s;
    }
    let leaked: &'static str = Box::leak(mime.to_string().into_boxed_str());
    cache.insert(mime.to_string(), leaked);
    leaked
}
fn handle_blob(cfg: &AguiServeConfig, path: &str, query: &HashMap<String, String>) -> Response {
    let task = path.trim_start_matches("/agui/blob/").to_string();
    let exp: i64 = query.get("exp").and_then(|v| v.parse().ok()).unwrap_or(0);
    let sig = query.get("sig").cloned().unwrap_or_default();
    if !valid_task_id(&task) {
        return json(400, br#"{"error":"bad task"}"#);
    }
    if !cfg.signer().verify(&task, exp, &sig) {
        return json(403, br#"{"error":"bad signature or expired"}"#);
    }
    let sup = match crate::Supervisor::open(cfg.data_dir.clone()) {
        Ok(sup) => sup,
        Err(_) => return json(500, br#"{"error":"ledger unavailable"}"#),
    };
    match sup.artifact(&task) {
        Ok(Some(artifact)) => Response::Buffered {
            status: 200,
            content_type: blob_content_type(&artifact.mime),
            body: artifact.bytes,
            extra_headers: Vec::new(),
        },
        Ok(None) => json(404, br#"{"error":"no such artifact"}"#),
        Err(_) => json(500, br#"{"error":"artifact read failed"}"#),
    }
}

/// How long a live-voice agent turn may run before the session gives up
/// on it and reports `turn_timeout`.
const LIVE_TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Hand `GET /agui/voice/live` to the gateway's live-voice session. The
/// request head is rebuilt verbatim (header names may arrive in any case,
/// which tungstenite tolerates) so `tungstenite::accept` sees the
/// handshake it expects. The runtime stays transport-blind: all voice
/// logic lives in `pantheon_gateway::live_voice`.
fn handle_live_voice(
    cfg: &AguiServeConfig,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
) -> Response {
    let mut head = format!("{method} {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let driver: Arc<dyn live_voice::LiveTurnDriver> = Arc::new(LiveVoiceDriver {
        data_dir: cfg.data_dir.clone(),
        port: cfg.port,
        genui_base: cfg.effective_genui_base(),
        host: cfg.host.clone(),
    });
    let live = Arc::clone(&cfg.live_voice);
    let head = head.into_bytes();
    Response::Takeover {
        run: Box::new(move |stream| live_voice::serve_live_session(stream, head, live, driver)),
    }
}

/// Drives one live-voice agent turn through the exact dispatcher path
/// `/agui/rpc` chat uses: `agui.send` admits the turn (same session
/// factory, same per-run turn lock), then this polls the ledger for the
/// terminal outcome - reply text, parked approval, or failure.
struct LiveVoiceDriver {
    data_dir: PathBuf,
    port: u16,
    genui_base: String,
    host: String,
}

impl live_voice::LiveTurnDriver for LiveVoiceDriver {
    fn run_turn(&self, transcript: &str) -> live_voice::TurnOutcome {
        use live_voice::TurnOutcome;
        let run_id = crate::new_run_id();
        // One thread per live session, so the whole conversation replays
        // coherently; one run per utterance, like tapping send each time.
        let thread_id = format!("voice-live:{}", crate::new_run_id());
        remember_thread(&self.data_dir, &run_id, &thread_id);
        let dispatcher = crate::agui::dispatcher_for_with_hint_and_host(
            self.data_dir.clone(),
            self.port,
            self.genui_base.clone(),
            &self.host,
        );
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "agui.send",
            "params": {"text": transcript, "run_id": run_id, "thread_id": thread_id},
        })
        .to_string();
        match dispatcher.handle_line(&line).into_iter().next() {
            Some(r) if r.is_success() => {}
            Some(r) => {
                let detail = r.error.map(|e| e.message).unwrap_or_default();
                log_warn!("live voice: agui.send rejected run {run_id}: {detail}");
                return TurnOutcome::failed("turn_rejected");
            }
            None => return TurnOutcome::failed("turn_rejected"),
        }
        let deadline = std::time::Instant::now() + LIVE_TURN_TIMEOUT;
        let mut ledger_errors = 0u32;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if std::time::Instant::now() >= deadline {
                return TurnOutcome::failed("turn_timeout");
            }
            let sup = match crate::Supervisor::open(self.data_dir.clone()) {
                Ok(s) => s,
                Err(e) => {
                    ledger_errors += 1;
                    log_warn!("live voice: supervisor open failed: {e}");
                    if ledger_errors >= 20 {
                        return TurnOutcome::failed("ledger_unavailable");
                    }
                    continue;
                }
            };
            let status = match sup.ledger_status(&run_id) {
                Ok(s) => {
                    ledger_errors = 0;
                    s
                }
                Err(e) => {
                    ledger_errors += 1;
                    log_warn!("live voice: ledger_status failed: {e}");
                    if ledger_errors >= 20 {
                        return TurnOutcome::failed("ledger_unavailable");
                    }
                    continue;
                }
            };
            match status.as_deref() {
                Some("awaiting_approval") => {
                    // Parked: surface the pending scope as `approval_needed`.
                    // Never auto-approved - the operator answers through
                    // the normal (text) approval path.
                    let scope = snapshot_frames(&self.data_dir, &run_id, &thread_id, 0)
                        .into_iter()
                        .rev()
                        .find(|f| f.kind == UiFrameKind::Approval && f.name == "requested")
                        .map(|f| f.text)
                        .unwrap_or_default();
                    return TurnOutcome::approval_needed(scope);
                }
                Some("completed") => {
                    let frames = snapshot_frames(&self.data_dir, &run_id, &thread_id, 0);
                    let mut text = String::new();
                    // The final assistant message first; streaming deltas
                    // only as a fallback. Tool result frames are never
                    // spoken back.
                    for name in ["message", "delta"] {
                        for f in &frames {
                            if f.kind == UiFrameKind::Text && f.name == name {
                                text.push_str(&f.text);
                            }
                        }
                        if !text.is_empty() {
                            break;
                        }
                    }
                    return TurnOutcome::answered(text);
                }
                Some("failed") => return TurnOutcome::failed("turn_failed"),
                Some("canceled") => return TurnOutcome::failed("turn_canceled"),
                _ => {}
            }
        }
    }
}

/// The AG-UI route group on the gateway's single listener.
#[derive(Debug, Clone)]
pub struct AguiMount {
    pub cfg: AguiServeConfig,
}

impl HttpMount for AguiMount {
    fn auth_group(&self, req: &Request) -> Option<AuthGroup> {
        if req.path == "/" || req.path.starts_with("/agui") {
            Some(AuthGroup::Agui)
        } else {
            None
        }
    }

    fn handle(&self, req: &Request) -> Response {
        let cfg = &self.cfg;
        if req.method == "GET" && (req.path == "/" || req.path == "/agui" || req.path == "/agui/") {
            // Inject the token into the served UI so its fetch calls carry it.
            // The token is encoded as a JSON string literal: raw replacement
            // would let a quote or </script> in the token break out of the
            // script block.
            let page = inject_token(WEB_UI, cfg.auth_token.as_deref());
            Response::Buffered {
                status: 200,
                content_type: "text/html; charset=utf-8",
                body: page.into_bytes(),
                extra_headers: Vec::new(),
            }
        } else if req.method == "GET" && route_is(&req.path, "/agui/stream") {
            // The 400 for a missing ?run= stays a buffered response; the
            // stream closure assumes a run id is present.
            let has_run = req.query.get("run").map(|s| !s.is_empty()).unwrap_or(false);
            if !has_run {
                return json(400, br#"{"error":"missing ?run="}"#);
            }
            let cfg = cfg.clone();
            let query = req.query.clone();
            let headers = req.headers.clone();
            Response::RawStream {
                content_type: "text/event-stream",
                stream: Box::new(move |stream: &TcpStream| {
                    handle_stream(stream, &cfg, &query, &headers)
                }),
            }
        } else if req.method == "POST" && route_is(&req.path, "/agui/rpc") {
            handle_rpc(cfg, req.body_str())
        } else if req.method == "POST" && route_is(&req.path, TRANSCRIBE_PATH) {
            let r = cfg.voice.handle_transcribe(&req.body);
            Response::Buffered {
                status: r.status,
                content_type: r.content_type,
                body: r.body,
                extra_headers: Vec::new(),
            }
        } else if req.method == "POST" && route_is(&req.path, SPEAK_PATH) {
            let r = cfg.voice.handle_speak(&req.body);
            Response::Buffered {
                status: r.status,
                content_type: r.content_type,
                body: r.body,
                extra_headers: Vec::new(),
            }
        } else if req.method == "GET" && route_is(&req.path, live_voice::LIVE_VOICE_PATH) {
            // Bearer auth already ran in the gateway layer (401 before
            // upgrade on failure); this layer only rebuilds the raw request
            // head the parse consumed, so tungstenite can parse the WS
            // handshake itself.
            handle_live_voice(cfg, &req.method, &req.path, &req.headers)
        } else if req.method == "GET" && req.path.starts_with("/agui/blob/") {
            handle_blob(cfg, &req.path, &req.query)
        } else if req.method == "GET" && req.path == "/agui/health" {
            json(200, br#"{"ok":true}"#)
        } else {
            json(404, br#"{"error":"unknown agui route"}"#)
        }
    }
}
