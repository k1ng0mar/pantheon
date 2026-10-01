//! Extension manager: owns loaded plugins, fires hooks in manifest order,
//! concatenates injected contexts. Fail-open per plugin.
//!
//! Session-scoped dedup: plugins marked `once_per_session: true` in
//! plugin.yaml (or Hermes `__init__.py` plugins that keep in-process
//! `_seen_sessions`, which our subprocess runner can't preserve) fire at
//! most once per (plugin, hook, session). The manager owns this, not the
//! plugin process — required because each fire spawns fresh.
use crate::hooks::{Hook, HookClass};
use crate::python_runner::{
    fire_hook, fire_hook_full, HookDirective, HookInput, PythonPlugin, RunnerConfig,
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

/// Consecutive hook failures after which a plugin is skipped for the
/// session. A single timeout stays fail-open (one bad call should not
/// disable the plugin), but a wedge degrades every turn until stopped.
const SKIP_AFTER_FAILURES: u32 = 3;

/// Immediate subdirs of `dir` containing a plugin.yaml, sorted. A
/// missing or unreadable dir yields nothing — used for the optional
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
        }
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
    /// without approval.
    pub fn load_dir(&mut self, dir: &Path) -> Result<Vec<String>, PantheonError> {
        // The top-level dir keeps its old contract: unreadable means an
        // error the caller can report. The bundled subdir is optional —
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
        for p in tops {
            self.load_plugin_dir(dir, &p);
        }
        for p in plugin_dirs(&dir.join("bundled")) {
            self.load_plugin_dir(dir, &p);
        }
        self.plugins
            .sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        self.pending.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(self.names())
    }

    /// Try one plugin directory: load it when approved (bundled plugins
    /// consult the config enablement flag, third-party ones the approval
    /// store), otherwise record why it was skipped.
    fn load_plugin_dir(&mut self, ext_dir: &Path, p: &Path) {
        match PythonPlugin::load(p) {
            Ok(pl) => {
                let name = pl.manifest.name.clone();
                if self.approved_here(
                    ext_dir,
                    p,
                    &pl.manifest.name,
                    &pl.manifest.version,
                    pl.manifest.enabled,
                ) {
                    if !self.plugins.iter().any(|q| q.manifest.name == name) {
                        // Bind the approval scope: every hook fire re-hashes
                        // the plugin dir against this scope immediately
                        // before spawn and fails closed on divergence.
                        let mut pl = pl;
                        pl.scope_dir = Some(ext_dir.to_path_buf());
                        self.plugins.push(pl);
                    }
                } else if approval::is_bundled(ext_dir, p) {
                    // Bundled but disabled in config: not pending
                    // approval, just off. Never silently enabled.
                    eprintln!(
                        "extension '{name}' is bundled but disabled; enable it with [plugins.{name}] enabled = true"
                    );
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
            Err(e) => eprintln!("skip {}: {e}", p.display()),
        }
    }
    pub fn load_one(&mut self, dir: &Path) -> Result<String, PantheonError> {
        let pl = PythonPlugin::load(dir)?;
        let name = pl.manifest.name.clone();
        // `load_one` targets a specific plugin dir; the extensions root is
        // its parent. Approval still applies — an explicit load is not
        // consent.
        let ext_dir = dir.parent().unwrap_or(dir);
        if !self.approved_here(
            ext_dir,
            dir,
            &pl.manifest.name,
            &pl.manifest.version,
            pl.manifest.enabled,
        ) {
            if approval::is_bundled(ext_dir, dir) {
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
    /// Approval check for one plugin dir. Bundled (first-party) plugins
    /// always pass; third-party plugins need a live approval bound to the
    /// current content hash.
    fn approved_here(
        &self,
        ext_dir: &Path,
        plugin_dir: &Path,
        name: &str,
        version: &str,
        manifest_enabled: bool,
    ) -> bool {
        if approval::is_bundled(ext_dir, plugin_dir) {
            // Bundled (first-party) plugins skip the third-party approval
            // store. Enablement: the config file (`[plugins.<name>]`) is
            // the single enablement state, shared with the dashboard, the
            // mobile app, and the agent — and it wins when present. When
            // absent, the bundled manifest's own `enabled` flag is the
            // default (true only for plugins that ship on, like
            // noisegate). A missing or unparsable config fails closed
            // (disabled).
            let data_dir = ext_dir.parent().unwrap_or(ext_dir);
            return crate::bundled::load_config(data_dir)
                .map(|c| crate::bundled::is_enabled_with_default(&c, name, manifest_enabled))
                .unwrap_or(false);
        }
        match approval::dir_hash(plugin_dir) {
            Ok(h) => approval::is_approved(ext_dir, name, version, &h),
            Err(_) => false,
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
            if pl.manifest.once_per_session(Some(&pl.dir)) {
                let key = (
                    pl.manifest.name.clone(),
                    hook.name().to_string(),
                    session.to_string(),
                );
                let mut seen = self.seen_once.lock().unwrap_or_else(|e| e.into_inner());
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);
            }
            let input = HookInput {
                hook: hook.name().into(),
                session_id: session.into(),
                platform: platform.into(),
                extra: extra.clone(),
            };
            match fire_hook(pl, hook, &input, &self.cfg) {
                Ok(Some(ctx)) => {
                    self.timeout_streaks
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&pl.manifest.name);
                    parts.push(ctx)
                }
                Ok(None) => {}
                Err(e) => {
                    let mut streaks = self
                        .timeout_streaks
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let n = streaks.entry(pl.manifest.name.clone()).or_insert(0);
                    *n += 1;
                    if *n == SKIP_AFTER_FAILURES {
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
                extra: extra.clone(),
            };
            match fire_hook_full(pl, hook, &input, &self.cfg) {
                Ok(out) => {
                    // The shim CATCHES plugin exceptions and reports them as a
                    // clean envelope with `error` set, so a raising plugin
                    // arrives here as Ok, not Err. A gate must read both as
                    // "no answer" — otherwise the most likely real-world
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
                extra: payload,
            };
            match fire_hook_full(pl, hook, &hin, &self.cfg) {
                Ok(out) => {
                    self.clear_streak(&pl.manifest.name);
                    if let Some(rep) = out.directive.and_then(|d| d.replacement) {
                        if !rep.is_empty() {
                            return rep;
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
