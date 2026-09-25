//! User-defined memory plugins.
//!
//! Drop a TOML manifest at `<data_dir>/memory-plugins/<name>.toml` and
//! the backend becomes selectable like any built-in:
//!
//! ```toml
//! name = "mybrain"
//! label = "MyBrain memory service"
//! kind = "http"              # or "stdio"
//! url = "http://127.0.0.1:9000"   # http
//! key = "token"                  # http (optional)
//! prefix = "/v1/memory"          # http (optional; protocol mount point)
//! command = "python3"            # stdio
//! args = ["mybrain_bridge.py"]   # stdio
//! timeout_ms = 5000              # stdio (optional, default 5000)
//! ```
//!
//! Selection options (`memory backend select NAME url=... key=...`) override
//! manifest values, so one manifest can serve several deployments.
//!
//! The stdio protocol is one JSON request line in, one JSON response line
//! out. Requests: `{"op":"recall"|"write"|"list_agent"|"get"|"forget"|"confirm", ...}`.
//! Responses: `{"ok":true, ...}` or `{"ok":false,"code":"...","cause":"..."}`.
//! Process-per-call (same model as hook execution): simple, isolated, and
//! a hanging plugin cannot poison the runtime — it is killed on timeout.
//!
//! Policy note: this crate never asks a plugin whether a write is allowed.
//! Gate (`write_via` / `recall_via` / `confirm_via`) runs before the plugin
//! is invoked; plugins only store/return what they are handed.
use crate::backend::BackendKind;
use crate::{BackendInfo, BackendRegistry, BackendSelection, LayerKind};
use pantheon_core::error::{Layer, PantheonError};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn perr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Memory,
        false,
        cause,
        "check the memory plugin manifest and its service",
        "",
    )
}

/// One user plugin manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryPluginManifest {
    pub name: String,
    #[serde(default)]
    pub label: String,
    /// "http" (default) or "stdio".
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_kind() -> String {
    "http".into()
}
fn default_timeout_ms() -> u64 {
    5_000
}

impl MemoryPluginManifest {
    pub fn label_or_default(&self) -> String {
        if self.label.trim().is_empty() {
            format!("{} (user memory plugin)", self.name)
        } else {
            self.label.clone()
        }
    }

    /// Validate required fields per kind. Returns a structured error so
    /// `backend list` / session startup can report *why* a plugin was skipped.
    pub fn validate(&self) -> Result<(), PantheonError> {
        if self.name.trim().is_empty() {
            return Err(perr("MEM_PLUGIN_MANIFEST", "manifest has no name".into()));
        }
        match self.kind.as_str() {
            "http" => {
                if self.url.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(perr(
                        "MEM_PLUGIN_MANIFEST",
                        format!("{}: kind=http requires `url`", self.name),
                    ));
                }
            }
            "stdio" => {
                if self.command.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(perr(
                        "MEM_PLUGIN_MANIFEST",
                        format!("{}: kind=stdio requires `command`", self.name),
                    ));
                }
            }
            other => {
                return Err(perr(
                    "MEM_PLUGIN_MANIFEST",
                    format!("{}: unknown kind '{other}' (http|stdio)", self.name),
                ))
            }
        }
        Ok(())
    }
}

/// Parse a manifest from TOML text.
pub fn parse_manifest(text: &str) -> Result<MemoryPluginManifest, PantheonError> {
    let m: MemoryPluginManifest = toml::from_str(text)
        .map_err(|e| perr("MEM_PLUGIN_MANIFEST", format!("invalid manifest TOML: {e}")))?;
    m.validate()?;
    Ok(m)
}

/// Load every `*.toml` manifest in `<data_dir>/memory-plugins` and register
/// it. Bad manifests are skipped with a loud line, never a panic: one broken
/// file must not brick the registry. Returns the names registerable after
/// the load (good ones), so callers can report what happened.
pub fn load_dir(reg: &mut BackendRegistry, data_dir: &Path) -> Vec<String> {
    let dir = data_dir.join("memory-plugins");
    let mut loaded = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return loaded, // no plugins dir: normal case
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("memory plugin {}: unreadable: {e}", path.display());
                continue;
            }
        };
        match parse_manifest(&text) {
            Ok(m) => {
                // File name wins over a mismatched manifest name so the
                // selection can always be derived from `ls`.
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&m.name)
                    .to_string();
                let name = stem;
                register_manifest(reg, name.clone(), m);
                loaded.push(name);
            }
            Err(e) => eprintln!("memory plugin {}: skipped: {e}", path.display()),
        }
    }
    loaded.sort();
    loaded
}

fn register_manifest(reg: &mut BackendRegistry, name: String, m: MemoryPluginManifest) {
    let info = BackendInfo {
        name: name.clone(),
        label: m.label_or_default(),
        kind: if m.kind == "stdio" {
            BackendKind::Subprocess
        } else {
            BackendKind::Http
        },
        capabilities: vec!["memory.read".into(), "memory.write".into()],
    };
    reg.register_with(info, move |sel: &BackendSelection| {
        // Selection options override manifest values, so one manifest can
        // serve several deployments (dev/staging pointers, rotated keys).
        let opt = |k: &str| sel.options.get(k).cloned();
        if m.kind == "stdio" {
            let command = opt("command")
                .or_else(|| m.command.clone())
                .unwrap_or_default();
            let args = if sel.options.contains_key("args") {
                split_args(opt("args").unwrap_or_default())
            } else {
                m.args.clone()
            };
            let timeout_ms = opt("timeout_ms")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(m.timeout_ms);
            return Ok(Arc::new(StdioBackend::new(command, args, timeout_ms))
                as Arc<dyn crate::MemoryBackend>);
        }
        let base = opt("url")
            .or_else(|| m.url.clone())
            .ok_or_else(|| perr("MEM_PLUGIN_MANIFEST", format!("{name}: no url")))?;
        let key = opt("key").or_else(|| m.key.clone());
        let prefix = opt("prefix")
            .or_else(|| m.prefix.clone())
            .unwrap_or_else(|| "/v1/memory".into());
        Ok(Arc::new(crate::http_backend::HttpBackend::with_prefix(
            base, key, prefix,
        )) as Arc<dyn crate::MemoryBackend>)
    });
}

/// `args="a b c"` option form (selection options are flat strings).
fn split_args(s: String) -> Vec<String> {
    s.split_whitespace().map(|t| t.to_string()).collect()
}

fn layer_tag(layer: LayerKind) -> &'static str {
    match layer {
        LayerKind::Global => "Global",
        LayerKind::Agent => "Agent",
        LayerKind::Project => "Project",
        LayerKind::TaskSession => "TaskSession",
        LayerKind::EphemeralTurn => "EphemeralTurn",
    }
}

/// Subprocess backend: one JSON request line in, one JSON response line out.
/// Spawned per call (same model as hook execution) so a wedged plugin is
/// killed on timeout instead of holding the runtime.
#[derive(Debug, Clone)]
pub struct StdioBackend {
    command: String,
    args: Vec<String>,
    timeout: Duration,
}

impl StdioBackend {
    pub fn new(command: String, args: Vec<String>, timeout_ms: u64) -> Self {
        Self {
            command,
            args,
            timeout: Duration::from_millis(timeout_ms.max(1)),
        }
    }

    /// One request/response exchange. Never panics; timeout kills the child.
    fn call(&self, req: &serde_json::Value) -> Result<serde_json::Value, PantheonError> {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        let mut child = Command::new(&self.command)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                perr(
                    "MEM_PLUGIN_SPAWN",
                    format!("spawning {}: {e}", self.command),
                )
            })?;
        {
            let mut stdin = child.stdin.take().ok_or_else(|| {
                perr("MEM_PLUGIN_IO", format!("{}: stdin unavailable", self.command))
            })?;
            let line = format!("{req}\n");
            stdin.write_all(line.as_bytes()).map_err(|e| {
                perr("MEM_PLUGIN_IO", format!("writing to {}: {e}", self.command))
            })?;
        }
        let stdout = child.stdout.take().ok_or_else(|| {
            perr("MEM_PLUGIN_IO", format!("{}: stdout unavailable", self.command))
        })?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let res = reader.read_line(&mut line).map(|_| line);
            let _ = tx.send(res);
        });
        let line = match rx.recv_timeout(self.timeout) {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => {
                return Err(perr(
                    "MEM_PLUGIN_IO",
                    format!("reading from {}: {e}", self.command),
                ))
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(perr(
                    "MEM_PLUGIN_TIMEOUT",
                    format!(
                        "{} did not answer within {}ms",
                        self.command,
                        self.timeout.as_millis()
                    ),
                ));
            }
        };
        let line = line.trim();
        if line.is_empty() {
            return Err(perr(
                "MEM_PLUGIN_EMPTY",
                format!("{} produced no response", self.command),
            ));
        }
        let v: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            perr(
                "MEM_PLUGIN_DECODE",
                format!("{} returned invalid JSON: {e}", self.command),
            )
        })?;
        if v.get("ok").and_then(|b| b.as_bool()) == Some(true) {
            Ok(v)
        } else {
            let code = v
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("MEM_PLUGIN_ERROR")
                .to_string();
            let cause = v
                .get("cause")
                .and_then(|c| c.as_str())
                .unwrap_or("plugin reported failure")
                .to_string();
            Err(perr(&code, cause))
        }
    }
}

impl crate::MemoryBackend for StdioBackend {
    fn recall(
        &self,
        _policy: &pantheon_core::capability::Policy,
        layers: &[LayerKind],
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::Recalled>, PantheonError> {
        let names: Vec<&str> = layers.iter().map(|l| layer_tag(*l)).collect();
        let resp = self.call(&serde_json::json!({
            "op": "recall",
            "query": query,
            "limit": limit,
            "layers": names,
        }))?;
        serde_json::from_value(resp.get("hits").cloned().unwrap_or(serde_json::json!([]))).map_err(
            |e| {
                perr(
                    "MEM_PLUGIN_DECODE",
                    format!("recall hits invalid: {e}"),
                )
            },
        )
    }

    fn write(
        &self,
        _policy: &pantheon_core::capability::Policy,
        proposal: crate::Proposal,
        max_bytes: usize,
    ) -> Result<crate::MemoryRecord, PantheonError> {
        let resp = self.call(&serde_json::json!({
            "op": "write",
            "layer": layer_tag(proposal.layer),
            "namespace": proposal.namespace,
            "key": proposal.key,
            "value": proposal.value,
            "provenance": proposal.provenance,
            "max_bytes": max_bytes,
        }))?;
        serde_json::from_value(resp.get("record").cloned().unwrap_or(serde_json::Value::Null))
            .map_err(|e| {
                perr(
                    "MEM_PLUGIN_DECODE",
                    format!("write record invalid: {e}"),
                )
            })
    }

    fn list_agent(&self, namespace: &str) -> Result<Vec<(String, String)>, PantheonError> {
        let resp = self.call(&serde_json::json!({
            "op": "list_agent",
            "namespace": namespace,
        }))?;
        serde_json::from_value(resp.get("rows").cloned().unwrap_or(serde_json::json!([])))
            .map_err(|e| {
                perr(
                    "MEM_PLUGIN_DECODE",
                    format!("list_agent rows invalid: {e}"),
                )
            })
    }

    fn get(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<crate::MemoryRecord>, PantheonError> {
        let resp = self.call(&serde_json::json!({
            "op": "get",
            "namespace": namespace,
            "key": key,
        }))?;
        if resp.get("found").and_then(|f| f.as_bool()) != Some(true) {
            return Ok(None);
        }
        serde_json::from_value(resp.get("record").cloned().unwrap_or(serde_json::Value::Null))
            .map(Some)
            .map_err(|e| perr("MEM_PLUGIN_DECODE", format!("get record invalid: {e}")))
    }

    fn forget(
        &self,
        layer: LayerKind,
        namespace: &str,
        key: &str,
    ) -> Result<bool, PantheonError> {
        let resp = self.call(&serde_json::json!({
            "op": "forget",
            "layer": layer_tag(layer),
            "namespace": namespace,
            "key": key,
        }))?;
        Ok(resp.get("removed").and_then(|r| r.as_bool()).unwrap_or(false))
    }

    fn confirm(
        &self,
        _policy: &pantheon_core::capability::Policy,
        namespace: &str,
        key: &str,
    ) -> Result<crate::MemoryRecord, PantheonError> {
        let resp = self.call(&serde_json::json!({
            "op": "confirm",
            "namespace": namespace,
            "key": key,
        }))?;
        serde_json::from_value(resp.get("record").cloned().unwrap_or(serde_json::Value::Null))
            .map_err(|e| perr("MEM_PLUGIN_DECODE", format!("confirm record invalid: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryBackend;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pantheon-memplug-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn manifest_validation_reports_the_missing_field() {
        let err = parse_manifest("name = \"x\"\nkind = \"http\"\n").unwrap_err();
        assert_eq!(err.code, "MEM_PLUGIN_MANIFEST");
        assert!(err.cause.contains("url"), "{}", err.cause);
        let err2 = parse_manifest("name = \"x\"\nkind = \"stdio\"\n").unwrap_err();
        assert!(err2.cause.contains("command"), "{}", err2.cause);
        let err3 = parse_manifest("name = \"x\"\nkind = \"carrier-pigeon\"\n").unwrap_err();
        assert!(err3.cause.contains("unknown kind"), "{}", err3.cause);
    }

    #[test]
    fn load_dir_registers_user_plugins_and_skips_broken_ones() {
        let dir = tmp_dir("load");
        let plug = dir.join("memory-plugins");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(
            plug.join("mybrain.toml"),
            "name = \"ignored-name\"\nlabel = \"MyBrain\"\nkind = \"http\"\nurl = \"http://127.0.0.1:9000\"\nprefix = \"/memory\"\n",
        )
        .unwrap();
        std::fs::write(plug.join("broken.toml"), "name = \"\"\n").unwrap();
        let mut reg = BackendRegistry::with_defaults();
        let loaded = load_dir(&mut reg, &dir);
        // File stem wins; broken file skipped without killing the load.
        assert_eq!(loaded, vec!["mybrain".to_string()]);
        assert!(reg.contains("mybrain"));
        let info = reg.info("mybrain").unwrap();
        assert_eq!(info.label, "MyBrain");
        assert_eq!(info.kind, BackendKind::Http);
        let backend = reg
            .instantiate_selected(&BackendSelection {
                name: "mybrain".into(),
                options: Default::default(),
            })
            .unwrap();
        // Offline: construction succeeds, first call fails structured.
        let err = backend.list_agent("nyx").unwrap_err();
        assert_eq!(err.code, "MEM_HTTP_CONN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_options_override_manifest_url() {
        let dir = tmp_dir("override");
        let plug = dir.join("memory-plugins");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(
            plug.join("mybrain.toml"),
            "name = \"mybrain\"\nkind = \"http\"\nurl = \"http://127.0.0.1:9/old\"\n",
        )
        .unwrap();
        let mut reg = BackendRegistry::with_defaults();
        load_dir(&mut reg, &dir);
        let backend = reg
            .instantiate_selected(&BackendSelection {
                name: "mybrain".into(),
                options: [("url".to_string(), "http://127.0.0.1:9/new".to_string())]
                    .into_iter()
                    .collect(),
            })
            .unwrap();
        let err = backend.list_agent("nyx").unwrap_err();
        assert!(err.cause.contains("/new"), "{}", err.cause);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write a tiny shell responder: reads one line, prints one JSON line.
    fn responder(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("responder.sh");
        std::fs::write(&p, format!("#!/bin/sh
read -r line
{body}
")).unwrap();
        p
    }

    fn ok_script(dir: &Path, json: &str) -> StdioBackend {
        let p = responder(dir, &format!("printf '%s\\n' '{json}'"));
        StdioBackend::new("sh".into(), vec![p.to_string_lossy().to_string()], 2000)
    }

    /// A stdio plugin answers a recall request; proves the whole path
    /// (spawn -> JSON in -> JSON out -> typed hits).
    #[test]
    fn stdio_backend_recall_round_trips() {
        let dir = tmp_dir("recall");
        let backend = ok_script(&dir, r#"{"ok":true,"hits":[]}"#);
        let policy = pantheon_core::capability::Policy::coder_with_memory();
        let hits = backend
            .recall(&policy, &[LayerKind::Agent], "anything", 5)
            .unwrap();
        assert!(hits.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plugin that reports failure maps to its own structured code.
    #[test]
    fn stdio_backend_error_response_maps_to_code() {
        let dir = tmp_dir("err");
        let backend = ok_script(
            &dir,
            r#"{"ok":false,"code":"MYBRAIN_DOWN","cause":"index missing"}"#,
        );
        let policy = pantheon_core::capability::Policy::coder_with_memory();
        let err = backend
            .recall(&policy, &[LayerKind::Agent], "q", 5)
            .unwrap_err();
        assert_eq!(err.code, "MYBRAIN_DOWN");
        assert!(err.cause.contains("index missing"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hung plugin is killed, never allowed to wedge the runtime.
    #[test]
    fn stdio_backend_timeout_kills_the_child() {
        let backend = StdioBackend::new("sleep".into(), vec!["5".into()], 200);
        let policy = pantheon_core::capability::Policy::coder_with_memory();
        let err = backend
            .recall(&policy, &[LayerKind::Agent], "q", 5)
            .unwrap_err();
        assert_eq!(err.code, "MEM_PLUGIN_TIMEOUT");
    }

    /// A manifest-declared stdio plugin is selectable by name and its
    /// command/args come from the manifest.
    #[test]
    fn stdio_manifest_plugin_is_selectable() {
        let dir = tmp_dir("stdio");
        let plug = dir.join("memory-plugins");
        std::fs::create_dir_all(&plug).unwrap();
        let script = responder(&dir, r#"printf '%s\n' '{"ok":true,"rows":[["k","v"]]}'"#);
        let manifest = format!(
            "name = \"shplug\"\nlabel = \"shell plugin\"\nkind = \"stdio\"\ncommand = \"sh\"\nargs = [\"{}\"]\ntimeout_ms = 2000\n",
            script.to_string_lossy()
        );
        std::fs::write(plug.join("shplug.toml"), manifest).unwrap();
        let mut reg = BackendRegistry::with_defaults();
        let loaded = load_dir(&mut reg, &dir);
        assert_eq!(loaded, vec!["shplug".to_string()]);
        assert_eq!(
            reg.info("shplug").unwrap().kind,
            crate::backend::BackendKind::Subprocess
        );
        let backend = reg
            .instantiate_selected(&BackendSelection {
                name: "shplug".into(),
                options: Default::default(),
            })
            .unwrap();
        let rows = backend.list_agent("nyx").unwrap();
        assert_eq!(rows, vec![("k".to_string(), "v".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
