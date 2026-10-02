//! Bundled plugin catalog: the plugins Pantheon ships. All disabled by
//! default; enabling one is always an explicit, recorded act.
//!
//! ## Plugins vs MCPs: the trust distinction
//!
//! This is the distinction the approval UI must surface, because the two
//! look similar (both extend what the agent can do) and are trusted
//! completely differently:
//!
//! - A **plugin** (tool or hook) is code Pantheon itself runs on the
//!   operator's machine. The runtime loads its manifest and executes it:
//!   tool plugins as spawned runner processes whose tools sit behind the
//!   capability gate, hook plugins as per-fire child processes with a
//!   scrubbed environment (PATH only). Either way the code lives on the
//!   operator's side of the trust boundary, running with the operator's
//!   user privileges - Pantheon does **not** sandbox plugins. Enabling a
//!   plugin is consent to run its code.
//!
//! - An **MCP server** is an out-of-process **integration**. Pantheon
//!   spawns or connects to a server and speaks the MCP protocol to it;
//!   Pantheon never executes the server's code, which may run on another
//!   machine entirely. Trust there is in the endpoint and its
//!   configuration (command, URL, env), not in shipped code.
//!
//! Bundled plugins are first-party - shipped in this repo, reviewed
//! in-tree - so they skip the third-party approval store
//! ([`pantheon_api::approval`], which exists to gate code from sources
//! the operator has not vetted). They do **not** skip enablement: every
//! catalog entry is disabled unless `[plugins.<name>]` says otherwise.
//!
//! ## Materialization
//!
//! The catalog ships as source in `vendor/` and is compiled into the
//! binary. [`seed`] materializes each entry into
//! `<ext_dir>/bundled/<name>/` on load - inert files, never enabled.
//! The `<dir>/bundled/` layout is what
//! [`pantheon_api::approval::is_bundled`] recognizes, and
//! [`crate::manager::ExtensionManager::load_dir`] scans it. An existing
//! dir is never overwritten.
//!
//! ## One enablement state
//!
//! The config file is the source of truth. `[plugins.<name>]` with
//! `enabled` (default false) is read and written identically by the
//! config file, the dashboard (`GET /api/plugins`,
//! `POST /api/plugins/:kind/:name/{approve,disable}`), the mobile app
//! (which talks to the same dashboard endpoints), and the agent's
//! `enable_plugin` tool. What wins, and why:
//!
//! - For a **bundled** plugin, the config entry wins over the plugin
//!   manifest's own `enabled` flag. The manifest is install metadata that
//!   ships with the plugin; the config is the operator's declared intent,
//!   and it is the one value every surface (TUI, dashboard, app, agent)
//!   can read and write. A bundled plugin whose manifest says enabled
//!   but whose config entry is absent or false does not load.
//! - For a **third-party** plugin, the approval store
//!   (`<scope>/.approvals.json`) remains the gate: unapproved third-party
//!   code never loads no matter what any flag says. Its manifest
//!   `enabled` flag is a per-install switch, unchanged by this module.
//!
//! ## Agent-initiated enable
//!
//! The agent may propose enabling a bundled plugin through the
//! `enable_plugin` tool (capability `plugin.enable`, which default
//! policies mark Approval). The run parks, an `ApprovalRequested` event
//! is appended to the ledger (audit-logged like every approval), and the
//! plugin is switched on only if the operator grants. It is never silent,
//! and only catalog names are accepted - there is no agent path to
//! install or enable arbitrary plugins.
//!
//! ## Stable interface
//!
//! The setup wizard's Extensions screen consumes this module. The stable
//! surface is [`BundledPlugin`], [`bundled_plugins`], [`find`],
//! [`is_enabled`], [`enable`], [`disable`], [`set_enabled`], [`seed`],
//! and [`PluginKind`]. New catalog
//! entries only add rows; they never change these shapes.

use pantheon_api::config::Config;
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::Path;

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Extension,
        false,
        cause,
        "check the plugin name against the bundled catalog",
        "",
    )
}

/// Which half of the plugin system a bundled plugin belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// A `manifest.yaml` plugin whose runner's tools are projected into
    /// the tool registry behind the capability gate.
    Tool,
    /// A `plugin.yaml` hook plugin fired per hook point by the
    /// [`crate::manager::ExtensionManager`].
    Hook,
}

impl PluginKind {
    /// Stable lowercase token used in config entries and API payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            PluginKind::Tool => "tool",
            PluginKind::Hook => "hook",
        }
    }

    /// Parse the [`PluginKind::as_str`] token back. Unknown tokens are
    /// `None` so a typo degrades to "not a kind" rather than a panic.
    pub fn parse(s: &str) -> Option<PluginKind> {
        match s.trim() {
            "tool" => Some(PluginKind::Tool),
            "hook" => Some(PluginKind::Hook),
            _ => None,
        }
    }
}

impl std::fmt::Display for PluginKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One plugin Pantheon ships. Most entries are disabled by default
/// [`is_enabled_with_default`] falls back to the catalog entry's own
/// manifest `enabled` flag when there is no `[plugins.<name>]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundledPlugin {
    /// Catalog name. Matches the plugin's manifest `name` and the
    /// `[plugins.<name>]` config key.
    pub name: String,
    /// Tool plugin or hook plugin.
    pub kind: PluginKind,
    /// Version shipped (parsed from the vendored manifest, so the
    /// catalog can never drift from what is actually in the repo).
    pub version: String,
    /// One-line description (also from the vendored manifest).
    pub description: String,
    /// The manifest's default-enabled flag (false unless the vendored
    /// `plugin.yaml` says otherwise). The `[plugins.<name>]` config entry
    /// wins when present.
    pub enabled: bool,
    /// What enabling this plugin actually authorizes, in plain terms.
    /// Shown by approval surfaces before the operator decides.
    pub privilege_notes: String,
}

/// One catalog row: the vendored manifest it is parsed from, the files
/// materialized on seed, plus the human-facing privilege notes.
struct CatalogRow {
    kind: PluginKind,
    /// Compile-time include of the shipped `plugin.yaml`, so version and
    /// description always match the vendored source.
    manifest_yaml: &'static str,
    /// `(relative path, contents)` pairs written under
    /// `<ext_dir>/bundled/<name>/` by [`seed`]. Add every file the plugin
    /// needs to load.
    files: &'static [(&'static str, &'static str)],
    privilege_notes: &'static str,
}

/// The catalog table. Add a row here (and vendor the plugin source) to
/// ship a new bundled plugin. Never list a plugin that is not in the repo.
fn catalog_rows() -> Vec<CatalogRow> {
    vec![CatalogRow {
        kind: PluginKind::Hook,
        manifest_yaml: include_str!("../../../vendor/time-gap-pantheon/plugin.yaml"),
        files: &[(
            "__init__.py",
            include_str!("../../../vendor/time-gap-pantheon/__init__.py"),
        )],
        privilege_notes: "Hook plugin (Python): injects an implicit time-gap \
            sense into the prompt on `pre_llm_call`, only when a gap or date \
            rollover crosses. Context-class hook - it may add text, never deny \
            an action, and a crash or timeout is fail-open (the injection is \
            skipped, the turn continues). Runs as a child process Pantheon \
            spawns with a scrubbed environment (PATH only), under your user \
            privileges; not sandboxed. First-party code shipped with Pantheon.",
    }]
}

/// Every plugin Pantheon ships, sorted by name. Deterministic order so
/// dashboard, app, and wizard render the same list.
pub fn bundled_plugins() -> Vec<BundledPlugin> {
    let mut out: Vec<BundledPlugin> = catalog_rows()
        .into_iter()
        .filter_map(|row| {
            let m: crate::manifest::PluginManifest =
                serde_yaml::from_str(row.manifest_yaml).ok()?;
            Some(BundledPlugin {
                name: m.name,
                kind: row.kind,
                version: m.version,
                description: m.description,
                enabled: m.enabled,
                privilege_notes: row.privilege_notes.to_string(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The catalog entry for `name`, or `None` when it is not bundled.
/// Only catalog names are toggleable - this is what keeps the agent
/// from proposing (or any surface from flipping) an arbitrary plugin.
pub fn find(name: &str) -> Option<BundledPlugin> {
    bundled_plugins().into_iter().find(|p| p.name == name)
}

/// Is the bundled plugin `name` enabled? Pure read over the config:
/// absent `[plugins.<name>]` (or an entry without `enabled`) = false.
/// Unknown names are false too - inert entries never enable anything.
///
/// Prefer [`is_enabled_with_default`] when the bundled manifest is at
/// hand: it honors the manifest's default-enabled flag.
pub fn is_enabled(config: &Config, name: &str) -> bool {
    is_enabled_with_default(config, name, false)
}

/// Is the bundled plugin `name` enabled, with a manifest default?
///
/// The `[plugins.<name>]` config entry wins when present - an explicit
/// operator choice always overrides the shipped default. When the entry
/// is absent, the bundled manifest's own `enabled` flag is the default
/// (true only for plugins that ship on, like noisegate; false for the
/// rest). Unknown names are false too - inert entries never enable
/// anything.
pub fn is_enabled_with_default(config: &Config, name: &str, manifest_default: bool) -> bool {
    if config.plugins.contains_key(name) {
        config.plugin_enabled(name)
    } else {
        manifest_default
    }
}

/// Load the config for enablement decisions without
/// [`Config::load_or_report`]'s process-exit behavior: a library must
/// never `exit(2)` the host because the config has a typo. A missing
/// config means every bundled plugin is disabled (fail closed); a config
/// that exists but does not parse is reported on stderr and also fails
/// closed - a typo must never silently flip a plugin on.
pub fn load_config(data_dir: &Path) -> Option<Config> {
    match Config::load(data_dir) {
        Ok(c) => Some(c),
        Err(e) if e.code == "CONFIG_OPEN" => None,
        Err(e) => {
            eprintln!("pantheon: bundled plugins disabled: {}", e.cause);
            eprintln!("pantheon: fix: {}", e.remediation);
            None
        }
    }
}

/// Write the single enablement state: set `[plugins.<name>].enabled` in
/// `<data_dir>/config.toml`, creating the file and tables as needed.
/// Only bundled-catalog names are accepted (`PLUGIN_UNKNOWN_PLUGIN`
/// otherwise) - this is the no-arbitrary-plugin-install boundary.
/// Enabling stamps the catalog `kind` and `version` so the entry is
/// self-describing; disabling keeps them and flips the flag.
///
/// The rest of the file is preserved: the document is edited as TOML,
/// not re-serialized from the [`Config`] struct, so unknown keys and
/// values survive the write. (Comments are not preserved - TOML
/// re-serialization drops them, the same tradeoff the dashboard's config
/// editor makes.)
/// Enable a catalog plugin, persisting to the config file. Rejects
/// anything outside the catalog.
pub fn enable(data_dir: &Path, name: &str) -> Result<(), PantheonError> {
    set_enabled(data_dir, name, true)
}

/// Disable a catalog plugin, persisting to the config file. Rejects
/// anything outside the catalog.
pub fn disable(data_dir: &Path, name: &str) -> Result<(), PantheonError> {
    set_enabled(data_dir, name, false)
}

pub fn set_enabled(data_dir: &Path, name: &str, enabled: bool) -> Result<(), PantheonError> {
    let plugin = find(name).ok_or_else(|| {
        merr(
            "PLUGIN_UNKNOWN_PLUGIN",
            format!("'{name}' is not a bundled plugin; only catalog plugins can be toggled"),
        )
    })?;
    let path = Config::path(data_dir);
    let mut doc: toml::Value = match std::fs::read_to_string(&path) {
        Ok(text) => text.parse().map_err(|e| {
            merr(
                "PLUGIN_CONFIG_PARSE",
                format!("parse {}: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(toml::map::Map::new())
        }
        Err(e) => {
            return Err(merr(
                "PLUGIN_CONFIG_READ",
                format!("read {}: {e}", path.display()),
            ))
        }
    };
    if !doc.is_table() {
        return Err(merr(
            "PLUGIN_CONFIG_PARSE",
            format!("{} is not a TOML table", path.display()),
        ));
    }
    let plugins = doc
        .as_table_mut()
        .ok_or_else(|| {
            merr(
                "PLUGIN_CONFIG_PARSE",
                format!("{} is not a TOML table", path.display()),
            )
        })?
        .entry("plugins")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let entry = plugins
        .as_table_mut()
        .ok_or_else(|| {
            merr(
                "PLUGIN_CONFIG_PARSE",
                "config has a non-table [plugins] value".to_string(),
            )
        })?
        .entry(name)
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let table = entry.as_table_mut().ok_or_else(|| {
        merr(
            "PLUGIN_CONFIG_PARSE",
            format!("[plugins.{name}] is not a table"),
        )
    })?;
    table.insert("enabled".to_string(), toml::Value::Boolean(enabled));
    if enabled {
        table.insert(
            "kind".to_string(),
            toml::Value::String(plugin.kind.as_str().to_string()),
        );
        if !plugin.version.is_empty() {
            table.insert(
                "version".to_string(),
                toml::Value::String(plugin.version.clone()),
            );
        }
    }
    let text = toml::to_string_pretty(&doc)
        .map_err(|e| merr("PLUGIN_CONFIG_WRITE", format!("serialize: {e}")))?;
    // Atomic: write tmp then rename, so a crash never leaves half a config.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            merr(
                "PLUGIN_CONFIG_WRITE",
                format!("create {}: {e}", parent.display()),
            )
        })?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            merr(
                "PLUGIN_CONFIG_WRITE",
                format!("write {}: {e}", path.display()),
            )
        })?;
    Ok(())
}

/// Materialize the catalog into the runtime extension dir:
/// `<ext_dir>/bundled/<name>/` with the vendored plugin files.
///
/// Seeding is inert, never enabling: every catalog entry stays disabled
/// until the operator flips `[plugins.<name>] enabled = true` (config,
/// dashboard, app, or agent proposal). An existing `<name>/` dir is
/// never touched, so operator edits or newer installs are not clobbered.
/// Returns the names that were actually seeded.
///
/// Call this before loading hook plugins (the runtime and TUI do) so a
/// catalog entry the operator enables always has files to load.
pub fn seed(ext_dir: &Path) -> Result<Vec<String>, PantheonError> {
    let mut seeded = Vec::new();
    for row in catalog_rows() {
        let m: crate::manifest::PluginManifest =
            serde_yaml::from_str(row.manifest_yaml).map_err(|e| {
                merr(
                    "PLUGIN_SEED_MANIFEST",
                    format!("parse vendored manifest: {e}"),
                )
            })?;
        let target = ext_dir.join("bundled").join(&m.name);
        if target.exists() {
            continue;
        }
        std::fs::create_dir_all(&target)
            .map_err(|e| merr("PLUGIN_SEED", format!("create {}: {e}", target.display())))?;
        std::fs::write(target.join("plugin.yaml"), row.manifest_yaml)
            .map_err(|e| merr("PLUGIN_SEED", format!("write manifest: {e}")))?;
        for (rel, contents) in row.files {
            let dest = target.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    merr("PLUGIN_SEED", format!("create {}: {e}", parent.display()))
                })?;
            }
            std::fs::write(&dest, contents)
                .map_err(|e| merr("PLUGIN_SEED", format!("write {}: {e}", dest.display())))?;
        }
        seeded.push(m.name);
    }
    Ok(seeded)
}

// Small deterministic invariant tests only. Filesystem behavior
// (set_enabled round-trip, config preservation) is covered in
// `pantheon-eval` (`eval/tests/bundled_plugins.rs`).

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_api::config::{Config, PluginEntry};

    fn config_with(name: &str, enabled: bool) -> Config {
        let mut c = Config::default();
        c.plugins.insert(
            name.to_string(),
            PluginEntry {
                enabled,
                kind: None,
                version: None,
            },
        );
        c
    }

    #[test]
    fn enablement_config_wins_manifest_default_applies() {
        let empty = Config::default();
        // No config entry: the manifest default decides.
        assert!(is_enabled_with_default(&empty, "noisegate", true));
        assert!(!is_enabled_with_default(&empty, "security-guidance", false));
        // Explicit config entry always wins over the manifest default
        // including an explicit `false`, which is how an operator turns
        // a default-on plugin back off.
        assert!(!is_enabled_with_default(
            &config_with("noisegate", false),
            "noisegate",
            true
        ));
        assert!(is_enabled_with_default(
            &config_with("security-guidance", true),
            "security-guidance",
            false
        ));
        // Legacy behavior preserved: is_enabled is config-only.
        assert!(!is_enabled(&empty, "noisegate"));
        assert!(is_enabled(&config_with("noisegate", true), "noisegate"));
        // Unknown names stay inert: callers only ever pass the default of
        // the real manifest being loaded, which is false for unknown names.
        assert!(!is_enabled_with_default(&empty, "not-a-plugin", false));
        assert!(!is_enabled_with_default(
            &config_with("other", true),
            "not-a-plugin",
            false
        ));
    }
}
