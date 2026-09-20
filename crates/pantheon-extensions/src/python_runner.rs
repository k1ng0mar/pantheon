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
    PantheonError::new(code, Layer::Extension, retryable, cause,
        "check plugin dir, python3, and hook timeout", "")
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

/// What a plugin process may return: null/{} = silent, {"context": ...} = inject.
#[derive(Debug, Clone, Deserialize)]
pub struct HookOutput {
    pub context: Option<String>,
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
            return Err(xerr("EXT_NO_ENTRY", format!("{} has no __init__.py", dir.display()), false));
        }
        Ok(Self { dir: dir.to_path_buf(), manifest })
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
        Self { python: "python3".into(), timeout: Duration::from_secs(10) }
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
    for fn in calls.get(hook_name, []):
        r = fn(**payload)
        if isinstance(r, dict) and r.get("context"):
            c = str(r["context"])
            out = (out + "\n" + c) if out else c
    result = {"context": out}
except Exception as e:
    result = {"context": None, "_error": f"{type(e).__name__}: {e}"}
print(json.dumps(result))
"#;

/// Fire one hook. Ok(Some(ctx)) = inject, Ok(None) = silent or fail-open.
pub fn fire_hook(
    plugin: &PythonPlugin,
    hook: Hook,
    input: &HookInput,
    cfg: &RunnerConfig,
) -> Result<Option<String>, PantheonError> {
    let payload = serde_json::to_string(input)
        .map_err(|e| xerr("EXT_INPUT_ENCODE", e.to_string(), false))?;
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
                    return Ok(None); // fail-open: plugin crashed
                }
                let parsed: HookOutput = match serde_json::from_str(out.trim()) {
                    Ok(p) => p,
                    Err(_) => return Ok(None), // fail-open: bad output
                };
                return Ok(parsed.context.filter(|c| !c.trim().is_empty()));
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(None); // fail-open: timeout
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(xerr("EXT_WAIT", e.to_string(), true)),
        }
    }
}
