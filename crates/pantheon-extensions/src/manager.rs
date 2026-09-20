//! Extension manager: owns loaded plugins, fires hooks in manifest order,
//! concatenates injected contexts. Fail-open per plugin.
//!
//! Session-scoped dedup: plugins marked `once_per_session: true` in
//! plugin.yaml (or Hermes `__init__.py` plugins that keep in-process
//! `_seen_sessions`, which our subprocess runner can't preserve) fire at
//! most once per (plugin, hook, session). The manager owns this, not the
//! plugin process — required because each fire spawns fresh.
use crate::hooks::Hook;
use crate::python_runner::{fire_hook, HookInput, PythonPlugin, RunnerConfig};
use pantheon_core::error::{Layer, PantheonError};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(code, Layer::Extension, false, cause,
        "check extension dir and manifests", "")
}

pub struct ExtensionManager {
    plugins: Vec<PythonPlugin>,
    cfg: RunnerConfig,
    seen_once: Mutex<HashSet<(String, String, String)>>,
}

impl ExtensionManager {
    pub fn new(cfg: RunnerConfig) -> Self {
        Self { plugins: Vec::new(), cfg, seen_once: Mutex::new(HashSet::new()) }
    }

    /// Pre-seed seen keys (e.g. from `hook_seen.json` for fresh CLI processes).
    pub fn preseed_seen(&self, keys: HashSet<(String, String, String)>) {
        let mut seen = self.seen_once.lock().unwrap_or_else(|e| e.into_inner());
        seen.extend(keys);
    }

    /// Snapshot seen keys (e.g. to persist for the next CLI invocation).
    pub fn seen_snapshot(&self) -> HashSet<(String, String, String)> {
        self.seen_once.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Load every immediate subdir of `dir` containing plugin.yaml.
    pub fn load_dir(&mut self, dir: &Path) -> Result<Vec<String>, PantheonError> {
        let rd = std::fs::read_dir(dir)
            .map_err(|e| merr("EXT_DIR", e.to_string()))?;
        let mut names = Vec::new();
        for entry in rd {
            let entry = entry.map_err(|e| merr("EXT_DIR", e.to_string()))?;
            let p = entry.path();
            if p.is_dir() && p.join("plugin.yaml").exists() {
                match PythonPlugin::load(&p) {
                    Ok(pl) => {
                        names.push(pl.manifest.name.clone());
                        self.plugins.push(pl);
                    }
                    Err(e) => eprintln!("skip {}: {e}", p.display()),
                }
            }
        }
        self.plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        Ok(names)
    }
    pub fn load_one(&mut self, dir: &Path) -> Result<String, PantheonError> {
        let pl = PythonPlugin::load(dir)?;
        let name = pl.manifest.name.clone();
        self.plugins.push(pl);
        self.plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        Ok(name)
    }
    pub fn names(&self) -> Vec<String> {
        self.plugins.iter().map(|p| p.manifest.name.clone()).collect()
    }
    /// Fire `hook` on all providers, concatenate contexts. Never fails the turn.
    pub fn fire(&self, hook: Hook, session: &str, platform: &str) -> Option<String> {
        let mut parts = Vec::new();
        for pl in &self.plugins {
            if !pl.provides(hook) { continue; }
            if pl.manifest.once_per_session(Some(&pl.dir)) {
                let key = (pl.manifest.name.clone(), hook.name().to_string(), session.to_string());
                let mut seen = self.seen_once.lock().unwrap_or_else(|e| e.into_inner());
                if seen.contains(&key) { continue; }
                seen.insert(key);
            }
            let input = HookInput {
                hook: hook.name().into(),
                session_id: session.into(),
                platform: platform.into(),
                extra: Default::default(),
            };
            match fire_hook(pl, hook, &input, &self.cfg) {
                Ok(Some(ctx)) => parts.push(ctx),
                Ok(None) => {}
                Err(e) => eprintln!("plugin {}: {e}", pl.manifest.name),
            }
        }
        if parts.is_empty() { None } else { Some(parts.join("\n")) }
    }
    pub fn plugin_dir(&self, name: &str) -> Option<PathBuf> {
        self.plugins.iter().find(|p| p.manifest.name == name).map(|p| p.dir.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn plug(dir: &std::path::Path, name: &str, once: bool, ctx_text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let man = format!(
            "name: {name}\nprovides_hooks:\n  - pre_llm_call\n{}\n",
            if once { "once_per_session: true" } else { "" }
        );
        std::fs::write(dir.join("plugin.yaml"), man).unwrap();
        let init = format!(
            "def register(ctx):\n    ctx.register_hook('pre_llm_call', _h)\n\
             def _h(**kw):\n    return {{'context': '{}'}} \n",
            ctx_text
        );
        let mut f = std::fs::File::create(dir.join("__init__.py")).unwrap();
        f.write_all(init.as_bytes()).unwrap();
    }

    #[test]
    fn once_per_session_dedups() {
        let base = std::env::temp_dir().join(format!("pantheon-mgr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        plug(&base.join("p"), "p", true, "CTX");
        let mut m = ExtensionManager::new(RunnerConfig::default());
        m.load_dir(&base).unwrap();
        assert_eq!(m.fire(Hook::PreLlmCall, "s1", "cli"), Some("CTX".into()));
        assert_eq!(m.fire(Hook::PreLlmCall, "s1", "cli"), None);
        assert_eq!(m.fire(Hook::PreLlmCall, "s2", "cli"), Some("CTX".into()));
    }

    #[test]
    fn seen_sessions_sniff_dedups_without_manifest_flag() {
        let base = std::env::temp_dir().join(format!("pantheon-mgr2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let d = base.join("q");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin.yaml"), "name: q\nprovides_hooks:\n  - pre_llm_call\n").unwrap();
        std::fs::write(
            d.join("__init__.py"),
            "_seen_sessions = set()\ndef register(ctx):\n    ctx.register_hook('pre_llm_call', _h)\ndef _h(**kw):\n    return {'context': 'Q'}\n",
        )
        .unwrap();
        let mut m = ExtensionManager::new(RunnerConfig::default());
        m.load_dir(&base).unwrap();
        assert_eq!(m.fire(Hook::PreLlmCall, "s1", "cli"), Some("Q".into()));
        assert_eq!(m.fire(Hook::PreLlmCall, "s1", "cli"), None);
    }

    #[test]
    fn normal_plugin_fires_every_time() {
        let base = std::env::temp_dir().join(format!("pantheon-mgr3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        plug(&base.join("r"), "r", false, "R");
        let mut m = ExtensionManager::new(RunnerConfig::default());
        m.load_dir(&base).unwrap();
        assert_eq!(m.fire(Hook::PreLlmCall, "s1", "cli"), Some("R".into()));
        assert_eq!(m.fire(Hook::PreLlmCall, "s1", "cli"), Some("R".into()));
    }
}
