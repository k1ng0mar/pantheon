//! Plugin management: list, approve, and disable tool and hook plugins.
//!
//! Tool plugins live under `<data_dir>/plugins` (`manifest.yaml`);
//! hook plugins under `<data_dir>/extensions` (`plugin.yaml`). Third-party
//! plugins of both kinds gate on the unified approval store
//! (`pantheon_api::approval`): a third-party plugin that is not approved
//! never verifies/loads, no matter what its manifest says.
//!
//! Bundled (first-party) plugins are different: they skip the approval
//! store, and their enablement lives in the config file
//! (`[plugins.<name>]`) - the single enablement state shared with the
//! mobile app (which calls these same endpoints), the TUI, and the
//! agent's `enable_plugin` tool. The config entry wins when present;
//! when absent, the bundled manifest's own `enabled` flag is the default
//! (on only for noisegate, off for the rest), exactly as the runtime,
//! TUI, and CLI resolve it. For a third-party plugin the approval store
//! remains the gate and the manifest flag is a per-install switch. `bundled: true` in the list
//! output tells the UI which semantics apply.
//!
//! - `GET /api/plugins` - every discovered plugin plus every bundled
//!   catalog entry, with kind, version, approval status, the config-
//!   derived `enabled` flag for bundled plugins, and privilege notes.
//! - `POST /api/plugins/import` ← `{url, ref?}` - fetch a plugin from a
//!   GitHub repo URL (or `clawhub:<slug>`, which 422s with a pointer to
//!   the skills importer - ClawHub is skills-only), detect its format
//!   (native tool/hook, Claude Code conversion; unknown layouts 422),
//!   run the heuristic static scanner, and quarantine it under
//!   `<data_dir>/plugins/.quarantine/<name>/` - unapproved by default,
//!   never on a load path. 201 with the scan report.
//! - `GET /api/plugins/registry/search?q=&source=clawhub` - search the
//!   ClawHub skill registry for the import picker UI.
//! - `POST /api/plugins/:kind/:name/approve` - third-party: record
//!   operator approval (informed consent; the UI shows the privilege
//!   warning first). Bundled: write `enabled = true` to the config
//!   the operator's explicit enable is the consent record for
//!   first-party code. Quarantined imports additionally gate on the
//!   scan verdict: `malicious` is never approvable, `suspicious`
//!   requires `{"acknowledge_risk": true}`; approval promotes the tree
//!   into the live dir.
//! - `POST /api/plugins/:kind/:name/disable` - third-party: revoke the
//!   approval (the plugin stays installed but will not verify/load);
//!   tool plugins additionally get `enabled = false` written to their
//!   manifest. Bundled: write `enabled = false` to the config.

use crate::plugin_import;
use crate::{bad_json, body_json, created_json, err_json, json_ok, App};
use pantheon_exec::plugin_approval;
use pantheon_exec::plugins::{self, DiscoveredPlugin};
use pantheon_extensions::bundled;
use pantheon_gateway::http::{Request, Response};

use crate::util::project_root;

/// Reject path-unsafe names before they reach any lookup. Plugins are
/// found by parsed manifest name (never by joining this segment), but a
/// hostile name still has no business in the route.
fn safe_name(name: &str) -> bool {
    !(name.is_empty() || name.contains('/') || name.contains('\\') || name.contains(".."))
}

/// Config-derived enablement for a bundled plugin: the `[plugins.<name>]`
/// config entry wins when present; when absent, the bundled manifest's
/// own `enabled` flag is the default (true only for plugins that ship
/// on, like noisegate). A missing or unparsable config fails closed
/// (disabled). This is the same resolution the runtime, TUI, and CLI
/// use, so every surface reports the same state.
fn bundled_enabled(app: &App, name: &str, manifest_default: bool) -> bool {
    bundled::load_config(&app.data_dir)
        .map(|c| bundled::is_enabled_with_default(&c, name, manifest_default))
        .unwrap_or(false)
}

/// List JSON for a bundled catalog entry. `approved` is always true:
/// bundled plugins are first-party and skip the third-party approval
/// store; their gate is the config `enabled` flag.
fn bundled_json(p: &bundled::BundledPlugin, enabled: bool) -> serde_json::Value {
    serde_json::json!({
        "kind": p.kind.as_str(),
        "name": p.name,
        "version": p.version,
        "description": p.description,
        "privilege_notes": p.privilege_notes,
        "enabled": enabled,
        "bundled": true,
        "approved": true,
        "location": "Bundled",
    })
}

/// `GET /api/plugins`
pub fn list(app: &App, _req: &Request) -> Response {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for p in plugins::discover_plugins(&app.data_dir, &project_root()) {
        let is_bundled = plugin_approval::is_bundled(&p);
        // Bundled: the config file is the single enablement state and
        // wins over the manifest flag. Third-party: the manifest flag,
        // gated by the approval store at load time.
        let enabled = if is_bundled {
            bundled_enabled(app, &p.manifest.name, p.manifest.enabled)
        } else {
            p.manifest.enabled
        };
        seen.insert(("tool".to_string(), p.manifest.name.clone()));
        out.push(serde_json::json!({
            "kind": "tool",
            "name": p.manifest.name,
            "version": p.manifest.version,
            "description": p.manifest.description,
            "enabled": enabled,
            "bundled": is_bundled,
            "approved": plugin_approval::is_approved(&p),
            "location": format!("{:?}", p.location),
        }));
    }
    let ext_dir = app.data_dir.join("extensions");
    let pending: std::collections::HashSet<String> =
        pantheon_extensions::pending_approvals(&ext_dir)
            .into_iter()
            .map(|p| p.name)
            .collect();
    // Hook plugins live as immediate subdirs of the extensions dir, and
    // bundled ones under `<ext_dir>/bundled/` (the layout
    // `approval::is_bundled` recognizes and `ExtensionManager::load_dir`
    // scans). Both layers are listed; the catalog fill below covers
    // entries with no files on disk yet.
    let mut hook_dirs: Vec<std::path::PathBuf> = Vec::new();
    for base in [&ext_dir, &ext_dir.join("bundled")] {
        if let Ok(entries) = std::fs::read_dir(base) {
            hook_dirs.extend(
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir() && p.join("plugin.yaml").exists()),
            );
        }
    }
    hook_dirs.sort();
    {
        let mut hooks: Vec<serde_json::Value> = hook_dirs
            .iter()
            .filter_map(|dir| {
                let m =
                    pantheon_extensions::manifest::PluginManifest::load(&dir.join("plugin.yaml"))
                        .ok()?;
                let is_bundled = pantheon_api::approval::is_bundled(&ext_dir, dir);
                seen.insert(("hook".to_string(), m.name.clone()));
                // Bundled hook plugins skip the approval store; their gate
                // is the config `enabled` flag.
                let (enabled, approved) = if is_bundled {
                    (bundled_enabled(app, &m.name, m.enabled), true)
                } else {
                    let approved = !pending.contains(&m.name);
                    (approved, approved)
                };
                Some(serde_json::json!({
                    "kind": "hook",
                    "name": m.name,
                    "version": m.version,
                    "description": m.description,
                    "enabled": enabled,
                    "bundled": is_bundled,
                    "approved": approved,
                    "location": if is_bundled { "Bundled".to_string() } else { "User".to_string() },
                }))
            })
            .collect();
        hooks.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out.extend(hooks);
    }
    // Quarantined imports (POST /api/plugins/import): staged, scanned,
    // unapproved, and never on a load path. Listed so the operator can
    // review the scan report and approve (promoting them live) or leave
    // them. They are not part of `seen` for the live layers above, but
    // they do suppress the bundled-catalog fill below so one entry does
    // not appear twice.
    if let Ok(entries) = std::fs::read_dir(plugin_import::quarantine_dir(&app.data_dir)) {
        let mut quarantined: Vec<serde_json::Value> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter_map(|dir| {
                let name = dir.file_name()?.to_string_lossy().to_string();
                let meta = plugin_import::ImportMeta::load_from(&dir)?;
                let scan = plugin_import::read_scan_report(&app.data_dir, &name);
                let verdict = scan
                    .as_ref()
                    .map(|s| s.verdict.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                seen.insert((meta.kind.clone(), name.clone()));
                Some(serde_json::json!({
                    "kind": meta.kind,
                    "name": name,
                    "version": meta.version,
                    "description": meta.description,
                    "enabled": false,
                    "bundled": false,
                    "approved": false,
                    "quarantined": true,
                    "location": format!("Quarantine ({})", meta.source_ref),
                    "scan_verdict": verdict,
                    "scan_report": scan.map(|s| s.to_json()),
                }))
            })
            .collect();
        quarantined.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out.extend(quarantined);
    }
    // Bundled catalog entries that are not installed on disk yet. The
    // catalog is the source of shippable plugins; the dashboard lists
    // them so the operator can enable one before (or without) it being
    // installed. Enablement is the config flag either way. (The runtime
    // and TUI seed `<ext_dir>/bundled/<name>/` from the vendored source
    // on load, so a catalog entry enabled here will have files to load.)
    let mut catalog: Vec<serde_json::Value> = bundled::bundled_plugins()
        .iter()
        .filter(|p| !seen.contains(&(p.kind.as_str().to_string(), p.name.clone())))
        .map(|p| bundled_json(p, bundled_enabled(app, &p.name, p.enabled)))
        .collect();
    catalog.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out.extend(catalog);
    json_ok(serde_json::json!({"plugins": out}))
}

fn find_tool_plugin(app: &App, name: &str) -> Option<DiscoveredPlugin> {
    plugins::discover_plugins(&app.data_dir, &project_root())
        .into_iter()
        .find(|p| p.manifest.name == name)
}

/// Enable a bundled plugin through the config file - the single
/// enablement state. Bundled plugins are first-party, so there is no
/// third-party approval record to write; the operator's explicit enable
/// is the consent record.
fn enable_bundled(app: &App, kind: &str, name: &str, enabled: bool) -> Option<Response> {
    let p = bundled::find(name)?;
    if p.kind.as_str() != kind {
        return Some(bad_json("kind mismatch for bundled plugin"));
    }
    match bundled::set_enabled(&app.data_dir, name, enabled) {
        Ok(()) => Some(json_ok(serde_json::json!({
            "ok": true, "kind": kind, "name": name,
            "enabled": enabled, "bundled": true,
        }))),
        Err(e) => Some(err_json(500, "PLUGIN_ENABLE", &e.to_string())),
    }
}

/// `POST /api/plugins/:kind/:name/approve` ← optional `{"acknowledge_risk": true}`.
///
/// Quarantine gate (scan/quarantine design): a quarantined import carries
/// a persisted scan verdict
/// - `malicious` → 409 SCAN_BLOCKED, always;
/// - `suspicious` → 409 RISK_ACK_REQUIRED unless the body carries
///   `{"acknowledge_risk": true}`;
/// - `clean` (or no report) → the normal flow.
/// Approving a quarantined import promotes its tree into the live dir
/// first; the scan report and import metadata travel with it as an audit
/// trail. Non-quarantined plugins keep the existing behavior exactly.
pub fn approve(app: &App, kind: &str, name: &str, req: &Request) -> Response {
    if !safe_name(name) {
        return bad_json("unsafe plugin name");
    }
    if let Some(r) = enable_bundled(app, kind, name, true) {
        return r;
    }
    if plugin_import::is_quarantined(&app.data_dir, name) {
        let meta = match plugin_import::ImportMeta::load(&app.data_dir, name) {
            Some(m) => m,
            None => {
                return err_json(
                    500,
                    "PLUGIN_APPROVE",
                    "quarantined plugin has no import metadata",
                )
            }
        };
        if meta.kind != kind {
            return bad_json("kind mismatch for quarantined plugin");
        }
        match plugin_import::read_scan_verdict(&app.data_dir, name).as_deref() {
            Some("malicious") => {
                return err_json(
                    409,
                    "SCAN_BLOCKED",
                    "the static scan flagged this plugin as malicious; approval is blocked",
                )
            }
            Some("suspicious") => {
                let ack = body_json(req)
                    .ok()
                    .and_then(|b| b.get("acknowledge_risk").and_then(|v| v.as_bool()))
                    .unwrap_or(false);
                if !ack {
                    return err_json(
                        409,
                        "RISK_ACK_REQUIRED",
                        "the static scan flagged this plugin as suspicious; re-submit with {\"acknowledge_risk\": true} to approve anyway",
                    );
                }
            }
            _ => {}
        }
        if let Err(e) = plugin_import::promote_from_quarantine(&app.data_dir, &meta) {
            return err_json(500, "PLUGIN_APPROVE", &e);
        }
    }
    match kind {
        "tool" => match find_tool_plugin(app, name) {
            Some(p) => match plugin_approval::record_approval(&p) {
                Ok(rec) => json_ok(serde_json::json!({
                    "ok": true, "kind": "tool", "name": name,
                    "version": rec.version, "approved": true,
                })),
                Err(e) => err_json(500, "PLUGIN_APPROVE", &e.to_string()),
            },
            None => err_json(404, "NOT_FOUND", "no tool plugin by that name"),
        },
        "hook" => {
            let ext_dir = app.data_dir.join("extensions");
            match pantheon_extensions::record_approval(&ext_dir, name) {
                Ok(rec) => json_ok(serde_json::json!({
                    "ok": true, "kind": "hook", "name": name,
                    "version": rec.version, "approved": true,
                })),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("APPROVAL_NOT_PENDING") {
                        err_json(404, "NOT_FOUND", &msg)
                    } else {
                        err_json(500, "PLUGIN_APPROVE", &msg)
                    }
                }
            }
        }
        _ => bad_json("unknown plugin kind (want \"tool\" or \"hook\")"),
    }
}

/// `POST /api/plugins/:kind/:name/disable`
pub fn disable(app: &App, kind: &str, name: &str) -> Response {
    if !safe_name(name) {
        return bad_json("unsafe plugin name");
    }
    if let Some(r) = enable_bundled(app, kind, name, false) {
        return r;
    }
    match kind {
        "tool" => match find_tool_plugin(app, name) {
            Some(p) => {
                // Revoke first: the approval gate is what keeps an
                // unapproved plugin from verifying, even if the manifest
                // still says enabled.
                if let Err(e) = plugin_approval::revoke_approval(&p) {
                    return err_json(500, "PLUGIN_DISABLE", &e.to_string());
                }
                if let Err(e) = plugins::set_enabled(&p, false) {
                    return err_json(500, "PLUGIN_DISABLE", &e.to_string());
                }
                json_ok(serde_json::json!({
                    "ok": true, "kind": "tool", "name": name, "enabled": false,
                }))
            }
            None => err_json(404, "NOT_FOUND", "no tool plugin by that name"),
        },
        "hook" => {
            let ext_dir = app.data_dir.join("extensions");
            match pantheon_extensions::revoke_approval(&ext_dir, name) {
                Ok(revoked) => {
                    if !revoked {
                        return err_json(404, "NOT_FOUND", "no hook plugin approval by that name");
                    }
                    json_ok(serde_json::json!({
                        "ok": true, "kind": "hook", "name": name, "approved": false,
                    }))
                }
                Err(e) => err_json(500, "PLUGIN_DISABLE", &e.to_string()),
            }
        }
        _ => bad_json("unknown plugin kind (want \"tool\" or \"hook\")"),
    }
}

/// The production fetch: ureq, 30s timeout, 25 MiB cap, proxy from env
/// disabled (SSRF guard lives in `plugin_import::parse_source_spec`).
fn prod_fetch(url: &str) -> Result<Vec<u8>, plugin_import::FetchError> {
    plugin_import::fetch_url(
        url,
        plugin_import::FETCH_TIMEOUT,
        plugin_import::MAX_DOWNLOAD_BYTES,
    )
}

/// 502 with the standard error envelope, for upstream fetch failures.
fn bad_gateway(code: &str, msg: &str) -> Response {
    let body = serde_json::json!({"ok": false, "error": {"code": code, "message": msg}});
    Response::Buffered {
        status: 502,
        content_type: "application/json",
        body: serde_json::to_string(&body)
            .unwrap_or_else(|_| "{}".into())
            .into_bytes(),
        extra_headers: Vec::new(),
    }
}

/// 422 with structured extras (top-level tarball entries, skill pointer).
fn unprocessable(code: &str, v: serde_json::Value) -> Response {
    let mut body = serde_json::json!({"ok": false, "error": {"code": code}});
    if let (Some(map), serde_json::Value::Object(extra)) =
        (body.get_mut("error").and_then(|e| e.as_object_mut()), v)
    {
        map.extend(extra);
    }
    Response::Buffered {
        status: 422,
        content_type: "application/json",
        body: serde_json::to_string(&body)
            .unwrap_or_else(|_| "{}".into())
            .into_bytes(),
        extra_headers: Vec::new(),
    }
}

fn import_error(e: plugin_import::ImportError) -> Response {
    use plugin_import::ImportError as E;
    match e {
        E::BadRequest(m) => bad_json(&m),
        E::NotFound(m) => err_json(404, "PLUGIN_IMPORT_NOT_FOUND", &m),
        E::FetchFailed(m) => bad_gateway("PLUGIN_IMPORT_FETCH", &m),
        E::TooLarge(m) => err_json(413, "PLUGIN_IMPORT_TOO_LARGE", &m),
        E::UnsupportedLayout { message, found } => unprocessable(
            "UNSUPPORTED_LAYOUT",
            serde_json::json!({"message": message, "found": found}),
        ),
        E::NotAPlugin {
            message,
            skill,
            hint,
        } => unprocessable(
            "NOT_A_PLUGIN",
            serde_json::json!({"message": message, "skill": skill, "hint": hint}),
        ),
        E::Conflict(m) => err_json(409, "PLUGIN_IMPORT_EXISTS", &m),
        E::Io(m) => err_json(500, "PLUGIN_IMPORT_IO", &m),
    }
}

/// `POST /api/plugins/import` ← `{url, ref?}`.
///
/// `url` is a GitHub repo URL (optional `ref`: branch/tag/commit,
/// defaults to `main` with one `master` retry) or `clawhub:<slug>`.
/// The bundle is downloaded, format-detected, statically scanned, and
/// quarantined - never installed live, never approved by default.
/// 201 with the import report (including `scan_report`).
pub fn import(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let url = body
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .trim();
    if url.is_empty() {
        return bad_json("url is required");
    }
    let gitref = body.get("ref").and_then(|r| r.as_str());
    match plugin_import::run_import(&app.data_dir, url, gitref, &prod_fetch) {
        Ok(rep) => created_json(rep.to_json()),
        Err(e) => import_error(e),
    }
}

/// `GET /api/plugins/registry/search?q=&source=clawhub`
/// → `{results: [{slug, name, description, version, kind}]}`.
///
/// The import picker's registry search. Only `clawhub` is supported
/// today; GitHub stays the generic import fallback.
pub fn registry_search(_app: &App, req: &Request) -> Response {
    let source = req
        .query
        .get("source")
        .map(|s| s.as_str())
        .unwrap_or("")
        .trim();
    if source != "clawhub" {
        return bad_json("unknown registry source (want \"clawhub\")");
    }
    let q = req.query.get("q").map(|s| s.as_str()).unwrap_or("").trim();
    if q.is_empty() {
        return bad_json("q is required");
    }
    match plugin_import::clawhub_search(&prod_fetch, q) {
        Ok(results) => json_ok(serde_json::json!({
            "results": results.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        })),
        Err(e) => import_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch `App` whose only meaningful field is `data_dir`
    /// (`bundled_enabled` reads nothing else).
    fn scratch_app(data_dir: PathBuf) -> App {
        App {
            data_dir: data_dir.clone(),
            token: "test".to_string(),
            bind: "127.0.0.1".to_string(),
            bind_all: false,
            on_approval: None,
            send_locks: Default::default(),
            turn_children: Default::default(),
            swarm: crate::swarm::orchestrator_for(&data_dir),
        }
    }

    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dashboard-plugin-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_config(dir: &std::path::Path, toml: &str) {
        std::fs::write(dir.join("config.toml"), toml).unwrap();
    }

    /// Fresh install (no `[plugins]` entries): noisegate ships on, the
    /// rest ship off - matching the runtime, TUI, and CLI resolution.
    #[test]
    fn bundled_enabled_honors_manifest_default_on_fresh_config() {
        let dir = fresh_dir("fresh");
        write_config(&dir, "# empty pantheon config\n");
        let app = scratch_app(dir.clone());
        assert!(bundled_enabled(&app, "noisegate", true));
        assert!(!bundled_enabled(&app, "security-guidance", false));
        assert!(!bundled_enabled(&app, "doc-pack", false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An explicit config entry always wins over the manifest default
    /// including an explicit `false` for a default-on plugin.
    #[test]
    fn bundled_enabled_config_entry_overrides_manifest_default() {
        let dir = fresh_dir("override");
        write_config(
            &dir,
            "[plugins.noisegate]\nenabled = false\n\
             [plugins.security-guidance]\nenabled = true\n",
        );
        let app = scratch_app(dir.clone());
        assert!(!bundled_enabled(&app, "noisegate", true));
        assert!(bundled_enabled(&app, "security-guidance", false));
        // Untouched plugins still follow their manifest default.
        assert!(!bundled_enabled(&app, "doc-pack", false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing config file fails closed (disabled), even for a
    /// default-on plugin - a missing config must never flip anything on.
    #[test]
    fn bundled_enabled_missing_config_fails_closed() {
        let dir = fresh_dir("missing");
        let app = scratch_app(dir.clone());
        assert!(!bundled_enabled(&app, "noisegate", true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
