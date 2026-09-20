//! plugin.yaml loader. Accepts both Hermes shapes seen in the wild:
//! `provides_hooks: [...]` (anti-ai-writing) and `hooks: [...]`
//! (time-gap), plus manifest_version/api_version variance.
use crate::hooks::Hook;
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::Path;

fn xerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(code, Layer::Extension, false, cause,
        "fix plugin.yaml and reload", "")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub provides_hooks: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Pantheon extension: fire at most once per (plugin, hook, session).
    /// Covers Hermes plugins that dedup in-process via `_seen_sessions`,
    /// which our subprocess runner cannot preserve across fires.
    #[serde(default)]
    pub once_per_session: bool,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_yaml::Value>,
}

impl PluginManifest {
    pub fn load(path: &Path) -> Result<Self, PantheonError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| xerr("EXT_MANIFEST_READ", e.to_string()))?;
        serde_yaml::from_str(&text).map_err(|e| xerr("EXT_MANIFEST_PARSE", e.to_string()))
    }
    /// Union of both hook spellings, parsed. Unknown names are reported, not fatal.
    pub fn hook_list(&self) -> (Vec<Hook>, Vec<String>) {
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for raw in self.provides_hooks.iter().chain(self.hooks.iter()) {
            if !seen.insert(raw.clone()) { continue; }
            match Hook::parse(raw) {
                Some(h) => known.push(h),
                None => unknown.push(raw.clone()),
            }
        }
        (known, unknown)
    }

    /// Fire-at-most-once-per-session when the manifest says so, or when the
    /// plugin dir's `__init__.py` dedups in-process via `_seen_sessions`
    /// (anti-ai-writing shape). The subprocess runner spawns fresh per fire,
    /// so the manager must own this — the plugin process cannot.
    /// `dir` is the plugin dir (for the `__init__.py` sniff); `None` skips sniffing.
    pub fn once_per_session(&self, dir: Option<&Path>) -> bool {
        if self.once_per_session {
            return true;
        }
        if let Some(d) = dir {
            if let Ok(text) = std::fs::read_to_string(d.join("__init__.py")) {
                if text.contains("_seen_sessions") {
                    return true;
                }
            }
        }
        false
    }
}
