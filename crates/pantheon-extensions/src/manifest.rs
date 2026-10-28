//! plugin.yaml loader. Accepts both Hermes shapes seen in the wild:
//! `provides_hooks: [...]` (anti-ai-writing) and `hooks: [...]`
//! (time-gap), plus manifest_version/api_version variance.
use crate::hooks::Hook;
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::Path;

fn xerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Extension,
        false,
        cause,
        "fix plugin.yaml and reload",
        "",
    )
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
    /// Enabled by default after install. The `[plugins.<name>]` config
    /// entry wins when present; when absent this flag is the default
    /// (false for every bundled plugin except noisegate).
    #[serde(default)]
    pub enabled: bool,
    /// True when `plugin.yaml` explicitly sets `enabled` (set by
    /// [`PluginManifest::load`], not by serde: an absent key and an
    /// explicit `false` must differ, because a third-party plugin whose
    /// manifest explicitly says `enabled: false` must not fire even
    /// when approved, while one that merely omits the key keeps the
    /// old approved-means-loaded behaviour).
    #[serde(skip, default)]
    pub enabled_set: bool,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_yaml::Value>,
}

impl PluginManifest {
    pub fn load(path: &Path) -> Result<Self, PantheonError> {
        let text =
            std::fs::read_to_string(path).map_err(|e| xerr("EXT_MANIFEST_READ", e.to_string()))?;
        // Fail closed on a top-level `key`: serde would otherwise
        // silently ignore it, leaving a credential in a manifest that
        // can be shared. Credentials go through the secrets broker,
        // never through a manifest file (see the tool-plugin loader's
        // `parse_manifest` for the same rule).
        if let Ok(serde_yaml::Value::Mapping(map)) =
            serde_yaml::from_str::<serde_yaml::Value>(&text)
        {
            if map.contains_key(serde_yaml::Value::String("key".to_string())) {
                return Err(xerr(
                    "EXT_MANIFEST_KEY",
                    "manifest sets a `key`: API keys must never live in plugin manifests, which can be shared; provide credentials through the secrets broker instead".to_string(),
                ));
            }
        }
        let mut m: Self =
            serde_yaml::from_str(&text).map_err(|e| xerr("EXT_MANIFEST_PARSE", e.to_string()))?;
        // Presence, not just value: see `enabled_set`.
        m.enabled_set = serde_yaml::from_str::<serde_yaml::Value>(&text)
            .ok()
            .and_then(|v| v.as_mapping().cloned())
            .is_some_and(|map| {
                map.contains_key(serde_yaml::Value::String("enabled".to_string()))
            });
        Ok(m)
    }
    /// Union of both hook spellings, parsed. Unknown names are reported, not fatal.
    pub fn hook_list(&self) -> (Vec<Hook>, Vec<String>) {
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for raw in self.provides_hooks.iter().chain(self.hooks.iter()) {
            if !seen.insert(raw.clone()) {
                continue;
            }
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
    /// so the manager must own this - the plugin process cannot.
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
