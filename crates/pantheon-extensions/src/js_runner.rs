//! JavaScript/TypeScript hook runner: the other half of the compat adapter.
//!
//! Same wire contract as `python_runner` — one JSON line in on stdin, one JSON
//! line out on stdout, fail-open on every error path — so the manager cannot
//! tell a Python plugin from an adapted foreign one.
//!
//! The shim builds a minimal `api` object and calls the extension's
//! `register(api)`. Handlers attached via `api.on(event, fn)` are kept only
//! when `map_hook` says the event has a Panthey equivalent; everything else is
//! recorded in `_dropped` so the caller can report the loss instead of
//! pretending the plugin ran.
//!
//! Fail-open is inherited deliberately: a plugin that throws must never break
//! a turn. The error is returned in the output envelope for the doctor.

use crate::compat::{map_hook, HookMap};
use crate::hooks::Hook;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn xerr(code: &str, cause: String, retryable: bool) -> pantheon_core::error::PantheonError {
    use pantheon_core::error::Layer;
    pantheon_core::error::PantheonError::new(
        code,
        Layer::Extension,
        retryable,
        cause,
        "check the extension entry file, that node or bun is on PATH, and the hook timeout",
        "",
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct HookInput {
    pub hook: String,
    pub session_id: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub extra: std::collections::HashMap<String, String>,
}

/// The runner's reply. `_dropped` / `_refused` are diagnostics, not errors:
/// a partially-loaded plugin still runs the hooks that did map.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HookOutput {
    pub context: Option<String>,
    #[serde(default)]
    pub dropped: Vec<String>,
    #[serde(default)]
    pub refused: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A foreign extension loaded on disk, ready to fire.
#[derive(Debug, Clone)]
pub struct JsPlugin {
    pub dir: PathBuf,
    pub entry: PathBuf,
    pub name: String,
    /// Foreign events that mapped, discovered at load.
    pub mapped: Vec<String>,
    /// Foreign events with no Pantheon equivalent.
    pub dropped: Vec<String>,
}

impl JsPlugin {
    /// Load an adapted foreign extension. `entry` is the JS file that exports
    /// `register(api)`.
    pub fn load(
        dir: &Path,
        entry: PathBuf,
        name: String,
        mapped: Vec<String>,
        dropped: Vec<String>,
    ) -> Self {
        Self {
            dir: dir.to_path_buf(),
            entry,
            name,
            mapped,
            dropped,
        }
    }

    /// True when this plugin registers the given Pantheon hook.
    pub fn provides(&self, hook: Hook) -> bool {
        self.mapped
            .iter()
            .any(|e| matches!(map_hook(e), HookMap::Mapped(h) if h == hook))
    }

    /// Events that were lost at load time, for the doctor.
    pub fn dropped_events(&self) -> &[String] {
        &self.dropped
    }
}

/// Which interpreter runs the extension.
#[derive(Debug, Clone)]
pub struct JsRunnerConfig {
    /// `node`, `bun`, or an absolute path. Bun is preferred where present
    /// because the OMP extension set ships as an ESM bundle.
    pub runtime: String,
    pub timeout: Duration,
}

impl Default for JsRunnerConfig {
    fn default() -> Self {
        Self {
            runtime: "node".into(),
            timeout: Duration::from_secs(10),
        }
    }
}

impl JsRunnerConfig {
    /// Pick `bun` when it is on PATH, else `node`. Returns the resolved
    /// runtime name, or an error naming both when neither exists.
    pub fn detect() -> Result<Self, pantheon_core::error::PantheonError> {
        for candidate in ["bun", "node"] {
            if Command::new(candidate)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return Ok(Self {
                    runtime: candidate.to_string(),
                    ..Default::default()
                });
            }
        }
        Err(xerr(
            "EXT_JS_NO_RUNTIME",
            "neither bun nor node is on PATH".into(),
            false,
        ))
    }
}

/// The shim. Loaded as an ES module and handed the extension path + payload.
///
/// Design notes:
/// - `api.on` is the only registration we honour. Every other `api.*` method
///   is a no-op that records its own name, so a plugin asking for
///   `registerProvider` is told "refused" rather than silently doing nothing.
/// - Handlers may be sync or async; the shim awaits them and enforces its own
///   deadline so a hung handler cannot wedge the turn.
const SHIM: &str = r#"
import { pathToFileURL } from 'node:url';

// `runtime -e <code> ENTRY HOOK PAYLOAD` puts the user args at argv[1..] on
// node and at a slightly different offset on bun, so take the last three
// rather than hard-coding an index.
const [entry, hookName, payloadJson] = process.argv.slice(-3);

const out = { context: null, dropped: [], refused: [], error: null };
const REFUSABLE = [
  'registerProvider','registerTool','registerCommand','registerCli',
  'registerGatewayMethod','registerHttpRoute','registerService',
  'registerWebSearchProvider','registerImageGenerationProvider',
  'registerVideoGenerationProvider','registerSpeechProvider',
  'registerMediaUnderstandingProvider','registerMemoryEmbeddingProvider',
  'registerMusicGenerationProvider','registerRealtimeTranscriptionProvider',
  'registerModelCatalogProvider',
];

// event name -> pantheon hook name. The Rust side owns this table; the shim
// only needs to know which of the two names it was asked for so it can pick
// the right handler bucket.
const PANTHEON_HOOKS = ['pre_llm_call','pre_api_request','post_api_request','pre_gateway_dispatch'];
const ALIASES = {
  before_prompt_build:'pre_llm_call', before_llm_call:'pre_llm_call',
  pre_llm_call:'pre_llm_call', prompt_build:'pre_llm_call', before_prompt:'pre_llm_call',
  message_received:'pre_gateway_dispatch', pre_gateway_dispatch:'pre_gateway_dispatch',
  gateway_dispatch:'pre_gateway_dispatch', before_dispatch:'pre_gateway_dispatch',
  before_api_request:'pre_api_request', pre_api_request:'pre_api_request',
  api_request:'pre_api_request',
  after_api_request:'post_api_request', post_api_request:'post_api_request',
  api_response:'post_api_request',
  // Lifecycle observers. These have real fire sites in the runtime (see
  // `event_bridge`), so a foreign extension may bind them for real.
  on_session_start:'on_session_start', session_start:'on_session_start',
  on_session_end:'on_session_end', session_end:'on_session_end',
  session_finalize:'on_session_end',
  // Tool lifecycle. `pre_tool_call` is a GATE (can deny) and
  // `transform_tool_result` a TRANSFORM (can replace output); both are fired
  // inline by the host, not from the event bridge.
  before_tool_call:'pre_tool_call', pre_tool_call:'pre_tool_call',
  tool_call_before:'pre_tool_call',
  after_tool_call:'post_tool_call', post_tool_call:'post_tool_call',
  tool_call_after:'post_tool_call',
  transform_tool_result:'transform_tool_result',
  // Swarm. Both observers; fired from AgentSpawned/AgentCompleted.
  subagent_start:'subagent_start', agent_start:'subagent_start',
  subagent_stop:'subagent_stop', agent_end:'subagent_stop',
  // Stream lifecycle. `on_stream_delta` is opt-in (PANTHEON_HOOK_STREAM_DELTA).
  on_stream_start:'on_stream_start', on_stream_delta:'on_stream_delta',
  on_stream_end:'on_stream_end',
};

const handlers = {};
const api = {
  on(event, fn) {
    if (typeof event !== 'string' || typeof fn !== 'function') return;
    const target = ALIASES[event.trim().toLowerCase().replace(/[-:]/g,'_')];
    if (target) { (handlers[target] ||= []).push(fn); }
    else if (!out.dropped.includes(event)) { out.dropped.push(event); }
  },
  logger: { info(){}, warn(){}, error(){}, debug(){} },
  config: {},
  runtime: {},
};
for (const m of REFUSABLE) {
  api[m] = () => { if (!out.refused.includes(m)) out.refused.push(m); };
}

try {
  const mod = await import(pathToFileURL(entry).href);
  const register = mod.register ?? mod.default?.register ?? mod.default;
  if (typeof register !== 'function') throw new Error('extension exports no register(api)');
  await register(api);

  const bucket = handlers[hookName] || [];
  const payload = JSON.parse(payloadJson);
  const pieces = [];
  let directive = null;
  for (const fn of bucket) {
    // Handlers are called (event, ctx) in the foreign contract; the payload
    // stands in for both so a handler written for either shape gets data.
    const r = await fn(payload, payload);
    if (r && typeof r === 'object' && typeof r.context === 'string' && r.context.trim()) {
      pieces.push(r.context);
    } else if (typeof r === 'string' && r.trim()) {
      pieces.push(r);
    }
    // Gate/transform directive, first one wins (mirrors the Python shim and
    // Hermes' first-non-None contract). Only object returns carry one.
    if (!directive && r && typeof r === 'object') {
      const d = {};
      if (r.deny) d.deny = true;
      if (r.reason) d.reason = String(r.reason);
      if (r.replacement !== undefined && r.replacement !== null) d.replacement = String(r.replacement);
      if (Object.keys(d).length) directive = d;
    }
  }
  out.context = pieces.length ? pieces.join('\n') : null;
  out.directive = directive;
} catch (e) {
  out.error = `${e && e.name ? e.name : 'Error'}: ${e && e.message ? e.message : e}`;
}
process.stdout.write(JSON.stringify(out));
"#;

/// Fire one hook against a foreign extension, for its context return value.
///
/// `Ok(None)` means silent **or** fail-open; use [`fire_js_hook_full`] when
/// that difference matters (a gate must be able to fail closed).
pub fn fire_hook(
    plugin: &JsPlugin,
    hook: Hook,
    input: &HookInput,
    cfg: &JsRunnerConfig,
) -> Result<Option<String>, pantheon_core::error::PantheonError> {
    Ok(fire_js_hook_full(plugin, hook, input, cfg)
        .ok()
        .and_then(|o| o.context)
        .filter(|c| !c.trim().is_empty()))
}

/// Fire one hook and return the full answer, including `Err` for
/// infrastructure failure (spawn, crash, timeout, unparseable output) and the
/// extension's own `error` when it threw inside the shim.
///
/// A gate reads both: an extension that threw, crashed, or never answered is
/// not an extension that allowed the action.
pub fn fire_js_hook_full(
    plugin: &JsPlugin,
    hook: Hook,
    input: &HookInput,
    cfg: &JsRunnerConfig,
) -> Result<HookOutput, pantheon_core::error::PantheonError> {
    let payload =
        serde_json::to_string(input).map_err(|e| xerr("EXT_INPUT_ENCODE", e.to_string(), false))?;
    let mut child = Command::new(&cfg.runtime)
        .arg("--input-type=module")
        .arg("-e")
        .arg(SHIM)
        .arg(plugin.entry.to_string_lossy().to_string())
        .arg(hook.name())
        .arg(&payload)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| xerr("EXT_JS_SPAWN", e.to_string(), false))?;

    let timeout = cfg.timeout;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut buf = String::new();
                use std::io::Read;
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut buf);
                }
                if !status.success() {
                    return Err(xerr("EXT_JS_CRASH", format!("exited {status}"), false));
                }
                return serde_json::from_str(buf.trim())
                    .map_err(|e| xerr("EXT_JS_BAD_OUTPUT", e.to_string(), false));
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(xerr(
                        "EXT_JS_TIMEOUT",
                        format!("{}s", timeout.as_secs()),
                        true,
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(xerr("EXT_JS_WAIT", e.to_string(), true)),
        }
    }
}

/// Fire a hook and return the full envelope, for the doctor. Unlike
/// `fire_hook` this does not collapse the diagnostics away.
pub fn fire_hook_verbose(
    plugin: &JsPlugin,
    hook: Hook,
    input: &HookInput,
    cfg: &JsRunnerConfig,
) -> Result<HookOutput, pantheon_core::error::PantheonError> {
    let payload =
        serde_json::to_string(input).map_err(|e| xerr("EXT_INPUT_ENCODE", e.to_string(), false))?;
    let out = Command::new(&cfg.runtime)
        .arg("--input-type=module")
        .arg("-e")
        .arg(SHIM)
        .arg(plugin.entry.to_string_lossy().to_string())
        .arg(hook.name())
        .arg(&payload)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| xerr("EXT_JS_SPAWN", e.to_string(), false))?;
    serde_json::from_slice(&out.stdout).map_err(|e| {
        xerr(
            "EXT_JS_BAD_OUTPUT",
            format!("{}: {}", e, String::from_utf8_lossy(&out.stderr).trim()),
            false,
        )
    })
}

#[cfg(test)]
#[path = "js_runner_tests.rs"]
mod tests;
