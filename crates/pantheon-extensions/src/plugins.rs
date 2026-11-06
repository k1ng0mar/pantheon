//! Extensions merged into one file. External paths keep working through the
//! crate-root shims in lib.rs (same pattern as the providers merge).

pub mod bundled {
    //! Bundled plugin catalog: the plugins Pantheon ships. Most are disabled
    //! by default; a plugin whose shipped manifest says `enabled: true`
    //! (noisegate, planning-with-files) ships on unless the operator's
    //! config entry says otherwise - that is the manifest default, and it
    //! applies whether or not a `config.toml` file exists yet.
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
    //!   capability gate, with full user privileges and no namespace sandbox;
    //!   hook plugins as per-fire child processes with a scrubbed environment,
    //!   sandboxed at the VeryHigh level whenever a capability policy is
    //!   configured and spawned directly only when no policy is configured.
    //!   Enabling a plugin is consent to run its code under that gate.
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
    //! Catalog entries ship as source in the repo and are compiled into the
    //! binary. [`seed`] materializes each entry into
    //! `<ext_dir>/bundled/<name>/` on load - inert files, never enabled.
    //! The `<dir>/bundled/` layout is what
    //! [`pantheon_api::approval::is_bundled`] recognizes, and
    //! [`crate::manager::ExtensionManager::load_dir`] scans it. An existing
    //! dir is never overwritten.
    //!
    //! This catalog (the vendored-extension path) is currently **empty** -
    //! distinct from the embedded bundled-plugin registry in pantheon-exec
    //! (cf-ops, noisegate, hookify, ...), which ships tool/hook plugins of
    //! its own through the same `[plugins]` enablement. This catalog's
    //! one-time entry, the time-gap hook plugin (a Python port from Hermes),
    //! was removed once the native temporal-awareness feature
    //! (`pantheon_api::temporal`, `[temporal]` in config.toml) covered the
    //! same ground in-process: ledger-based and restart-safe by
    //! construction, no child process per call, on by default. The catalog,
    //! seed, and single-enablement machinery remain for the next vendored
    //! extension; a Python-plugin fixture for `doctor` lives in
    //! `eval/fixtures/time-gap-plugin/`.
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
    //!   manifest's own `enabled` flag when present. The manifest is install
    //!   metadata that ships with the plugin; the config is the operator's
    //!   declared intent, and it is the one value every surface (TUI,
    //!   dashboard, app, agent) can read and write. A bundled plugin whose
    //!   manifest says enabled and whose config entry is absent loads (the
    //!   manifest default); one whose config entry says false does not.
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
    //! and only bundled names are accepted - this catalog plus the embedded
    //! registry in pantheon-exec - there is no agent path to install or
    //! enable arbitrary plugins.
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

    /// The catalog table. Add a row here (and add the plugin source to the
    /// repo) to ship a new bundled plugin. Never list a plugin that is not
    /// in the repo.
    fn catalog_rows() -> Vec<CatalogRow> {
        // Empty since time-gap went native (`pantheon_api::temporal`); see
        // the module docs under "Materialization".
        Vec::new()
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
    /// config counts as an EMPTY config, so the shipped manifest defaults
    /// apply on a fresh install (a plugin that ships on, like noisegate,
    /// is on; the rest are off) - exactly as if the file existed but had
    /// no `[plugins.<name>]` entry. A config that exists but does not
    /// parse is reported on stderr and fails closed (`None`, and callers
    /// map `None` to disabled) - a typo must never silently flip a plugin
    /// on. This is the config-defaults layer only: it does not relax the
    /// approval gate, which is a separate layer (third-party plugins still
    /// need a live approval no matter what any flag says).
    pub fn load_config(data_dir: &Path) -> Option<Config> {
        match Config::load(data_dir) {
            Ok(c) => Some(c),
            Err(e) if e.code == "CONFIG_OPEN" => Some(Config::default()),
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
            Ok(text) => toml::from_str::<toml::Value>(&text).map_err(|e| {
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
            let m: crate::manifest::PluginManifest = serde_yaml::from_str(row.manifest_yaml)
                .map_err(|e| {
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

        #[test]
        fn load_config_missing_counts_as_empty_not_fail_closed() {
            let dir = std::env::temp_dir()
                .join(format!("pantheon-dec-sec-missing-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // No config.toml at all: missing counts as empty, so manifest
            // defaults apply through the normal resolution path.
            let cfg = load_config(&dir).expect("missing config must load as empty");
            assert!(is_enabled_with_default(&cfg, "noisegate", true));
            assert!(!is_enabled_with_default(&cfg, "security-guidance", false));
            // A config that exists but does not parse still fails closed.
            std::fs::write(dir.join("config.toml"), "this is [not valid toml").unwrap();
            assert!(load_config(&dir).is_none());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod doctor {
    //! skill doctor: loud preflight for any plugin dir. Targets the classic
    //! silent-death complaints: missing env, missing bins, bad manifest,
    //! missing entry, dangerous hooks. Non-zero problems => loud report.
    use crate::hooks::Hook;
    use crate::manifest::PluginManifest;
    use serde::{Deserialize, Serialize};
    use std::path::Path;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DoctorFinding {
        pub level: String,
        pub code: String,
        pub detail: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DoctorReport {
        pub plugin: String,
        pub ok: bool,
        pub hooks: Vec<String>,
        pub unknown_hooks: Vec<String>,
        pub findings: Vec<DoctorFinding>,
    }

    fn f(level: &str, code: &str, detail: String) -> DoctorFinding {
        DoctorFinding {
            level: level.into(),
            code: code.into(),
            detail,
        }
    }

    /// Inspect a plugin dir without executing it.
    pub fn doctor(dir: &Path) -> DoctorReport {
        let mut findings = Vec::new();
        let label = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| dir.display().to_string());
        if !dir.exists() {
            return DoctorReport {
                plugin: label,
                ok: false,
                hooks: vec![],
                unknown_hooks: vec![],
                findings: vec![f(
                    "error",
                    "NO_DIR",
                    format!("{} does not exist", dir.display()),
                )],
            };
        }
        let man_path = dir.join("plugin.yaml");
        if !man_path.exists() {
            findings.push(f("error", "NO_MANIFEST", "plugin.yaml missing".into()));
            return DoctorReport {
                plugin: label,
                ok: false,
                hooks: vec![],
                unknown_hooks: vec![],
                findings,
            };
        }
        let man = match PluginManifest::load(&man_path) {
            Ok(m) => m,
            Err(e) => {
                findings.push(f("error", "BAD_MANIFEST", e.to_string()));
                return DoctorReport {
                    plugin: label,
                    ok: false,
                    hooks: vec![],
                    unknown_hooks: vec![],
                    findings,
                };
            }
        };
        if man.name.trim().is_empty() {
            findings.push(f("error", "NO_NAME", "manifest has empty name".into()));
        }
        let entry_py = dir.join("__init__.py").exists();
        let entry_ts = dir.join("index.ts").exists();
        if !entry_py && !entry_ts {
            findings.push(f(
                "error",
                "NO_ENTRY",
                "neither __init__.py nor index.ts found".into(),
            ));
        }
        if entry_ts && !entry_py {
            // Error, not a warning: with no __init__.py the Python runner
            // can never run this plugin, so a passing verdict (ok=true,
            // exit 0) lied about a plugin that cannot execute. A TS entry
            // alongside __init__.py stays a warning: the Python entry runs.
            findings.push(f(
            "error",
            "TS_ENTRY",
            "TypeScript-only entry: needs an OpenClaw-compat adapter (Soul path); the Python runner cannot run this plugin"
                .into(),
        ));
        }
        let (hooks, unknown) = man.hook_list();
        for u in &unknown {
            findings.push(f("warn", "UNKNOWN_HOOK", format!("unknown hook '{u}'")));
        }
        // A hook Pantheon knows but cannot fire is the dangerous case: the
        // plugin author believes it runs. Say so loudly rather than letting the
        // manifest look healthy.
        for h in &hooks {
            if !h.is_wired() {
                findings.push(f(
                "warn",
                "UNWIRED_HOOK",
                format!(
                    "'{}' is a known hook with no fire site yet; this plugin's handler will not run",
                    h.name()
                ),
            ));
            }
        }
        if hooks.is_empty() {
            findings.push(f(
                "warn",
                "NO_HOOKS",
                "manifest declares no known hooks".into(),
            ));
        }
        let names: Vec<String> = hooks.iter().map(|h| h.name().to_string()).collect();
        // Static scan of __init__.py for suspicious imports (loud, not blocking).
        if entry_py {
            if let Ok(text) = std::fs::read_to_string(dir.join("__init__.py")) {
                for pat in ["os.system", "subprocess", "socket", "urllib", "requests"] {
                    if text.contains(pat) {
                        findings.push(f(
                            "info",
                            "USES_NET_OR_EXEC",
                            format!(
                            "__init__.py mentions '{pat}': hook plugins run behind the capability \
                             gate and the VeryHigh sandbox whenever a policy is configured (direct \
                             spawn only when no policy is configured); tool plugins run with your \
                             full privileges and no namespace sandbox. Approve only what you trust"
                        ),
                        ));
                    }
                }
            }
        }
        // Approval state: a third-party plugin that is not approved will not
        // load, so say so here instead of letting the manifest look healthy.
        if let Some(ext_dir) = dir.parent() {
            if !pantheon_api::approval::is_bundled(ext_dir, dir) && !man.name.trim().is_empty() {
                let approved = pantheon_api::approval::dir_hash(dir)
                    .map(|h| {
                        pantheon_api::approval::is_approved(ext_dir, &man.name, &man.version, &h)
                    })
                    .unwrap_or(false);
                if !approved {
                    findings.push(f(
                        "warn",
                        "NOT_APPROVED",
                        format!(
                            "'{}' is third-party and not approved; it will not load until you run \
                         `pantheon extensions approve {}`",
                            man.name, man.name
                        ),
                    ));
                }
            }
        }
        let ok = !findings.iter().any(|x| x.level == "error");
        let _ = Hook::all();
        DoctorReport {
            plugin: man.name.clone(),
            ok,
            hooks: names,
            unknown_hooks: unknown,
            findings,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;

        fn plugin_dir(name: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir()
                .join(format!("pantheon-doctor-ts-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("plugin.yaml"), "name: ts-only\nversion: \"1\"\n").unwrap();
            dir
        }

        #[test]
        fn typescript_only_entry_is_an_error_not_a_pass() {
            let dir = plugin_dir("only");
            fs::write(dir.join("index.ts"), "export {}").unwrap();
            let report = doctor(&dir);
            assert!(
                !report.ok,
                "a plugin the Python runner can never run must not pass"
            );
            assert!(
                report
                    .findings
                    .iter()
                    .any(|x| x.level == "error" && x.code == "TS_ENTRY"),
                "findings: {:?}",
                report.findings
            );
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn typescript_alongside_python_entry_draws_no_ts_finding() {
            let dir = plugin_dir("both");
            fs::write(dir.join("index.ts"), "export {}").unwrap();
            fs::write(dir.join("__init__.py"), "").unwrap();
            let report = doctor(&dir);
            assert!(
                !report.findings.iter().any(|x| x.code == "TS_ENTRY"),
                "findings: {:?}",
                report.findings
            );
            assert!(report.ok);
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

pub mod event_bridge {
    //! Canonical-event -> hook bridge.
    //!
    //! `Supervisor::emit` already fans out to registered observers *after* the
    //! durable ledger write, with exactly the fail-open semantics a hook needs
    //! ("the ledger is the contract, observers are best-effort views"). This
    //! module is a pure function from that stream to hook fires, so the runtime
    //! needs one registration point instead of sprinkling hook calls through the
    //! agent loop and the provider plane.
    //!
    //! Keeping it here (not in the runtime) also keeps the dependency direction
    //! clean: `pantheon-extensions` already depends on `pantheon-api`, and the
    //! runtime depends on extensions - never the reverse.

    use crate::hooks::Hook;
    use pantheon_api::events::Event;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// One hook fire derived from an event.
    pub struct HookFire {
        pub hook: Hook,
        /// Flat string payload. The subprocess runners exchange JSON, and a flat
        /// map keeps both runners' input shape identical.
        pub extra: HashMap<String, String>,
    }

    /// Whether per-delta streaming hooks are enabled. Off by default: the runner
    /// spawns a process per fire and `ModelDelta` arrives per token, so leaving
    /// this on would be a self-inflicted stall. Other runtimes reach the same
    /// conclusion ("off the token path").
    fn stream_deltas_enabled() -> bool {
        matches!(
            std::env::var("PANTHEON_HOOK_STREAM_DELTA").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
    }

    /// Map one canonical event to the hook it should fire, if any.
    ///
    /// Returns `None` for events with no hook (bookkeeping, approval, decision,
    /// context-trim, ...). Observers only: gate and transform hooks are *not*
    /// driven from here because they must run inline and their return value
    /// changes control flow - `pre_tool_call` and `transform_tool_result` are
    /// fired from the tool-execution path in `session.rs` instead.
    pub fn dispatch(ev: &Event) -> Option<HookFire> {
        use Event::*;
        let (hook, extra) = match ev {
            RunStarted { run_id } => (Hook::OnSessionStart, kv(&[("run_id", run_id)])),
            RunCompleted { run_id } | RunFailed { run_id, .. } | RunCanceled { run_id, .. } => {
                (Hook::OnSessionEnd, kv(&[("run_id", run_id)]))
            }
            ModelRequested { run_id, model } => (
                Hook::PreApiRequest,
                kv(&[("run_id", run_id), ("model", model)]),
            ),
            ModelCompleted { run_id } => {
                // A completed model turn is also the close of its stream. Others
                // keep these distinct because a provider can emit deltas across
                // several `ModelRequested`s in one turn; here the request is the
                // unit, so one event closes exactly one stream.
                (Hook::PostApiRequest, kv(&[("run_id", run_id)]))
            }
            ModelDelta { run_id, delta } => {
                if !stream_deltas_enabled() {
                    return None;
                }
                (
                    Hook::OnStreamDelta,
                    kv(&[("run_id", run_id), ("delta", delta)]),
                )
            }
            AgentSpawned { run_id, agent, .. } => (
                Hook::SubagentStart,
                kv(&[("run_id", run_id), ("agent", agent)]),
            ),
            AgentCompleted { run_id, agent, .. } => (
                Hook::SubagentStop,
                kv(&[("run_id", run_id), ("agent", agent)]),
            ),
            // Context compaction ran (rows trimmed or summarized). OMP emits
            // this as a start/end pair; Pantheon fires once, after the fact
            // see `Hook::OnCompaction` for the documented collapse.
            ContextTrimmed { run_id, .. } => (Hook::OnCompaction, kv(&[("run_id", run_id)])),
            ContextCompressed { run_id, .. } => (Hook::OnCompaction, kv(&[("run_id", run_id)])),
            // `ToolStarted` is deliberately NOT mapped: its natural hook
            // (`pre_tool_call`) is a gate whose return value must change control
            // flow, and an observer's return value is ignored. It is fired
            // inline in `session.rs` before the tool executes. Likewise
            // `transform_tool_result` needs the output string in hand, which
            // `ToolOutput` does not carry, so it fires inline too.
            ToolCompleted {
                run_id,
                call_id,
                tool,
                ..
            } => (
                Hook::PostToolCall,
                kv(&[("run_id", run_id), ("call_id", call_id), ("tool", tool)]),
            ),
            _ => return None,
        };
        Some(HookFire { hook, extra })
    }

    /// The stream-open/close pair, which cannot be inferred from a single event
    /// because `ModelRequested` also backs `pre_api_request`. The runtime calls
    /// this alongside [`dispatch`] so a model turn notifies both facts.
    pub fn dispatch_stream_edges(ev: &Event) -> Option<HookFire> {
        match ev {
            Event::ModelRequested { run_id, model } => Some(HookFire {
                hook: Hook::OnStreamStart,
                extra: kv(&[("run_id", run_id), ("model", model)]),
            }),
            Event::ModelCompleted { run_id } => Some(HookFire {
                hook: Hook::OnStreamEnd,
                extra: kv(&[("run_id", run_id)]),
            }),
            _ => None,
        }
    }

    fn kv(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Fire hooks off the caller's thread.
    ///
    /// The supervisor's observer contract is explicit that observers run
    /// *synchronously on the emit path* and must not block. A hook fire spawns a
    /// subprocess (python3/node, up to the 10s runner timeout), so doing that
    /// inline would stall the agent loop on every `ToolCompleted`.
    ///
    /// [`HookDispatcher`] therefore queues each event's hooks onto a
    /// short-lived worker thread and returns immediately. Ordering is preserved
    /// within one event's hook pair, but not across events: observers are
    /// notifications, not a ledger. If nobody binds a hook, the worker finds no
    /// plugin and exits.
    ///
    /// Terminal-event caveat: `on_session_end` is queued, not synchronous, so a
    /// process that exits immediately after a run may not flush it. Consumers
    /// that must not lose the event (GalaxyMem consolidation) should treat it as
    /// best-effort and reconcile on next start, or await [`HookDispatcher::drain`]
    /// at teardown.
    pub struct HookDispatcher {
        mgr: std::sync::Arc<crate::manager::ExtensionManager>,
        inflight: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl HookDispatcher {
        pub fn new(mgr: std::sync::Arc<crate::manager::ExtensionManager>) -> Self {
            Self {
                mgr,
                inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
        /// Queue the hooks implied by `ev`. Returns immediately.
        pub fn fire(&self, ev: &Event) {
            let fires: Vec<HookFire> = [dispatch(ev), dispatch_stream_edges(ev)]
                .into_iter()
                .flatten()
                .collect();
            if fires.is_empty() {
                return;
            }
            self.inflight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mgr = std::sync::Arc::clone(&self.mgr);
            let inflight = Arc::clone(&self.inflight);
            std::thread::spawn(move || {
                for f in fires {
                    let session = f.extra.get("run_id").cloned().unwrap_or_default();
                    mgr.notify(f.hook, &session, "runtime", f.extra);
                }
                inflight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        /// Block until queued fires have run. Bounded so a wedged plugin (the
        /// runner's own timeout) cannot hang the caller forever.
        pub fn drain(&self, budget: std::time::Duration) -> bool {
            let deadline = std::time::Instant::now() + budget;
            while self.inflight.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                if std::time::Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            true
        }
    }
}

pub mod hooks {
    //! Hook points.
    //!
    //! SCOPE (this comment is load-bearing - keep it true).
    //!
    //! Pantheon implements a *subset* of the Hermes hook surface, not a superset.
    //! Hermes' `VALID_HOOKS` (`hermes_cli/plugins.py`) names 41 events; of those,
    //! Pantheon wires all 15 declared below. Every wired hook has a
    //! real fire site: either in `session.rs` (the tool-execution gate/transform
    //! and the context-injection point) or via `event_bridge`, which maps
    //! canonical `pantheon_api::events::Event`s onto hooks on the supervisor's
    //! post-durable-write observer fan-out.
    //!
    //! The rule this file enforces: **no hook may be declared without a fire
    //! site.** `Hook::is_wired()` is the single source of truth, and
    //! `compat::map_hook` and `doctor` both consult it, so an unwired hook can
    //! never be reported as `Mapped` (which would promise a plugin that its
    //! handler will run) and a manifest that declares one is flagged loudly.
    //! This mirrors Hermes' own constraint: "no inert VALID_HOOKS surface is
    //! registered ahead of implementation."
    //!
    //! Hook classes matter because they have different contracts:
    //! - [`HookClass::Context`] may inject text; the return value is used.
    //! - [`HookClass::Gate`] may deny; **fails closed** on plugin error/timeout.
    //! - [`HookClass::Transform`] may replace a payload; fails open.
    //! - [`HookClass::Observer`] is notified; its return value is ignored.
    use serde::{Deserialize, Serialize};

    /// What a hook is allowed to do, which fixes its failure mode.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum HookClass {
        /// May inject context that the host adds to the prompt. Return value used.
        Context,
        /// May deny the action. Fails CLOSED: a plugin that errors or times out
        /// denies, because a security gate that fails open is not a gate.
        Gate,
        /// May return a replacement payload. Fails OPEN: first non-`None` wins,
        /// and a failing plugin leaves the payload unchanged.
        Transform,
        /// Notified only. Return values are ignored by the host.
        ///
        /// Deliberate, not a bug to work around: a plugin that needs its
        /// text in front of the model must use a Context hook
        /// (`pre_llm_call`, or `on_session_start`, which the session fires
        /// as context on a fresh run's first turn). Observer returns from
        /// `post_tool_call` / `on_session_end` are discarded by contract,
        /// and reclassifying those hooks as Context would break the
        /// observer fan-out that drives them.
        Observer,
    }

    /// Every extension hook point.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Hook {
        /// Before model inference. May inject context (anti-ai-writing, time-gap).
        PreLlmCall,
        /// Before an outbound provider API request.
        PreApiRequest,
        /// After a provider API response.
        PostApiRequest,
        /// Before a gateway dispatches an inbound message.
        PreGatewayDispatch,
        /// Once when a run starts. Observers: memory/session bootstrap.
        OnSessionStart,
        /// Once when a run reaches a terminal state (completed, failed, or
        /// canceled). GalaxyMem uses this for memory consolidation; disk-cleanup
        /// uses it. Terminal-once semantics matter: a crashed-then-recovered run
        /// must not double-consolidate, so the manager's `once_per_session`
        /// dedup is the enforcement point.
        OnSessionEnd,
        /// Before a gated tool call executes. May DENY the call.
        PreToolCall,
        /// After a tool call completes. Observer.
        PostToolCall,
        /// May REPLACE a tool's output before the model sees it. Redaction and
        /// secret-scrubbing live here.
        TransformToolResult,
        /// A sub-agent was spawned. Observer - this is what makes §3 swarm
        /// spawns visible to plugins.
        SubagentStart,
        /// A sub-agent finished. Observer.
        SubagentStop,
        /// A model response stream opened. Observer.
        OnStreamStart,
        /// A streamed text delta arrived. Observer, and **off by default**: the
        /// runner spawns a process per fire, and this fires per token. Enable
        /// with `PANTHEON_HOOK_STREAM_DELTA=1` only if a plugin truly needs it.
        /// Other runtimes make the same call ("off the token path").
        OnStreamDelta,
        /// A model response stream closed. Observer.
        OnStreamEnd,
        /// Context compaction ran on this run (oldest rows trimmed or summarized
        /// into a memory-tier note). Observer.
        ///
        /// OMP emits this as a start/end pair (`auto_compaction_start` /
        /// `auto_compaction_end`); Pantheon fires once, after the fact, off the
        /// durable `ContextTrimmed` / `ContextCompressed` events. A plugin that
        /// needs "compaction is about to happen" gets "compaction happened"
        /// instead - the collapse is documented here and in `compat::map_hook`
        /// so nobody mistakes it for a pre-compaction gate.
        OnCompaction,
    }

    impl Hook {
        pub fn name(&self) -> &'static str {
            match self {
                Hook::PreLlmCall => "pre_llm_call",
                Hook::PreApiRequest => "pre_api_request",
                Hook::PostApiRequest => "post_api_request",
                Hook::PreGatewayDispatch => "pre_gateway_dispatch",
                Hook::OnSessionStart => "on_session_start",
                Hook::OnSessionEnd => "on_session_end",
                Hook::PreToolCall => "pre_tool_call",
                Hook::PostToolCall => "post_tool_call",
                Hook::TransformToolResult => "transform_tool_result",
                Hook::SubagentStart => "subagent_start",
                Hook::SubagentStop => "subagent_stop",
                Hook::OnStreamStart => "on_stream_start",
                Hook::OnStreamDelta => "on_stream_delta",
                Hook::OnStreamEnd => "on_stream_end",
                Hook::OnCompaction => "on_compaction",
            }
        }
        pub fn parse(name: &str) -> Option<Hook> {
            match name.trim() {
                "pre_llm_call" => Some(Hook::PreLlmCall),
                "pre_api_request" => Some(Hook::PreApiRequest),
                "post_api_request" => Some(Hook::PostApiRequest),
                "pre_gateway_dispatch" => Some(Hook::PreGatewayDispatch),
                "on_session_start" => Some(Hook::OnSessionStart),
                "on_session_end" => Some(Hook::OnSessionEnd),
                "pre_tool_call" => Some(Hook::PreToolCall),
                "post_tool_call" => Some(Hook::PostToolCall),
                "transform_tool_result" => Some(Hook::TransformToolResult),
                "subagent_start" => Some(Hook::SubagentStart),
                "subagent_stop" => Some(Hook::SubagentStop),
                "on_stream_start" => Some(Hook::OnStreamStart),
                "on_stream_delta" => Some(Hook::OnStreamDelta),
                "on_stream_end" => Some(Hook::OnStreamEnd),
                "on_compaction" => Some(Hook::OnCompaction),
                _ => None,
            }
        }
        /// What this hook is permitted to do, which fixes its failure mode.
        pub fn class(&self) -> HookClass {
            match self {
                Hook::PreLlmCall => HookClass::Context,
                Hook::PreToolCall => HookClass::Gate,
                Hook::TransformToolResult => HookClass::Transform,
                _ => HookClass::Observer,
            }
        }
        /// Whether this hook has a real fire site in the runtime today.
        ///
        /// All declared hooks are wired. `pre_gateway_dispatch` fires in
        /// the TUI gateway's surface sink before any inbound event reaches
        /// the runtime; the gateway crate itself stays free of the
        /// extension manager by exposing a dependency-free
        /// `PreDispatchHook` observer that the daemon calls before
        /// dispatch (`route_event_with_hook` / `ChannelDaemon::with_pre_dispatch`).
        pub fn is_wired(&self) -> bool {
            true
        }
        /// Fires often enough that a per-fire process spawn is a hazard.
        /// Gated behind an explicit opt-in by the event bridge.
        pub fn is_high_frequency(&self) -> bool {
            matches!(self, Hook::OnStreamDelta)
        }
        /// All known hooks (for doctor + docs).
        pub fn all() -> &'static [Hook] {
            &[
                Hook::PreLlmCall,
                Hook::PreApiRequest,
                Hook::PostApiRequest,
                Hook::PreGatewayDispatch,
                Hook::OnSessionStart,
                Hook::OnSessionEnd,
                Hook::OnCompaction,
                Hook::PreToolCall,
                Hook::PostToolCall,
                Hook::TransformToolResult,
                Hook::SubagentStart,
                Hook::SubagentStop,
                Hook::OnStreamStart,
                Hook::OnStreamDelta,
                Hook::OnStreamEnd,
            ]
        }
    }
}

pub mod manager {
    //! Extension manager: owns loaded plugins, fires hooks in alphabetical
    //! (name-sorted) order - `load_dir`/`load_one` sort by manifest name, so
    //! alphabetical order, not manifest-file order, decides gate-deny and
    //! transform winners. Concatenates injected contexts. Gate hooks fail
    //! closed (see `fire_gate`); other hook failures are per-plugin
    //! fail-open, not a blanket fail-open policy.
    //!
    //! Load-gate outages quarantine, never fail open: a plugin whose own
    //! gate check fails (e.g. its content hash cannot be computed) is
    //! quarantined alone - recorded in [`ExtensionManager::quarantined`]
    //! with a loud warning naming it - while the rest keep loading. A
    //! systemic gate failure (the approval store itself corrupt or
    //! unreadable) quarantines every plugin that store gates, which is
    //! fail-closed in effect. Canonical bundled plugins skip the store, so
    //! a store outage does not quarantine them; their gate is the config
    //! flag, and a config that does not parse still fails closed.
    //!
    //! Session-scoped dedup: plugins marked `once_per_session: true` in
    //! plugin.yaml (or Hermes `__init__.py` plugins that keep in-process
    //! `_seen_sessions`, which our subprocess runner can't preserve) fire at
    //! most once per (plugin, hook, session). The manager owns this, not the
    //! plugin process - required because each fire spawns fresh.
    use crate::hooks::{Hook, HookClass};
    use crate::python_runner::{
        fire_hook_full, HookDirective, HookInput, PythonPlugin, RunnerConfig,
    };
    use pantheon_api::approval::{self, PendingPlugin};
    use pantheon_api::error::{Layer, PantheonError};
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    fn merr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Extension,
            false,
            cause,
            "check extension dir and manifests",
            "",
        )
    }

    pub struct ExtensionManager {
        plugins: Vec<PythonPlugin>,
        cfg: RunnerConfig,
        seen_once: Mutex<HashSet<(String, String, String)>>,
        /// Consecutive hook failures per plugin. A plugin that times out
        /// repeatedly degrades every turn; after SKIP_AFTER_FAILURES
        /// consecutive failures it is skipped for the rest of the session
        /// (in-memory only; a new session retries it).
        timeout_streaks: Mutex<HashMap<String, u32>>,
        /// Third-party plugins discovered but not approved yet. They are never
        /// fired; see [`approval`].
        pending: Vec<PendingPlugin>,
        /// Manifest/load failures per plugin dir, surfaced (not just
        /// eprintln'd) so session start can warn and ledger-log them: a
        /// typo'd `plugin.yaml` used to look identical to "no plugins".
        load_errors: Vec<String>,
        /// Duplicate-manifest-name conflicts: two dirs claiming one name.
        /// Approvals are keyed by name alone, so the second dir is never
        /// silently dropped and the first is never silently shadowed.
        conflicts: Vec<String>,
        /// Plugins quarantined during load because their gate check failed
        /// (per-plugin) or the approval store itself failed (systemic).
        /// Quarantined plugins are neither loaded nor pending: pending
        /// would imply an operator approval could load them, but a gate
        /// failure is not fixed by approving. Surfaced so session start
        /// can warn and ledger-log them.
        quarantined: Vec<String>,
        /// Explicit data dir (the dir holding `config.toml`). When set,
        /// enablement reads config from here instead of inferring it as
        /// `ext_dir.parent()` - that inference is wrong under a custom
        /// `PANTHEON_EXT_DIR` (e.g. `/tmp/ext` -> data dir `/tmp`), which
        /// silently disabled every bundled hook.
        data_dir: Option<PathBuf>,
        /// Project root handed to hook plugins (in `HookInput.extra` as
        /// `project_root`, and as `PANTHEON_DATA_DIR` in the child env via
        /// [`RunnerConfig::data_dir`]). Hook children run with cwd = the
        /// plugin dir, so without this a plugin walking up from its cwd can
        /// never find project files.
        project_root: Option<PathBuf>,
    }

    /// The verdict of a gate hook.
    ///
    /// `Deny` is the fail-closed direction: a plugin that crashes, times out, or
    /// has been skipped after repeated failures denies rather than allows. A gate
    /// that fails open is not a gate.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum GateDecision {
        Allow,
        Deny { reason: String, plugin: String },
    }

    /// The outcome of one plugin's load-time gate check. `GateFailed` is
    /// deliberately distinct from `NotApproved`: a failed check quarantines
    /// the plugin (alone) or, when the store itself failed, everything it
    /// gates - it must never degrade into "unapproved" (pending) or, worse,
    /// into loading.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum GateVerdict {
        Approved,
        NotApproved,
        GateFailed(String),
    }

    /// Consecutive hook failures after which a plugin is skipped for the
    /// session. A single timeout stays fail-open (one bad call should not
    /// disable the plugin), but a wedge degrades every turn until stopped.
    const SKIP_AFTER_FAILURES: u32 = 3;

    /// Immediate subdirs of `dir` containing a plugin.yaml, sorted. A
    /// missing or unreadable dir yields nothing - used for the optional
    /// `<ext>/bundled/` layer, where absence is the normal first-run state.
    fn plugin_dirs(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() && p.join("plugin.yaml").exists() {
                out.push(p);
            }
        }
        out.sort();
        out
    }

    impl ExtensionManager {
        pub fn new(cfg: RunnerConfig) -> Self {
            Self {
                plugins: Vec::new(),
                cfg,
                seen_once: Mutex::new(HashSet::new()),
                timeout_streaks: Mutex::new(HashMap::new()),
                pending: Vec::new(),
                load_errors: Vec::new(),
                conflicts: Vec::new(),
                quarantined: Vec::new(),
                data_dir: None,
                project_root: None,
            }
        }

        /// Builder: explicit data dir for enablement config (see field docs).
        /// Also forwarded to [`RunnerConfig::data_dir`] so hook children see
        /// `PANTHEON_DATA_DIR`.
        pub fn with_data_dir(mut self, dir: PathBuf) -> Self {
            self.cfg.data_dir = Some(dir.clone());
            self.data_dir = Some(dir);
            self
        }

        /// Builder: project root injected into every hook fire's `extra`.
        pub fn with_project_root(mut self, dir: PathBuf) -> Self {
            self.project_root = Some(dir);
            self
        }

        /// Manifest/load failures recorded during [`Self::load_dir`].
        pub fn load_errors(&self) -> &[String] {
            &self.load_errors
        }

        /// Duplicate-name conflicts recorded during [`Self::load_dir`].
        pub fn conflicts(&self) -> &[String] {
            &self.conflicts
        }

        /// Plugins quarantined during load because a gate check failed.
        /// Each entry names the plugin and the reason; quarantined plugins
        /// are never loaded and never fired.
        pub fn quarantined(&self) -> &[String] {
            &self.quarantined
        }

        /// Record a quarantine with a loud warning naming the plugin. The
        /// rest keep loading; only this plugin is held out.
        fn quarantine(&mut self, name: &str, reason: &str) {
            let msg = format!("extension '{name}' quarantined: {reason}");
            eprintln!("WARNING: {msg}");
            self.quarantined.push(msg);
        }

        /// `extra` plus the project/data-dir context hook plugins need.
        /// Caller keys win: an explicit per-call value is never overwritten.
        fn enriched_extra(
            &self,
            extra: std::collections::HashMap<String, String>,
        ) -> std::collections::HashMap<String, String> {
            let mut out = extra;
            if let Some(root) = &self.project_root {
                out.entry("project_root".to_string())
                    .or_insert_with(|| root.to_string_lossy().into_owned());
            }
            if let Some(dd) = &self.data_dir {
                out.entry("pantheon_data_dir".to_string())
                    .or_insert_with(|| dd.to_string_lossy().into_owned());
            }
            out
        }

        /// Path-bundled AND byte-identical to the shipped registry. A
        /// hand-dropped `bundled/evil/` (or a tampered shipped dir) fails
        /// this and falls through to the third-party approval check.
        fn is_canonical_bundled(ext_dir: &Path, plugin_dir: &Path) -> bool {
            approval::is_bundled(ext_dir, plugin_dir)
                && pantheon_exec::plugins::verify_bundled_dir(plugin_dir)
        }

        /// Pre-seed seen keys (e.g. from `hook_seen.json` for fresh CLI processes).
        pub fn preseed_seen(&self, keys: HashSet<(String, String, String)>) {
            let mut seen = self.seen_once.lock().unwrap_or_else(|e| e.into_inner());
            seen.extend(keys);
        }

        /// Snapshot seen keys (e.g. to persist for the next CLI invocation).
        pub fn seen_snapshot(&self) -> HashSet<(String, String, String)> {
            self.seen_once
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
        /// Load every immediate subdir of `dir` containing plugin.yaml, plus
        /// every plugin under `<dir>/bundled/` (the first-party layout
        /// [`pantheon_api::approval::is_bundled`] recognizes).
        ///
        /// Third-party plugins are loaded only with a live operator approval
        /// (see [`approval`]); unapproved ones are collected in [`Self::pending`]
        /// and never fired. First-party plugins under `<dir>/bundled/` load
        /// without approval only when canonical (see
        /// [`Self::is_canonical_bundled`]); a non-canonical bundled dir is
        /// treated as third-party.
        ///
        /// Asymmetry, documented not hidden: hook discovery scans only this
        /// extensions tree, unlike tool discovery which also scans
        /// `<project>/.pantheon/plugins`. A project
        /// `.pantheon/extensions/` plugin is therefore never loaded; that is
        /// deliberate (project hook code must not auto-run), not an oversight
        /// to silently "fix" by scanning it.
        pub fn load_dir(&mut self, dir: &Path) -> Result<Vec<String>, PantheonError> {
            // The top-level dir keeps its old contract: unreadable means an
            // error the caller can report. The bundled subdir is optional
            // missing just means no bundled plugins on disk yet.
            let rd = std::fs::read_dir(dir).map_err(|e| merr("EXT_DIR", e.to_string()))?;
            let mut tops = Vec::new();
            for entry in rd {
                let entry = entry.map_err(|e| merr("EXT_DIR", e.to_string()))?;
                let p = entry.path();
                if p.is_dir() && p.join("plugin.yaml").exists() {
                    tops.push(p);
                }
            }
            tops.sort();
            // Systemic gate state, computed once: when the approval store
            // itself is corrupt or unreadable, every plugin it gates is
            // quarantined (fail-closed in effect) instead of each being
            // reported as merely unapproved.
            let gate_failure = approval::store_gate_failure(dir);
            for p in tops {
                self.load_plugin_dir(dir, &p, gate_failure.as_deref());
            }
            for p in plugin_dirs(&dir.join("bundled")) {
                self.load_plugin_dir(dir, &p, gate_failure.as_deref());
            }
            self.plugins
                .sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
            self.pending.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(self.names())
        }

        /// Try one plugin directory: load it when approved (bundled plugins
        /// consult the config enablement flag, third-party ones the approval
        /// store), otherwise record why it was skipped. `store_gate_failure`
        /// is the systemic gate state from [`load_dir`]: when `Some`, every
        /// third-party plugin here is quarantined alone-or-all - alone in
        /// the sense that bundled plugins (which skip the store) and the
        /// load itself are unaffected, all in the sense that nothing the
        /// store gates loads.
        fn load_plugin_dir(&mut self, ext_dir: &Path, p: &Path, store_gate_failure: Option<&str>) {
            match PythonPlugin::load(p) {
                Ok(pl) => {
                    let name = pl.manifest.name.clone();
                    let canonical_bundled = Self::is_canonical_bundled(ext_dir, p);
                    if let Some(reason) = store_gate_failure.filter(|_| !canonical_bundled) {
                        // Systemic gate failure: quarantine everything the
                        // store gates. Never fail open, never merely pend:
                        // an approval cannot be recorded into a broken store.
                        self.quarantine(&name, &format!("systemic gate failure: {reason}"));
                        return;
                    }
                    let verdict = self.gate_verdict(
                        ext_dir,
                        p,
                        &pl.manifest.name,
                        &pl.manifest.version,
                        pl.manifest.enabled,
                        pl.manifest.enabled_set,
                    );
                    if let GateVerdict::GateFailed(reason) = verdict {
                        // Per-plugin gate failure: quarantine this plugin
                        // alone; the rest keep loading.
                        self.quarantine(&name, &format!("gate check failed: {reason}"));
                        return;
                    }
                    if verdict == GateVerdict::Approved {
                        if self.plugins.iter().any(|q| q.manifest.name == name) {
                            // Duplicate manifest name: approvals are keyed by
                            // name alone, so loading both would let one
                            // approval cover two different dirs, and pending
                            // would list an unresolvable second entry. Fail
                            // loudly, keep the first, drop the second.
                            let msg = format!(
                            "extension name conflict: '{name}' at {} is ignored; an extension named '{name}' is already loaded (rename one of them)",
                            p.display()
                        );
                            eprintln!("{msg}");
                            self.conflicts.push(msg);
                        } else {
                            // Bind the approval scope: every hook fire re-hashes
                            // the plugin dir against this scope immediately
                            // before spawn and fails closed on divergence.
                            let mut pl = pl;
                            pl.scope_dir = Some(ext_dir.to_path_buf());
                            self.plugins.push(pl);
                        }
                    } else if canonical_bundled {
                        // Bundled but disabled in config: not pending
                        // approval, just off. Never silently enabled.
                        eprintln!(
                        "extension '{name}' is bundled but disabled; enable it with [plugins.{name}] enabled = true"
                    );
                    } else {
                        if self.pending.iter().any(|q| q.name == name) {
                            // Same conflict on the pending side: a second dir
                            // with an already-pending name would leave an
                            // unresolvable pending entry after approving the
                            // first. Surface it, keep the first.
                            let msg = format!(
                            "extension name conflict: '{name}' at {} is ignored; a pending extension named '{name}' already exists (rename one of them)",
                            p.display()
                        );
                            eprintln!("{msg}");
                            self.conflicts.push(msg);
                        } else {
                            eprintln!(
                            "extension '{name}' is not approved and will not load; run `pantheon extensions approve {name}`"
                        );
                            self.pending.push(PendingPlugin {
                                name,
                                version: pl.manifest.version.clone(),
                                dir: p.to_path_buf(),
                            });
                        }
                    }
                }
                Err(e) => {
                    // Surfaced, not just printed: callers (session start)
                    // report `load_errors` so a broken plugin.yaml does not
                    // masquerade as "no plugins".
                    let msg = format!("skip {}: {e}", p.display());
                    eprintln!("{msg}");
                    self.load_errors.push(msg);
                }
            }
        }
        pub fn load_one(&mut self, dir: &Path) -> Result<String, PantheonError> {
            let pl = PythonPlugin::load(dir)?;
            let name = pl.manifest.name.clone();
            // `load_one` targets a specific plugin dir; the extensions root is
            // its parent. Approval still applies - an explicit load is not
            // consent.
            let ext_dir = dir.parent().unwrap_or(dir);
            let canonical_bundled = Self::is_canonical_bundled(ext_dir, dir);
            // Systemic gate failure on a single-plugin load: fail closed
            // with the systemic reason, never merely "not approved".
            if let Some(reason) =
                approval::store_gate_failure(ext_dir).filter(|_| !canonical_bundled)
            {
                self.quarantine(&name, &format!("systemic gate failure: {reason}"));
                return Err(merr(
                    "EXT_GATE_FAILED",
                    format!("extension '{name}' quarantined: systemic gate failure: {reason}"),
                ));
            }
            match self.gate_verdict(
                ext_dir,
                dir,
                &pl.manifest.name,
                &pl.manifest.version,
                pl.manifest.enabled,
                pl.manifest.enabled_set,
            ) {
                GateVerdict::Approved => {}
                GateVerdict::GateFailed(reason) => {
                    self.quarantine(&name, &format!("gate check failed: {reason}"));
                    return Err(merr(
                        "EXT_GATE_FAILED",
                        format!("extension '{name}' quarantined: gate check failed: {reason}"),
                    ));
                }
                GateVerdict::NotApproved => {
                    if canonical_bundled {
                        return Err(merr(
                        "EXT_BUNDLED_DISABLED",
                        format!(
                            "extension '{name}' is bundled but disabled; enable it with [plugins.{name}] enabled = true"
                        ),
                    ));
                    }
                    self.pending.push(PendingPlugin {
                        name: name.clone(),
                        version: pl.manifest.version.clone(),
                        dir: dir.to_path_buf(),
                    });
                    return Err(merr(
                    "EXT_NOT_APPROVED",
                    format!(
                        "extension '{name}' is not approved; run `pantheon extensions approve {name}`"
                    ),
                ));
                }
            }
            // Same-name handling, both ways: a reload of the SAME dir
            // replaces the old entry in place (loading twice used to push
            // a second copy, firing its hooks twice), but a DIFFERENT dir
            // with an already-loaded name is a conflict, not a silent
            // takeover - the second dir is ignored.
            if let Some(existing) = self.plugins.iter().find(|q| q.manifest.name == name) {
                if existing.dir.as_path() != dir {
                    return Err(merr(
                    "EXT_NAME_CONFLICT",
                    format!(
                        "extension '{name}' at {} is ignored; an extension named '{name}' is already loaded (rename one of them)",
                        dir.display()
                    ),
                ));
                }
                self.plugins.retain(|p| p.manifest.name != name);
                self.pending.retain(|p| p.name != name);
            }
            self.plugins.push({
                // Bind the approval scope: every hook fire re-hashes the
                // plugin dir against this scope immediately before spawn
                // and fails closed on divergence.
                let mut pl = pl;
                pl.scope_dir = Some(ext_dir.to_path_buf());
                pl
            });
            self.plugins
                .sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
            Ok(name)
        }
        /// Third-party plugins discovered but never approved. They are not
        /// loaded and never fire.
        pub fn pending(&self) -> &[PendingPlugin] {
            &self.pending
        }
        /// Approval check for one plugin dir. Canonical bundled (first-party)
        /// plugins skip the approval store; a bundled-path dir that is NOT
        /// canonical falls through to the third-party check below, so a
        /// hand-dropped `bundled/evil/` needs a real approval (and its
        /// `[plugins.<name>] enabled = true` alone never loads it).
        /// Third-party plugins need a live approval bound to the current
        /// content hash AND must not be explicitly disabled: a manifest
        /// that says `enabled: false` does not fire even when approved.
        /// (A manifest that omits `enabled` keeps the old approved-means-
        /// loaded behaviour; only an explicit `false` opts out.)
        fn gate_verdict(
            &self,
            ext_dir: &Path,
            plugin_dir: &Path,
            name: &str,
            version: &str,
            manifest_enabled: bool,
            manifest_enabled_set: bool,
        ) -> GateVerdict {
            if Self::is_canonical_bundled(ext_dir, plugin_dir) {
                // Bundled (first-party) plugins skip the third-party approval
                // store. Enablement: the config file (`[plugins.<name>]`) is
                // the single enablement state, shared with the dashboard, the
                // mobile app, and the agent - and it wins when present. When
                // absent, the bundled manifest's own `enabled` flag is the
                // default (true only for plugins that ship on, like
                // noisegate). An unparsable config fails closed (disabled);
                // a missing config counts as empty, so manifest defaults
                // apply (see [`crate::bundled::load_config`]).
                let data_dir = self
                    .data_dir
                    .clone()
                    .unwrap_or_else(|| ext_dir.parent().unwrap_or(ext_dir).to_path_buf());
                return if crate::bundled::load_config(&data_dir)
                    .map(|c| crate::bundled::is_enabled_with_default(&c, name, manifest_enabled))
                    .unwrap_or(false)
                {
                    GateVerdict::Approved
                } else {
                    GateVerdict::NotApproved
                };
            }
            if manifest_enabled_set && !manifest_enabled {
                return GateVerdict::NotApproved;
            }
            match approval::dir_hash(plugin_dir) {
                Ok(h) => {
                    if approval::is_approved(ext_dir, name, version, &h) {
                        GateVerdict::Approved
                    } else {
                        GateVerdict::NotApproved
                    }
                }
                // A hash failure is a GATE failure, not a "not approved":
                // collapsing it to false used to pend the plugin, inviting
                // an approval that the broken hash could never satisfy.
                // Callers quarantine it instead - alone, never fail-open.
                Err(e) => GateVerdict::GateFailed(e.to_string()),
            }
        }
        pub fn names(&self) -> Vec<String> {
            self.plugins
                .iter()
                .map(|p| p.manifest.name.clone())
                .collect()
        }
        /// Fire `hook` on all providers, concatenate contexts. Never fails the
        /// turn. `extra` carries per-call context (e.g. the user message for
        /// pre_llm_call) that plugins may read.
        pub fn fire(
            &self,
            hook: Hook,
            session: &str,
            platform: &str,
            extra: std::collections::HashMap<String, String>,
        ) -> Option<String> {
            let mut parts = Vec::new();
            for pl in &self.plugins {
                if !pl.provides(hook) {
                    continue;
                }
                // A plugin that failed repeatedly degrades every turn; skip it
                // for the rest of the session (streak resets on any success).
                if self
                    .timeout_streaks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&pl.manifest.name)
                    .copied()
                    .unwrap_or(0)
                    >= SKIP_AFTER_FAILURES
                {
                    continue;
                }
                let once_key = if pl.manifest.once_per_session(Some(&pl.dir)) {
                    let key = (
                        pl.manifest.name.clone(),
                        hook.name().to_string(),
                        session.to_string(),
                    );
                    let seen = self.seen_once.lock().unwrap_or_else(|e| e.into_inner());
                    if seen.contains(&key) {
                        continue;
                    }
                    Some(key)
                } else {
                    None
                };
                let input = HookInput {
                    hook: hook.name().into(),
                    session_id: session.into(),
                    platform: platform.into(),
                    extra: self.enriched_extra(extra.clone()),
                };
                // `fire_hook_full`, not `fire_hook`: the old collapse of every
                // infrastructure error into `Ok(None)` meant the streak below
                // could never trigger on this path, and the seen key above
                // was consumed by crashes. A clean answer (context or
                // silent) is success and consumes the once-per-session key;
                // an error envelope or spawn failure bumps the streak and
                // does NOT consume the key, so one crash does not suppress
                // the plugin for the whole session.
                match fire_hook_full(pl, hook, &input, &self.cfg) {
                    Ok(out) => {
                        if let Some(msg) = out.error {
                            self.bump_streak(&pl.manifest.name);
                            eprintln!("plugin {}: {msg}", pl.manifest.name);
                        } else {
                            self.clear_streak(&pl.manifest.name);
                            if let Some(key) = once_key {
                                self.seen_once
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(key);
                            }
                            if let Some(ctx) = out.context.filter(|c| !c.trim().is_empty()) {
                                parts.push(ctx);
                            }
                        }
                    }
                    Err(e) => {
                        self.bump_streak(&pl.manifest.name);
                        if self.is_streak_exhausted(&pl.manifest.name) {
                            eprintln!(
                            "plugin {}: skipped for the session after {SKIP_AFTER_FAILURES} consecutive failures",
                            pl.manifest.name
                        );
                        } else {
                            eprintln!("plugin {}: {e}", pl.manifest.name);
                        }
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        /// Fire a gate hook (`pre_tool_call`) and resolve a verdict.
        ///
        /// Contract, deliberately asymmetric:
        /// - No plugin provides the hook => `Allow` (nothing configured to gate).
        /// - A plugin answers cleanly without `deny` => `Allow`.
        /// - A plugin answers `{"deny": true, ...}` => `Deny` (first deny wins).
        /// - A plugin errors, times out, or returns junk => `Deny` (fail closed).
        /// - A plugin skipped for repeated failures => `Deny` (fail closed), so a
        ///   wedged security plugin cannot quietly turn itself off.
        pub fn fire_gate(
            &self,
            hook: Hook,
            session: &str,
            platform: &str,
            extra: std::collections::HashMap<String, String>,
        ) -> GateDecision {
            debug_assert_eq!(
                hook.class(),
                HookClass::Gate,
                "fire_gate on a non-gate hook"
            );
            for pl in &self.plugins {
                if !pl.provides(hook) {
                    continue;
                }
                if self.is_streak_exhausted(&pl.manifest.name) {
                    return GateDecision::Deny {
                        reason: format!(
                            "plugin '{}' is disabled after {SKIP_AFTER_FAILURES} consecutive hook \
                         failures; refusing because the gate could not be consulted",
                            pl.manifest.name
                        ),
                        plugin: pl.manifest.name.clone(),
                    };
                }
                let input = HookInput {
                    hook: hook.name().into(),
                    session_id: session.into(),
                    platform: platform.into(),
                    extra: self.enriched_extra(extra.clone()),
                };
                match fire_hook_full(pl, hook, &input, &self.cfg) {
                    Ok(out) => {
                        // The shim CATCHES plugin exceptions and reports them as a
                        // clean envelope with `error` set, so a raising plugin
                        // arrives here as Ok, not Err. A gate must read both as
                        // "no answer" - otherwise the most likely real-world
                        // failure (a plugin with a bug) fails OPEN.
                        if let Some(msg) = out.error {
                            self.bump_streak(&pl.manifest.name);
                            return GateDecision::Deny {
                                reason: format!(
                                "security gate '{}' errored ({msg}); denying because it could not \
                                 be consulted",
                                pl.manifest.name
                            ),
                                plugin: pl.manifest.name.clone(),
                            };
                        }
                        self.clear_streak(&pl.manifest.name);
                        if let Some(HookDirective {
                            deny: true, reason, ..
                        }) = out.directive
                        {
                            return GateDecision::Deny {
                                reason: reason.unwrap_or_else(|| {
                                    format!("plugin '{}' denied this action", pl.manifest.name)
                                }),
                                plugin: pl.manifest.name.clone(),
                            };
                        }
                    }
                    Err(e) => {
                        self.bump_streak(&pl.manifest.name);
                        // Fail closed, and say why: a silent allow here would be
                        // indistinguishable from "no policy configured".
                        return GateDecision::Deny {
                            reason: format!(
                                "security gate '{}' failed ({e}); denying because it could not be \
                             consulted",
                                pl.manifest.name
                            ),
                            plugin: pl.manifest.name.clone(),
                        };
                    }
                }
            }
            GateDecision::Allow
        }

        /// Fire a transform hook (`transform_tool_result`) and return the payload
        /// the model should actually see.
        ///
        /// Contract, the mirror image of the gate: fails OPEN. The first plugin to
        /// return a non-empty `replacement` wins; a plugin that errors, times out,
        /// or returns nothing leaves `input` untouched. Redaction must not become
        /// an outage.
        ///
        /// Composition is first-wins in load order (plugins sort by name), NOT
        /// chained: when two transform plugins are enabled, the alphabetically
        /// first one's replacement is the whole payload and the second never
        /// sees it. There is currently no priority knob, so this is a known
        /// limitation, not a security guarantee: a default-on noise
        /// transform that sorts before a secret redactor will preempt it.
        /// Chaining is a deliberate non-goal here because a chained
        /// transform would let an early plugin rewrite text a later security
        /// plugin was supposed to see verbatim; a priority field or an
        /// explicit redaction-last pass would be the real fix if this
        /// ordering ever matters in practice.
        pub fn fire_transform(
            &self,
            hook: Hook,
            session: &str,
            platform: &str,
            extra: std::collections::HashMap<String, String>,
            input: &str,
        ) -> String {
            debug_assert_eq!(
                hook.class(),
                HookClass::Transform,
                "fire_transform on a non-transform hook"
            );
            for pl in &self.plugins {
                if !pl.provides(hook) || self.is_streak_exhausted(&pl.manifest.name) {
                    continue;
                }
                let mut payload = extra.clone();
                payload.insert("result".to_string(), input.to_string());
                let hin = HookInput {
                    hook: hook.name().into(),
                    session_id: session.into(),
                    platform: platform.into(),
                    extra: self.enriched_extra(payload),
                };
                match fire_hook_full(pl, hook, &hin, &self.cfg) {
                    Ok(out) => {
                        // An error envelope is a failure, not a clean answer:
                        // the old code cleared the streak on ANY Ok, so a
                        // transform that raised on every fire never exhausted.
                        if let Some(msg) = out.error {
                            self.bump_streak(&pl.manifest.name);
                            eprintln!("transform {}: {msg} (payload unchanged)", pl.manifest.name);
                        } else {
                            self.clear_streak(&pl.manifest.name);
                            if let Some(rep) = out.directive.and_then(|d| d.replacement) {
                                if !rep.is_empty() {
                                    return rep;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        self.bump_streak(&pl.manifest.name);
                        eprintln!("transform {}: {e} (payload unchanged)", pl.manifest.name);
                    }
                }
            }
            input.to_string()
        }

        /// Consecutive-failure streak for `plugin` (diagnostics/tests).
        pub fn streak_count(&self, plugin: &str) -> u32 {
            self.timeout_streaks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(plugin)
                .copied()
                .unwrap_or(0)
        }

        fn is_streak_exhausted(&self, plugin: &str) -> bool {
            self.timeout_streaks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(plugin)
                .copied()
                .unwrap_or(0)
                >= SKIP_AFTER_FAILURES
        }
        fn clear_streak(&self, plugin: &str) {
            self.timeout_streaks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(plugin);
        }
        fn bump_streak(&self, plugin: &str) {
            let mut streaks = self
                .timeout_streaks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let n = streaks.entry(plugin.to_string()).or_insert(0);
            *n += 1;
        }

        /// Notify observers for one event-derived fire. The return value is
        /// discarded on purpose: [`HookClass::Observer`] promises the host ignores
        /// it, and discarding here keeps that promise honest. A gate/transform
        /// routed here would silently lose its power, so it is refused loudly.
        pub fn notify(
            &self,
            hook: Hook,
            session: &str,
            platform: &str,
            extra: std::collections::HashMap<String, String>,
        ) {
            if hook.class() != HookClass::Observer {
                eprintln!(
                    "notify: '{}' is not an observer hook and needs fire_gate/fire_transform; \
                 refusing to notify",
                    hook.name()
                );
                return;
            }
            let _ = self.fire(hook, session, platform, extra);
        }

        pub fn plugin_dir(&self, name: &str) -> Option<PathBuf> {
            self.plugins
                .iter()
                .find(|p| p.manifest.name == name)
                .map(|p| p.dir.clone())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        fn scratch(tag: &str) -> PathBuf {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let d = std::env::temp_dir().join(format!(
                "pantheon-ext-mgr-test-{tag}-{n}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        fn write_plugin(dir: &Path, manifest: &str, init_py: &str) {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("plugin.yaml"), manifest).unwrap();
            std::fs::write(dir.join("__init__.py"), init_py).unwrap();
        }

        const ECHO_INIT: &str = "def register(ctx):\n    def pre_llm_call(**kw):\n        return {'context': 'hello'}\n    ctx.register_hook('pre_llm_call', pre_llm_call)\n";

        fn approved_mgr(ext_dir: &Path) -> ExtensionManager {
            let mut mgr = ExtensionManager::new(RunnerConfig::default());
            mgr.load_dir(ext_dir).unwrap();
            mgr
        }

        fn approve(ext_dir: &Path, dir: &Path, name: &str, version: &str) {
            approval::record_approval_for(ext_dir, name, version, dir).unwrap();
        }

        #[test]
        fn third_party_explicit_enabled_false_does_not_load_even_when_approved() {
            let root = scratch("disabled");
            let ext = root.join("ext");
            let dir = ext.join("demo");
            write_plugin(
                &dir,
                "name: demo\nversion: \"1.0\"\nenabled: false\nprovides_hooks: [pre_llm_call]\n",
                ECHO_INIT,
            );
            approve(&ext, &dir, "demo", "1.0");
            let mgr = approved_mgr(&ext);
            assert!(
                mgr.names().is_empty(),
                "explicit enabled:false must not fire"
            );
            assert_eq!(mgr.pending().len(), 1);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn third_party_omitted_enabled_approved_loads() {
            let root = scratch("omitted");
            let ext = root.join("ext");
            let dir = ext.join("demo");
            write_plugin(
                &dir,
                "name: demo\nversion: \"1.0\"\nprovides_hooks: [pre_llm_call]\n",
                ECHO_INIT,
            );
            approve(&ext, &dir, "demo", "1.0");
            let mgr = approved_mgr(&ext);
            assert_eq!(mgr.names(), vec!["demo".to_string()]);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn duplicate_manifest_names_conflict_loudly_on_pending_side() {
            let root = scratch("dup");
            let ext = root.join("ext");
            // Two unapproved dirs with one name: the first goes pending,
            // the second must surface as a conflict, not a second pending
            // entry that stays unresolvable after approving the first.
            let manifest = "name: demo\nversion: \"1.0\"\nprovides_hooks: [pre_llm_call]\n";
            let a = ext.join("a-demo");
            let b = ext.join("b-demo");
            write_plugin(&a, manifest, ECHO_INIT);
            write_plugin(&b, manifest, ECHO_INIT);
            let mgr = approved_mgr(&ext);
            assert!(mgr.names().is_empty());
            assert_eq!(mgr.pending().len(), 1);
            assert_eq!(mgr.conflicts().len(), 1, "conflicts: {:?}", mgr.conflicts());
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        #[cfg(unix)]
        fn duplicate_manifest_names_conflict_loudly_when_loaded() {
            use std::os::unix::fs::symlink;
            let root = scratch("dup-load");
            let ext = root.join("ext");
            // Both dirs are symlinks to one real tree, so both pass the
            // name-keyed approval check (the approval hash binds the
            // target's inodes, which are shared) - the loaded-side conflict
            // must still fire, keeping only the first.
            let real = root.join("real");
            write_plugin(
                &real,
                "name: demo\nversion: \"1.0\"\nprovides_hooks: [pre_llm_call]\n",
                ECHO_INIT,
            );
            std::fs::create_dir_all(&ext).unwrap();
            symlink(&real, ext.join("a-demo")).unwrap();
            symlink(&real, ext.join("b-demo")).unwrap();
            approve(&ext, &ext.join("a-demo"), "demo", "1.0");
            let mgr = approved_mgr(&ext);
            assert_eq!(mgr.names(), vec!["demo".to_string()]);
            assert_eq!(mgr.conflicts().len(), 1, "conflicts: {:?}", mgr.conflicts());
            assert!(mgr.pending().is_empty());
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn broken_manifest_is_a_recorded_load_error_not_silence() {
            let root = scratch("broken");
            let ext = root.join("ext");
            let dir = ext.join("bad");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("plugin.yaml"), "name: [\n").unwrap();
            let mgr = approved_mgr(&ext);
            assert!(mgr.names().is_empty());
            assert_eq!(mgr.load_errors().len(), 1);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn context_error_bumps_streak_and_does_not_consume_once_per_session() {
            let root = scratch("streak");
            let ext = root.join("ext");
            let dir = ext.join("flaky");
            let marker = root.join("fail.flag");
            std::fs::write(&marker, "1").unwrap();
            let init = format!(
            "import os\ndef register(ctx):\n    def pre_llm_call(**kw):\n        if os.path.exists({marker:?}):\n            raise RuntimeError('boom')\n        return {{'context': 'recovered'}}\n    ctx.register_hook('pre_llm_call', pre_llm_call)\n",
            marker = marker.to_string_lossy()
        );
            write_plugin(
            &dir,
            "name: flaky\nversion: \"1.0\"\nonce_per_session: true\nprovides_hooks: [pre_llm_call]\n",
            &init,
        );
            approve(&ext, &dir, "flaky", "1.0");
            let mgr = approved_mgr(&ext);
            let extra = std::collections::HashMap::new();
            assert_eq!(
                mgr.fire(Hook::PreLlmCall, "s1", "test", extra.clone()),
                None
            );
            assert_eq!(mgr.streak_count("flaky"), 1);
            // One crash must not consume the once-per-session key: remove
            // the failure condition and the plugin still fires.
            std::fs::remove_file(&marker).unwrap();
            assert_eq!(
                mgr.fire(Hook::PreLlmCall, "s1", "test", extra.clone())
                    .as_deref(),
                Some("recovered")
            );
            assert_eq!(mgr.streak_count("flaky"), 0);
            // And now the once-per-session key IS consumed by success.
            assert_eq!(mgr.fire(Hook::PreLlmCall, "s1", "test", extra), None);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn transform_error_envelope_bumps_streak() {
            let root = scratch("transform");
            let ext = root.join("ext");
            let dir = ext.join("boom");
            write_plugin(
            &dir,
            "name: boom\nversion: \"1.0\"\nprovides_hooks: [transform_tool_result]\n",
            "def register(ctx):\n    def transform_tool_result(**kw):\n        raise RuntimeError('boom')\n    ctx.register_hook('transform_tool_result', transform_tool_result)\n",
        );
            approve(&ext, &dir, "boom", "1.0");
            let mgr = approved_mgr(&ext);
            let extra = std::collections::HashMap::new();
            for _ in 0..3 {
                assert_eq!(
                    mgr.fire_transform(
                        Hook::TransformToolResult,
                        "s",
                        "test",
                        extra.clone(),
                        "payload"
                    ),
                    "payload"
                );
            }
            assert_eq!(mgr.streak_count("boom"), 3);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn hook_fire_receives_project_root_in_extra() {
            let root = scratch("projroot");
            let ext = root.join("ext");
            let dir = ext.join("echo");
            write_plugin(
            &dir,
            "name: echo\nversion: \"1.0\"\nprovides_hooks: [pre_llm_call]\n",
            "def register(ctx):\n    def pre_llm_call(**kw):\n        return {'context': 'root=' + str(kw.get('project_root'))}\n    ctx.register_hook('pre_llm_call', pre_llm_call)\n",
        );
            approve(&ext, &dir, "echo", "1.0");
            let proj = root.join("myproj");
            let mut mgr =
                ExtensionManager::new(RunnerConfig::default()).with_project_root(proj.clone());
            mgr.load_dir(&ext).unwrap();
            let out = mgr.fire(
                Hook::PreLlmCall,
                "s",
                "test",
                std::collections::HashMap::new(),
            );
            let expected = format!("root={}", proj.display());
            assert_eq!(out.as_deref(), Some(expected.as_str()));
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}

pub mod manifest {
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
            let text = std::fs::read_to_string(path)
                .map_err(|e| xerr("EXT_MANIFEST_READ", e.to_string()))?;
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
            let mut m: Self = serde_yaml::from_str(&text)
                .map_err(|e| xerr("EXT_MANIFEST_PARSE", e.to_string()))?;
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
}

pub mod python_runner {
    //! Python hook runner: spawns `__init__.py`-style plugins in a subprocess.
    //! V0 contract: one JSON line in on stdin, one JSON line out on stdout.
    //! Fail-open: any crash/timeout/bad output => Ok(None), never a broken turn.
    use crate::hooks::Hook;
    use crate::manifest::PluginManifest;
    use pantheon_api::capability::{Capability, Policy};
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_exec::sandbox::{build_sandboxed_checked, enforce, Enforcement};
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus, Stdio};
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
        /// Human-readable reason for a deny (gate) - surfaced to the model/user.
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
        /// Extensions root that approved this plugin, set by the manager at
        /// load time. `fire_hook_full` re-verifies the plugin dir against this
        /// scope immediately before every spawn (canonicalize + containment +
        /// approval-hash re-check, fail closed). `None` = standalone/test
        /// use: no fire-time re-verification, as before.
        pub scope_dir: Option<PathBuf>,
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
                scope_dir: None,
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
        /// Data dir handed to hook children as the single allowlisted
        /// `PANTHEON_DATA_DIR` env var (plugins such as
        /// self-improving-agent resolve their learnings dir from it). The
        /// child env is otherwise still scrubbed to PATH only.
        pub data_dir: Option<PathBuf>,
        /// Capability policy gating plugin spawns. `Some` = every spawn goes
        /// through [`enforce`] first: `Deny` refuses the spawn, `Approval`
        /// holds it at the boundary, `Allow` runs it inside the enforcement's
        /// sandbox profile. `None` = no policy configured: spawns run as
        /// before (direct child, minimal env), preserving the standalone /
        /// test behavior.
        pub policy: Option<Policy>,
    }

    impl Default for RunnerConfig {
        fn default() -> Self {
            Self {
                // Overridable, not hardcoded: a missing/broken `python3` on
                // PATH used to turn every gate-plugin fire into a fail-closed
                // denial (total tool outage) with no operator recourse short
                // of fixing PATH. `PANTHEON_PYTHON` names the interpreter.
                python: std::env::var("PANTHEON_PYTHON")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "python3".into()),
                timeout: Duration::from_secs(10),
                policy: None,
                data_dir: None,
            }
        }
    }

    impl RunnerConfig {
        /// Config with a policy attached (builder-style; the policy gates
        /// every plugin spawn - see [`fire_hook_full`]).
        pub fn with_policy(mut self, policy: Policy) -> Self {
            self.policy = Some(policy);
            self
        }
    }

    /// The shim: imports the plugin `__init__`, calls `register(ctx)` with a
    /// capturing ctx, dispatches the requested hook, prints result JSON.
    ///
    /// Hash-bound at load: the host passes the expected sha256 of the
    /// verified `__init__.py` as argv[4]. The shim reads the entrypoint ONCE,
    /// refuses to run when the bytes differ from what the host verified
    /// (a swap between the host's check and this read fails closed here),
    /// and executes EXACTLY the bytes it hashed - there is no
    /// check-then-use gap inside the child between hashing and importing.
    const SHIM: &str = r#"
import hashlib, importlib.util, json, sys
plug_dir, hook_name, payload_json, expected_sha = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
entry = plug_dir + "/__init__.py"
result = {"context": None}
try:
    with open(entry, "rb") as f:
        src = f.read()
    if hashlib.sha256(src).hexdigest() != expected_sha:
        raise RuntimeError(
            "plugin __init__.py changed after host verification; refusing to run swapped code")
    spec = importlib.util.spec_from_file_location(
        "pantheon_plugin", entry)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["pantheon_plugin"] = mod
    exec(compile(src, entry, "exec"), mod.__dict__)
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

    /// Hash-bind at spawn for hook plugins. The manager's approval check runs
    /// at load time; the plugin dir is re-verified immediately before every
    /// hook fire and the CANONICAL dir is what the child receives.
    ///
    /// Returns the canonical dir plus the expected sha256 of the verified
    /// `__init__.py`, which the SHIM re-checks inside the child before
    /// importing (see `SHIM`). The entrypoint bytes are sandwiched around the
    /// approval hash - read, hash+approve, re-read and compare - so the
    /// expected hash is bound to bytes that were part of an approved tree:
    /// a swap at any point fails closed here, and a swap after this function
    /// returns fails closed in the SHIM. Either way swapped bytes never
    /// execute.
    ///
    /// Callers must not execute on `Err`; `fire_hook` collapses this to
    /// silence, `fire_hook_full` surfaces it so gates can distinguish
    /// "plugin refused" from "plugin allowed".
    fn verify_hook_spawn(plugin: &PythonPlugin) -> Result<(PathBuf, String), PantheonError> {
        let tampered = |cause: String| xerr("EXT_TAMPERED", cause, false);
        let canon_dir = plugin
            .dir
            .canonicalize()
            .map_err(|e| tampered(format!("canonicalize {}: {e}", plugin.dir.display())))?;
        if let Some(scope) = plugin.scope_dir.as_ref() {
            let canon_scope = scope
                .canonicalize()
                .map_err(|e| tampered(format!("canonicalize {}: {e}", scope.display())))?;
            if !canon_dir.starts_with(&canon_scope) {
                return Err(tampered(format!(
                    "plugin dir {} escapes the extensions dir",
                    canon_dir.display()
                )));
            }
            if !pantheon_api::approval::is_bundled(&canon_scope, &canon_dir) {
                let entry = canon_dir.join("__init__.py");
                // (1) Read the entrypoint bytes BEFORE the approval hash.
                let before = std::fs::read(&entry)
                    .map_err(|e| tampered(format!("read {}: {e}", entry.display())))?;
                // (2) The tree must be in an approved state.
                let hash = pantheon_api::approval::dir_hash(&canon_dir)
                    .map_err(|e| tampered(format!("hash {}: {e}", canon_dir.display())))?;
                if !pantheon_api::approval::is_approved(
                    &canon_scope,
                    &plugin.manifest.name,
                    &plugin.manifest.version,
                    &hash,
                ) {
                    return Err(tampered(format!(
                        "plugin '{}' changed since approval; re-approve to run it",
                        plugin.manifest.name
                    )));
                }
                // (3) Re-read: the entrypoint must be byte-identical to what
                // step (1) saw, otherwise the expected hash below would not
                // be bound to the approved tree.
                let after = std::fs::read(&entry)
                    .map_err(|e| tampered(format!("re-read {}: {e}", entry.display())))?;
                if before != after {
                    return Err(tampered(format!(
                        "plugin '{}' changed during verification",
                        plugin.manifest.name
                    )));
                }
                let expected = pantheon_api::approval::bytes_hash(&before);
                return Ok((canon_dir, expected));
            }
        }
        // Standalone (no scope) or bundled: no approval hash to bind against.
        // Still hand the SHIM the current bytes' hash so a swap between here
        // and the child's import fails closed instead of executing.
        let entry = canon_dir.join("__init__.py");
        let bytes = std::fs::read(&entry)
            .map_err(|e| tampered(format!("read {}: {e}", entry.display())))?;
        if !canon_dir.join("plugin.yaml").is_file() {
            return Err(tampered(format!(
                "plugin manifest missing in {}",
                canon_dir.display()
            )));
        }
        Ok((canon_dir, pantheon_api::approval::bytes_hash(&bytes)))
    }

    /// Fire one hook and return the full answer, INCLUDING an error for
    /// infrastructure failure.
    ///
    /// This distinction is the whole point: a context hook can collapse failure
    /// into silence, but a *gate* must be able to tell "the plugin allowed this"
    /// apart from "the plugin never answered". So this returns `Err` on spawn
    /// failure, crash, timeout, and unparseable output; a clean answer - even an
    /// empty one - is `Ok`.
    pub fn fire_hook_full(
        plugin: &PythonPlugin,
        hook: Hook,
        input: &HookInput,
        cfg: &RunnerConfig,
    ) -> Result<HookOutput, PantheonError> {
        let payload = serde_json::to_string(input)
            .map_err(|e| xerr("EXT_INPUT_ENCODE", e.to_string(), false))?;
        // Sandbox enforcement: the capability policy owns *whether* the
        // plugin process may run at all. `None` = no policy configured
        // (standalone/test use): the spawn proceeds un-gated, as before.
        let profile = match cfg.policy.as_ref() {
            Some(policy) => match enforce(policy, &Capability::PluginEnable) {
                Enforcement::Run(profile) => Some(profile),
                Enforcement::Deny { reason, .. } => {
                    return Err(xerr(
                        "EXT_DENIED",
                        format!("plugin.enable: {reason}"),
                        false,
                    ))
                }
                Enforcement::RequireApproval { scope, .. } => {
                    return Err(xerr(
                        "EXT_APPROVAL_REQUIRED",
                        format!("plugin.enable requires approval (scope: {scope})"),
                        false,
                    ))
                }
            },
            None => None,
        };
        let arg_dir = {
            // Re-verify immediately before spawn: canonicalize, prove
            // containment, re-hash against the approval store with the
            // entrypoint bytes sandwiched around the hash, fail closed on
            // any divergence since load. The canonical dir - not the raw
            // configured path - is what the child loads `__init__.py` from,
            // and the SHIM executes exactly the bytes this hash covers.
            let (canon_dir, expected_sha) = verify_hook_spawn(plugin)?;
            (canon_dir.to_string_lossy().to_string(), expected_sha)
        };
        let (arg_dir, expected_sha) = arg_dir;
        let args = [
            "-c",
            SHIM,
            arg_dir.as_str(),
            hook.name(),
            payload.as_str(),
            expected_sha.as_str(),
        ];
        // The plugin runs in its own directory; canonicalize always yields an
        // absolute path, so it is a valid cwd for build_sandboxed.
        let cwd = arg_dir.clone();
        let (mut cmd, group_leader) = match &profile {
            // Policy allowed it: the enforcement's sandbox profile decides
            // how isolated the child runs. `build_sandboxed_checked` applies
            // the same fail-closed gate as `run_sandboxed`: a boundary that
            // needs a wrapper which cannot initialize refuses the spawn
            // (`EXT_SANDBOX`) unless the direct fallback is opted in - the
            // policy gate above decides *whether*, this guarantees *how
            // isolated*.
            Some(profile) => {
                let (cmd, _sandboxed) = build_sandboxed_checked(profile, &cfg.python, &args, &cwd)
                    .map_err(|e| xerr("EXT_SANDBOX", e.cause, false))?;
                // The checked build confines the child via pre_exec setsid(),
                // so it is a process-group leader and run_drained may killpg
                // the group on a stuck drain. A direct spawn is not a group
                // leader - signalling its pid as a group could hit unrelated
                // processes.
                (cmd, true)
            }
            None => {
                let mut cmd = Command::new(&cfg.python);
                cmd.args(args);
                cmd.current_dir(&cwd);
                (cmd, false)
            }
        };
        // Third-party plugin code: no ambient host env crosses the boundary.
        crate::minimal_child_env(&mut cmd);
        // One allowlisted exception: the data dir, so plugins can resolve
        // operator-owned state (learnings, plans) instead of falling back to
        // $HOME. Set only when the host configured one.
        if let Some(dd) = &cfg.data_dir {
            cmd.env("PANTHEON_DATA_DIR", dd);
        }
        let (status, out_bytes, _err_bytes) =
            run_drained(cmd, cfg.timeout, "EXT_TIMEOUT", "plugin hook", group_leader)?;
        if !status.success() {
            return Err(xerr("EXT_CRASH", format!("plugin exited {status}"), false));
        }
        let out = String::from_utf8_lossy(&out_bytes);
        serde_json::from_str(out.trim()).map_err(|e| xerr("EXT_BAD_OUTPUT", e.to_string(), false))
    }

    /// Cap per captured pipe, in bytes: a plugin's answer is one JSON line,
    /// so anything past this is protocol breakage, not data. Bounded so a
    /// chatty plugin cannot grow the parent without limit.
    const PIPE_CAP_BYTES: usize = 1024 * 1024;

    /// Grace after the child is gone (exited or killed) for the pipe
    /// drainers to observe EOF before we stop waiting for them. A
    /// grandchild that inherited the pipe can hold it open indefinitely
    /// CPython file descriptors are inheritable by default, so a plugin
    /// that double-forks leaves the write end open after the direct child
    /// is dead. Waiting for the drainers without a bound hangs the caller;
    /// the old code did exactly that (its "the pipes hit EOF once the child
    /// is dead, so joining cannot hang" comment was wrong).
    const POST_EXIT_DRAIN_GRACE: Duration = Duration::from_secs(5);

    /// Tail wait after a group-kill: the pipe holder usually dies fast, and
    /// its last bytes may already sit in the pipe. Bounded; whatever is
    /// buffered when it expires is what we keep.
    const POST_KILL_DRAIN_GRACE: Duration = Duration::from_secs(1);

    /// Spawn `cmd` (already configured with piped stdout/stderr) and wait up
    /// to `timeout`, draining BOTH pipes on reader threads from the moment
    /// of spawn.
    ///
    /// A child that fills the 64KiB pipe buffer while the parent only
    /// `try_wait()`s wedges forever: the child blocks on write, the parent
    /// blocks on wait, and the watchdog misreports the wedge as a timeout.
    /// Draining concurrently removes the wedge; a genuinely slow plugin
    /// still trips the timeout and is killed.
    ///
    /// `group_leader` must be true exactly when the child was spawned as a
    /// process-group leader - every `build_sandboxed` command is, via
    /// `setsid()` in `pre_exec`; a direct spawn is not. Only then may we
    /// `killpg` the group when a pipe-holding grandchild outlives the child:
    /// signalling a pid that is not a group leader could hit unrelated
    /// processes.
    ///
    /// Returns `(status, stdout, stderr)`, each stream truncated to
    /// [`PIPE_CAP_BYTES`]. `timeout_code`/`what` name the timeout error.
    fn run_drained(
        mut cmd: Command,
        timeout: Duration,
        timeout_code: &'static str,
        what: &'static str,
        group_leader: bool,
    ) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), PantheonError> {
        use std::io::Read;
        use std::sync::{Arc, Mutex};
        /// Drain one pipe on a thread, appending into a shared buffer as
        /// bytes arrive - so the parent can take what has been captured
        /// even if EOF never comes. Returns the thread and the buffer.
        fn drain<R: Read + Send + 'static>(
            pipe: R,
        ) -> (std::thread::JoinHandle<()>, Arc<Mutex<Vec<u8>>>) {
            let buf = Arc::new(Mutex::new(Vec::new()));
            let buf2 = Arc::clone(&buf);
            let h = std::thread::spawn(move || {
                let mut pipe = pipe;
                let mut chunk = [0u8; 8192];
                loop {
                    let n = match pipe.read(&mut chunk) {
                        Ok(0) => break, // EOF
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    if let Ok(mut kept) = buf2.lock() {
                        let room = PIPE_CAP_BYTES.saturating_sub(kept.len());
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            });
            (h, buf)
        }
        /// Finish one drainer after the child is gone: wait up to
        /// [`POST_EXIT_DRAIN_GRACE`] for EOF, then stop waiting and return
        /// the bytes captured so far. On expiry, when `group_leader` is set,
        /// SIGKILL the child's process group first (with the same
        /// never-signal-init-or-ourselves guard the sandbox runner uses), so
        /// a pipe-holding grandchild dies and the drainer can still observe
        /// EOF - then a short tail wait for its last bytes. Never hangs; in
        /// the worst case a blocked reader thread is left behind instead of
        /// the caller.
        fn finish_drain(
            handle: std::thread::JoinHandle<()>,
            buf: Arc<Mutex<Vec<u8>>>,
            child_pid: u32,
            group_leader: bool,
        ) -> Vec<u8> {
            let buffered = || buf.lock().map(|g| g.clone()).unwrap_or_default();
            let deadline = std::time::Instant::now() + POST_EXIT_DRAIN_GRACE;
            loop {
                if handle.is_finished() {
                    let _ = handle.join();
                    return buffered();
                }
                if std::time::Instant::now() >= deadline {
                    #[cfg(unix)]
                    if group_leader && child_pid > 1 && child_pid != std::process::id() {
                        // SAFETY: libc::killpg with a validated pgid; no
                        // memory unsafety involved.
                        unsafe { libc::killpg(child_pid as libc::pid_t, libc::SIGKILL) };
                    }
                    // Short tail wait: the holder usually dies at once and
                    // its last bytes are already in the pipe.
                    let tail = std::time::Instant::now() + POST_KILL_DRAIN_GRACE;
                    while !handle.is_finished() && std::time::Instant::now() < tail {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let _ = handle.is_finished().then(|| handle.join());
                    return buffered();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| xerr("EXT_SPAWN", e.to_string(), false))?;
        let child_pid = child.id();
        let stdout_drainer = child.stdout.take().map(drain);
        let stderr_drainer = child.stderr.take().map(drain);

        let start = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        // Bounded drain after the kill too: a grandchild
                        // holding a pipe open must not hang the timeout
                        // path either. (Bytes are discarded; the run
                        // already failed.)
                        for (h, b) in stdout_drainer.into_iter().chain(stderr_drainer) {
                            finish_drain(h, b, child_pid, group_leader);
                        }
                        return Err(xerr(
                            timeout_code,
                            format!("{what} exceeded {}s", timeout.as_secs()),
                            true,
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(xerr("EXT_WAIT", e.to_string(), true)),
            }
        };
        let out = stdout_drainer
            .map(|(h, b)| finish_drain(h, b, child_pid, group_leader))
            .unwrap_or_default();
        let err = stderr_drainer
            .map(|(h, b)| finish_drain(h, b, child_pid, group_leader))
            .unwrap_or_default();
        Ok((status, out, err))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

        /// A real plugin dir on disk: `plugin.yaml` + `__init__.py` whose
        /// `pre_llm_call` handler floods stderr with 200KB (well past the
        /// 64KiB pipe buffer) and then answers cleanly.
        fn chatty_plugin_dir() -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-ext-drain-test-{}-{}",
                std::process::id(),
                FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("plugin.yaml"),
                "name: drain-fixture\nprovides_hooks: [pre_llm_call]\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("__init__.py"),
                "import sys\n\
             def register(ctx):\n\
             \x20   def pre_llm_call(**kw):\n\
             \x20       sys.stderr.write('e' * 200000)\n\
             \x20       sys.stderr.flush()\n\
             \x20       return {'context': 'plugin-says-hi'}\n\
             \x20   ctx.register_hook('pre_llm_call', pre_llm_call)\n",
            )
            .unwrap();
            dir
        }

        fn hook_input() -> HookInput {
            HookInput {
                hook: Hook::PreLlmCall.name().to_string(),
                session_id: "test-session".to_string(),
                platform: "test".to_string(),
                extra: HashMap::new(),
            }
        }

        /// Item 1 regression, helper level: 200KB on stdout *and* stderr
        /// must be captured, not wedged. The old try_wait-only loop blocked
        /// here (child stuck on write, parent on wait) and the watchdog
        /// misreported the wedge as a timeout.
        #[test]
        fn drained_spawn_captures_pipe_flood_without_wedging() {
            let mut cmd = Command::new("python3");
            cmd.arg("-c").arg(
                "import sys; sys.stdout.write('o' * 200000); sys.stdout.flush(); \
             sys.stderr.write('e' * 200000); sys.stderr.flush()",
            );
            // Generous: the run takes well under a second; the timeout is
            // only a backstop so a regression fails instead of hanging.
            let (status, out, err) =
                run_drained(cmd, Duration::from_secs(30), "EXT_TIMEOUT", "test", false)
                    .expect("drained spawn should complete");
            assert!(status.success());
            assert_eq!(out.len(), 200_000, "stdout fully captured");
            assert_eq!(err.len(), 200_000, "stderr fully captured");
        }

        /// Item 3: a grandchild that inherits the pipe must not hang the
        /// drain. The child calls `os.setsid()` (so it is its own process
        /// group, like every `build_sandboxed` child), spawns `sleep 60`
        /// with the pipe as its stdout - the grandchild holds the write end
        /// open - then prints its answer and exits at once. The pipe never
        /// hits EOF while the grandchild lives, and CPython fds are
        /// inheritable by default, so a daemonizing plugin does exactly
        /// this. Before the fix `run_drained` joined the drainer
        /// unconditionally and hung here forever (fail-before was shown
        /// with `timeout` -> exit 124); now the grace expires, the group is
        /// killed, and the captured answer is returned.
        #[test]
        fn grandchild_holding_stdout_does_not_hang_drain() {
            let mut cmd = Command::new("python3");
            cmd.arg("-c").arg(
                "import os, subprocess, sys; \
             os.setsid(); \
             subprocess.Popen(['sleep', '60'], stdout=sys.stdout, stderr=sys.stderr); \
             sys.stdout.write('PLUGIN_ANSWER\\n'); sys.stdout.flush()",
            );
            let start = std::time::Instant::now();
            let (status, out, _err) =
                run_drained(cmd, Duration::from_secs(30), "EXT_TIMEOUT", "test", true)
                    .expect("drain must complete despite the pipe-holding grandchild");
            let elapsed = start.elapsed();
            assert!(status.success(), "child exited 0");
            assert!(
                out.windows(b"PLUGIN_ANSWER".len())
                    .any(|w| w == b"PLUGIN_ANSWER"),
                "the child's answer must be captured, got: {:?}",
                String::from_utf8_lossy(&out),
            );
            assert!(
            elapsed < Duration::from_secs(30),
            "drain must return via the grace, not outlive the grandchild's 60s sleep (took {elapsed:?})"
        );
        }

        /// Item 1 regression, end to end: a plugin flooding stderr past the
        /// pipe buffer still completes with its answer instead of timing out.
        #[test]
        fn hook_full_completes_despite_chatty_stderr() {
            let dir = chatty_plugin_dir();
            let plugin = PythonPlugin::load(&dir).expect("fixture plugin loads");
            let cfg = RunnerConfig {
                timeout: Duration::from_secs(15),
                ..RunnerConfig::default()
            };
            let out = fire_hook_full(&plugin, Hook::PreLlmCall, &hook_input(), &cfg)
                .expect("chatty plugin should complete, not time out");
            assert_eq!(out.context.as_deref(), Some("plugin-says-hi"));
            std::fs::remove_dir_all(&dir).ok();
        }

        /// Item 2: a policy that denies `plugin.enable` refuses the spawn
        /// no child is ever started (a spawn would surface as a different
        /// error, never EXT_DENIED).
        #[test]
        fn hook_full_denied_when_policy_denies_plugin_enable() {
            let dir = chatty_plugin_dir();
            let plugin = PythonPlugin::load(&dir).expect("fixture plugin loads");
            // Policy::default() is default-deny: no rule grants plugin.enable.
            let cfg = RunnerConfig::default().with_policy(Policy::default());
            let err = fire_hook_full(&plugin, Hook::PreLlmCall, &hook_input(), &cfg)
                .expect_err("denied policy must refuse the spawn");
            assert_eq!(err.code, "EXT_DENIED", "unexpected error: {err:?}");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// Item 2: a policy that allows `plugin.enable` lets the spawn
        /// proceed through the sandbox profile - the gate was consulted and
        /// passed.
        ///
        /// `PluginEnable` maps to `VeryHigh`, so this needs a working bwrap
        /// user namespace. Hosts that refuse the uid map fail closed by
        /// design, which is not what this test is about: it is about the
        /// policy gate letting the spawn through. Skip when the host cannot
        /// provide the boundary, and say so.
        #[test]
        fn hook_full_proceeds_when_policy_allows_plugin_enable() {
            if !pantheon_exec::sandbox::boundary_available(
                pantheon_exec::sandbox::ExecutionBoundary::StrictNamespaces,
            ) {
                eprintln!("SKIP: host cannot build the StrictNamespaces boundary (bwrap userns)");
                return;
            }
            let dir = chatty_plugin_dir();
            let plugin = PythonPlugin::load(&dir).expect("fixture plugin loads");
            let policy = Policy::default().allow(Capability::PluginEnable);
            let cfg = RunnerConfig {
                timeout: Duration::from_secs(30),
                ..RunnerConfig::default().with_policy(policy)
            };
            let out = fire_hook_full(&plugin, Hook::PreLlmCall, &hook_input(), &cfg)
                .expect("allowed policy should let the spawn proceed");
            assert_eq!(out.context.as_deref(), Some("plugin-says-hi"));
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
