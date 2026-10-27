//! Extension manager: owns loaded plugins, fires hooks in manifest order,
//! concatenates injected contexts. Fail-open per plugin.
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
                let canonical_bundled = Self::is_canonical_bundled(ext_dir, p);
                if self.approved_here(
                    ext_dir,
                    p,
                    &pl.manifest.name,
                    &pl.manifest.version,
                    pl.manifest.enabled,
                    pl.manifest.enabled_set,
                ) {
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
        if !self.approved_here(
            ext_dir,
            dir,
            &pl.manifest.name,
            &pl.manifest.version,
            pl.manifest.enabled,
            pl.manifest.enabled_set,
        ) {
            if Self::is_canonical_bundled(ext_dir, dir) {
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
        if self.plugins.iter().any(|q| q.manifest.name == name) {
            return Err(merr(
                "EXT_NAME_CONFLICT",
                format!(
                    "extension '{name}' at {} is ignored; an extension named '{name}' is already loaded (rename one of them)",
                    dir.display()
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
    fn approved_here(
        &self,
        ext_dir: &Path,
        plugin_dir: &Path,
        name: &str,
        version: &str,
        manifest_enabled: bool,
        manifest_enabled_set: bool,
    ) -> bool {
        if Self::is_canonical_bundled(ext_dir, plugin_dir) {
            // Bundled (first-party) plugins skip the third-party approval
            // store. Enablement: the config file (`[plugins.<name>]`) is
            // the single enablement state, shared with the dashboard, the
            // mobile app, and the agent - and it wins when present. When
            // absent, the bundled manifest's own `enabled` flag is the
            // default (true only for plugins that ship on, like
            // noisegate and planning-with-files). An unparsable config
            // fails closed (disabled); a missing config is an empty
            // config, so manifest defaults apply (see
            // [`crate::bundled::load_config`]).
            let data_dir = self
                .data_dir
                .clone()
                .unwrap_or_else(|| ext_dir.parent().unwrap_or(ext_dir).to_path_buf());
            return crate::bundled::load_config(&data_dir)
                .map(|c| crate::bundled::is_enabled_with_default(&c, name, manifest_enabled))
                .unwrap_or(false);
        }
        if manifest_enabled_set && !manifest_enabled {
            return false;
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
    /// sees it. Operators who need both a noise filter and a secret
    /// redactor must order them so the redactor wins, or run one as a
    /// separate pass - chaining is a deliberate non-goal here because a
    /// chained transform would let an early plugin rewrite text a later
    /// security plugin was supposed to see verbatim.
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
        assert!(mgr.names().is_empty(), "explicit enabled:false must not fire");
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
        assert_eq!(mgr.fire(Hook::PreLlmCall, "s1", "test", extra.clone()), None);
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
                mgr.fire_transform(Hook::TransformToolResult, "s", "test", extra.clone(), "payload"),
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
