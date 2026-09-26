//! Python hook runner: spawns `__init__.py`-style plugins in a subprocess.
//! V0 contract: one JSON line in on stdin, one JSON line out on stdout.
//! Fail-open: any crash/timeout/bad output => Ok(None), never a broken turn.
use crate::hooks::Hook;
use crate::manifest::PluginManifest;
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn xerr(code: &str, cause: String, retryable: bool) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Extension,
        retryable,
        cause,
        "check plugin dir, python3, and hook timeout",
        "",
    )
}

/// What the runner sends to the plugin process.
#[derive(Debug, Clone, Serialize)]
pub struct HookInput {
    pub hook: String,
    pub session_id: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, String>,
}

/// What a plugin process may return. `context` = inject (existing contract);
/// `directive` = a gate/transform decision (see [`crate::hooks::HookClass`]).
/// null/{} = silent.
#[derive(Debug, Clone, Deserialize)]
pub struct HookOutput {
    #[serde(default)]
    pub context: Option<String>,
    /// A gate deny or a transform replacement. `deny: true` blocks the gated
    /// action; `replacement` (when set) replaces the payload. Both fail open
    /// on a *failing* plugin (see manager), while an explicit directive is
    /// authoritative when the plugin answers cleanly.
    #[serde(default)]
    pub directive: Option<HookDirective>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A plugin's answer to a gate or transform hook.
#[derive(Debug, Clone, Deserialize)]
pub struct HookDirective {
    /// Gate: block the action. Fails the turn closed when true.
    #[serde(default)]
    pub deny: bool,
    /// Human-readable reason for a deny (gate) — surfaced to the model/user.
    #[serde(default)]
    pub reason: Option<String>,
    /// Transform: replacement payload. Empty/None means "no change".
    #[serde(default)]
    pub replacement: Option<String>,
}

/// A loaded Python plugin on disk.
#[derive(Debug, Clone)]
pub struct PythonPlugin {
    pub dir: PathBuf,
    pub manifest: PluginManifest,
}

impl PythonPlugin {
    pub fn load(dir: &Path) -> Result<Self, PantheonError> {
        let manifest = PluginManifest::load(&dir.join("plugin.yaml"))?;
        if !dir.join("__init__.py").exists() {
            return Err(xerr(
                "EXT_NO_ENTRY",
                format!("{} has no __init__.py", dir.display()),
                false,
            ));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
        })
    }
    pub fn provides(&self, hook: Hook) -> bool {
        self.manifest.hook_list().0.contains(&hook)
    }
}

/// Runner config.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub python: String,
    pub timeout: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            python: "python3".into(),
            timeout: Duration::from_secs(10),
        }
    }
}

/// The shim: imports the plugin `__init__`, calls `register(ctx)` with a
/// capturing ctx, dispatches the requested hook, prints result JSON.
const SHIM: &str = r#"
import importlib.util, json, sys
plug_dir, hook_name, payload_json = sys.argv[1], sys.argv[2], sys.argv[3]
result = {"context": None}
try:
    spec = importlib.util.spec_from_file_location(
        "pantheon_plugin", plug_dir + "/__init__.py")
    mod = importlib.util.module_from_spec(spec)
    sys.modules["pantheon_plugin"] = mod
    spec.loader.exec_module(mod)
    payload = json.loads(payload_json)
    calls = {}
    class Ctx:
        def register_hook(self, name, fn):
            calls.setdefault(name, []).append(fn)
    if not hasattr(mod, "register"):
        raise RuntimeError("plugin has no register(ctx)")
    mod.register(Ctx())
    out = None
    directive = None
    # Hermes plugins take FLAT kwargs (a `transform_tool_result` handler
    # expects `result=`, a `pre_tool_call` handler expects `tool=`/`args=`).
    # HookInput nests per-call context under `extra`, so lift it to the top
    # level here. Reserved names (hook/session_id/platform) win on collision,
    # so `extra` can never shadow the envelope's own identity.
    call = dict(payload.get("extra") or {})
    for k in ("hook", "session_id", "platform"):
        if k in payload:
            call[k] = payload[k]
    for fn in calls.get(hook_name, []):
        r = fn(**call)
        if not isinstance(r, dict):
            continue
        if r.get("context"):
            c = str(r["context"])
            out = (out + "\n" + c) if out else c
        # Gate/transform directives: first one wins, mirroring Hermes'
        # first-non-None contract. Keys are read flat so a plugin can answer
        # `{"deny": True, "reason": "..."}` or `{"replacement": "..."}`.
        if directive is None:
            d = {}
            if r.get("deny"):
                d["deny"] = True
            if r.get("reason"):
                d["reason"] = str(r["reason"])
            if r.get("replacement") is not None:
                d["replacement"] = str(r["replacement"])
            if d:
                directive = d
    result = {"context": out, "directive": directive}
except Exception as e:
    result = {"context": None, "directive": None,
              "error": f"{type(e).__name__}: {e}"}
print(json.dumps(result))
"#;

/// Fire one hook for its context return value. `Ok(Some(ctx))` = inject,
/// `Ok(None)` = silent. Infrastructure failures (crash, timeout, bad output)
/// are fail-open here: a context hook must never break a turn.
pub fn fire_hook(
    plugin: &PythonPlugin,
    hook: Hook,
    input: &HookInput,
    cfg: &RunnerConfig,
) -> Result<Option<String>, PantheonError> {
    Ok(fire_hook_full(plugin, hook, input, cfg)
        .ok()
        .and_then(|o| o.context)
        .filter(|c| !c.trim().is_empty()))
}

/// Fire one hook and return the full answer, INCLUDING an error for
/// infrastructure failure.
///
/// This distinction is the whole point: a context hook can collapse failure
/// into silence, but a *gate* must be able to tell "the plugin allowed this"
/// apart from "the plugin never answered". So this returns `Err` on spawn
/// failure, crash, timeout, and unparseable output; a clean answer — even an
/// empty one — is `Ok`.
pub fn fire_hook_full(
    plugin: &PythonPlugin,
    hook: Hook,
    input: &HookInput,
    cfg: &RunnerConfig,
) -> Result<HookOutput, PantheonError> {
    let payload =
        serde_json::to_string(input).map_err(|e| xerr("EXT_INPUT_ENCODE", e.to_string(), false))?;
    let mut child = Command::new(&cfg.python)
        .arg("-c")
        .arg(SHIM)
        .arg(plugin.dir.to_string_lossy().to_string())
        .arg(hook.name())
        .arg(payload)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| xerr("EXT_SPAWN", e.to_string(), false))?;
    let timeout = cfg.timeout;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                use std::io::Read;
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut out);
                }
                if !status.success() {
                    return Err(xerr("EXT_CRASH", format!("plugin exited {status}"), false));
                }
                return serde_json::from_str(out.trim())
                    .map_err(|e| xerr("EXT_BAD_OUTPUT", e.to_string(), false));
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(xerr("EXT_TIMEOUT", format!("{}s", timeout.as_secs()), true));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(xerr("EXT_WAIT", e.to_string(), true)),
        }
    }
}
