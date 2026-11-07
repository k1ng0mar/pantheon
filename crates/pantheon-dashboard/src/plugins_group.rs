//! plugins_group group: plugins, plugin_import, mcp in one file.

pub mod plugins {
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
    /// on, like noisegate). A missing config counts as empty, so manifest
    /// defaults apply on a fresh install; an unparsable config fails closed
    /// (disabled). This is the config-defaults layer only, not the approval
    /// gate: third-party plugins still need a live approval regardless of
    /// any flag. This is the same resolution the runtime, TUI, and CLI
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
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for p in plugins::discover_plugins(&app.data_dir, &project_root()) {
            let is_bundled = plugin_approval::is_bundled(&p);
            // Bundled: the config file is the single enablement state and
            // wins over the manifest flag. Third-party: the manifest flag
            // AND approval - an unapproved plugin must not render as
            // enabled merely because its manifest says enabled:true, since
            // spawn will refuse it until approved. The separate `approved`
            // field below still distinguishes the two reasons.
            let approved = plugin_approval::is_approved(&p);
            let enabled = if is_bundled {
                bundled_enabled(app, &p.manifest.name, p.manifest.enabled)
            } else {
                p.manifest.enabled && approved
            };
            seen.insert(("tool".to_string(), p.manifest.name.clone()));
            out.push(serde_json::json!({
                "kind": "tool",
                "name": p.manifest.name,
                "version": p.manifest.version,
                "description": p.manifest.description,
                "enabled": enabled,
                "bundled": is_bundled,
                "approved": approved,
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
    ///
    /// Two catalogs, checked in order: the vendored catalog in
    /// `pantheon_extensions::bundled` (currently empty) and the embedded
    /// bundled-plugin registry in `pantheon_exec::plugins` (the real
    /// bundled plugins: noisegate, hookify, doc-pack, ...). Checking only
    /// the vendored catalog meant dashboard approve/disable of a real
    /// bundled plugin fell through to the third-party paths - tool approve
    /// returned ok:true from the approval store without ever writing
    /// `[plugins.<name>] enabled = true`, and hook approve/disable 404'd
    /// with APPROVAL_NOT_PENDING because bundled plugins have no pending
    /// third-party approval.
    fn enable_bundled(app: &App, kind: &str, name: &str, enabled: bool) -> Option<Response> {
        if let Some(p) = bundled::find(name) {
            if p.kind.as_str() != kind {
                return Some(bad_json("kind mismatch for bundled plugin"));
            }
            return match bundled::set_enabled(&app.data_dir, name, enabled) {
                Ok(()) => Some(json_ok(serde_json::json!({
                    "ok": true, "kind": kind, "name": name,
                    "enabled": enabled, "bundled": true,
                }))),
                Err(e) => Some(err_json(500, "PLUGIN_ENABLE", &e.to_string())),
            };
        }
        // Embedded registry: kind must match, then the single
        // `[plugins.<name>]` config write.
        if let Some(info) = plugins::bundled_plugin_infos()
            .into_iter()
            .find(|p| p.name == name)
        {
            if info.kind.as_str() != kind {
                return Some(bad_json("kind mismatch for bundled plugin"));
            }
            return match plugins::set_bundled_enabled(&app.data_dir, name, enabled) {
                Ok(()) => Some(json_ok(serde_json::json!({
                    "ok": true, "kind": kind, "name": name,
                    "enabled": enabled, "bundled": true,
                }))),
                Err(e) => Some(err_json(500, "PLUGIN_ENABLE", &e.to_string())),
            };
        }
        None
    }

    /// `POST /api/plugins/:kind/:name/approve` ← optional `{"acknowledge_risk": true}`.
    ///
    /// Quarantine gate (scan/quarantine design): a quarantined import carries
    /// a persisted scan verdict
    /// - `malicious` → 409 SCAN_BLOCKED, always;
    /// - `suspicious` → 409 RISK_ACK_REQUIRED unless the body carries
    ///   `{"acknowledge_risk": true}`;
    /// - `clean` (or no report) leads to the normal flow.
    ///
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
                Some(p) => {
                    // Order matters: approval is bound to the directory
                    // hash, and flipping the manifest rewrites it. Flip
                    // first, then record approval for the post-flip bytes -
                    // recording first used to leave the list showing
                    // approved:false right after a successful approve.
                    if let Err(e) = plugins::set_enabled(&p, true) {
                        return err_json(500, "PLUGIN_APPROVE", &e.to_string());
                    }
                    match plugin_approval::record_approval(&p) {
                        Ok(rec) => {
                            // Approval alone does not enable: the runtime skips
                            // a plugin whose manifest flag is false before
                            // `spawn_verified`, so an approved-but-disabled plugin
                            // used to stay unloaded while the list showed
                            // `approved: true, enabled: false` with no enable
                            // action. Approving here also flips the manifest on
                            // (done above, before recording, so the approval
                            // hash covers the enabled manifest).
                            json_ok(serde_json::json!({
                                "ok": true, "kind": "tool", "name": name,
                                "version": rec.version, "approved": true,
                            }))
                        }
                        Err(e) => err_json(500, "PLUGIN_APPROVE", &e.to_string()),
                    }
                }
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
                            return err_json(
                                404,
                                "NOT_FOUND",
                                "no hook plugin approval by that name",
                            );
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
                ui_action_locks: Default::default(),
                turn_children: Default::default(),
                swarm: crate::swarm::orchestrator_for(&data_dir),
                desktop: Default::default(),
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

        /// A missing config file counts as EMPTY, so shipped manifest
        /// defaults apply on a fresh install: a default-on plugin is on,
        /// a default-off one is off. This is the config-defaults layer
        /// only - it does not relax the approval gate, which is separate:
        /// a third-party plugin still needs a live approval (its
        /// `enabled` rendering is `manifest.enabled && approved` in
        /// `list`, never the flag alone), so "missing config applies
        /// defaults" can never approve unapproved code.
        #[test]
        fn bundled_enabled_missing_config_uses_manifest_default() {
            let dir = fresh_dir("missing");
            let app = scratch_app(dir.clone());
            assert!(bundled_enabled(&app, "noisegate", true));
            assert!(!bundled_enabled(&app, "security-guidance", false));
            let _ = std::fs::remove_dir_all(&dir);
        }

        fn post_req() -> Request {
            Request {
                method: "POST".to_string(),
                path: "/api/plugins".to_string(),
                query: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                body: b"{}".to_vec(),
            }
        }

        fn resp_json(resp: Response) -> (u16, serde_json::Value) {
            match resp {
                Response::Buffered { status, body, .. } => {
                    (status, serde_json::from_slice(&body).unwrap_or_default())
                }
                _ => panic!("expected a buffered response"),
            }
        }

        fn config_plugin_enabled(dir: &std::path::Path, name: &str) -> Option<bool> {
            let text = std::fs::read_to_string(dir.join("config.toml")).ok()?;
            let doc: toml::Value = toml::from_str(&text).ok()?;
            doc.get("plugins")?.get(name)?.get("enabled")?.as_bool()
        }

        /// Cluster 1 (e): dashboard approve/disable of a real (embedded
        /// registry) bundled plugin must write the single enablement
        /// state, `[plugins.<name>] enabled`. Before the fix
        /// `enable_bundled` consulted only the empty vendored catalog, so
        /// hook approve/disable fell through to the third-party paths and
        /// 404'd with APPROVAL_NOT_PENDING, and tool approve returned
        /// ok:true from the approval store without touching the config.
        #[test]
        fn dashboard_approve_disable_writes_bundled_config_flag() {
            let dir = fresh_dir("bundled-approve");
            write_config(&dir, "# empty pantheon config\n");
            let app = scratch_app(dir.clone());
            // noisegate is an embedded hook plugin; it is not in the
            // vendored catalog, so this exercises the embedded path.
            let (status, body) = resp_json(approve(&app, "hook", "noisegate", &post_req()));
            assert_eq!(status, 200, "approve must succeed: {body}");
            assert_eq!(body["enabled"], serde_json::json!(true));
            assert_eq!(
                config_plugin_enabled(&dir, "noisegate"),
                Some(true),
                "approve must write [plugins.noisegate] enabled = true"
            );
            let (status, body) = resp_json(disable(&app, "hook", "noisegate"));
            assert_eq!(status, 200, "disable must succeed: {body}");
            assert_eq!(
                config_plugin_enabled(&dir, "noisegate"),
                Some(false),
                "disable must write [plugins.noisegate] enabled = false"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod plugin_import {
    //! Plugin import pipeline: source validation, tarball fetch, format
    //! detection, static security scan, and quarantine staging.
    //!
    //! Sources: `https://github.com/<owner>/<repo>` (optional `#<branch|tag>`
    //! fragment or `ref` field) and `clawhub:<slug>`.
    //!
    //! Honest design notes - read before extending:
    //!
    //! - The static scanner below is **heuristic pattern matching**, not a
    //!   sandbox and not a guarantee. It catches low-effort malicious
    //!   patterns (miners, `curl|sh`, credential grabs) and flags suspicious
    //!   ones for a human to review. Defense in depth, in order: (1) the
    //!   quarantine dir is never on any plugin load path, (2) the scan gates
    //!   approval, (3) the existing approval store still gates loading.
    //!   A clever attacker can evade static heuristics; treat `clean` as
    //!   "nothing obviously bad found", not "safe".
    //! - ClawHub has a real packages API (verified live 2026-09-30):
    //!   `GET /api/v1/packages/{name}` returns `{"package": {family,
    //!   latestVersion, ...}}` and
    //!   `GET /api/v1/packages/{name}/download?version={v}` returns the
    //!   package zip. So `clawhub:<slug>` (either `<name>` or
    //!   `<owner>/<name>`) resolves via the packages API and downloads the
    //!   zip through the same quarantine → scan → format-detect pipeline as
    //!   GitHub tarballs. A package whose `family` is `skill` still ends in
    //!   422 NOT_A_PLUGIN pointing at the skills importer, and a skill slug
    //!   that 404s on the packages endpoint is checked against the skills
    //!   endpoint before giving up, so the hint survives.
    //! - GitHub tarballs come from
    //!   `codeload.github.com/<owner>/<repo>/tar.gz/<ref>`. A repo's default
    //!   branch is unknowable without the GitHub API, so `ref` defaults to
    //!   `main` and we retry `master` once on 404. Pass an explicit `ref`
    //!   (or `#fragment`) for anything else.
    //! - codeload.github.com (Fastly) closes the TLS session without
    //!   `close_notify`; rustls reports that as a transport error even when
    //!   every byte already arrived. Body reads go through the shared
    //!   `pantheon_exec::http` helper, which accepts a body the framing says
    //!   is whole (declared length fully received, or close-delimited with
    //!   bytes received) and keeps failing hard on genuine truncation;
    //!   unclean-shutdown errors are rewritten in plain English.
    //! - Tar entries are parsed by hand (no `tar` crate) with flate2 for
    //!   gzip; entries with `..`, absolute paths, or symlinks/hardlinks are
    //!   skipped and counted, never materialized. Zip entries (ClawHub
    //!   packages) are parsed by hand the same way - central directory,
    //!   stored/deflated via flate2 - with the same safety rules.
    //! - Imported plugins land UNAPPROVED in
    //!   `<data_dir>/plugins/.quarantine/<name>/` with `.scan-report.json`
    //!   and `.import-meta.json` beside them. Approval (via the existing
    //!   `plugins::approve` gate, extended with the scan verdict) promotes
    //!   the tree into the live dir and only then records approval.

    use std::collections::HashMap;
    use std::io::Read;
    use std::path::{Component, Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Cap on downloaded bundle bytes (task spec).
    pub const MAX_DOWNLOAD_BYTES: u64 = 25 * 1024 * 1024;
    /// Cap on decompressed tar bytes (zip-bomb guard).
    pub const MAX_DECOMPRESSED_BYTES: u64 = 100 * 1024 * 1024;
    /// Files bigger than this are skipped by the scanner.
    pub const MAX_SCANNED_FILE_BYTES: u64 = 2 * 1024 * 1024;
    /// Network timeout for bundle fetches (task spec: 30s).
    pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
    /// ClawHub public API base.
    pub const CLAWHUB_API: &str = "https://clawhub.ai/api/v1";

    // ---------------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------------

    /// Pipeline failure. The dashboard maps these to HTTP responses; the
    /// `code()` values are the stable `error.code` strings clients match on.
    #[derive(Debug)]
    pub enum ImportError {
        BadRequest(String),
        NotFound(String),
        Conflict(String),
        TooLarge(String),
        FetchFailed(String),
        UnsupportedLayout {
            message: String,
            found: Vec<String>,
        },
        NotAPlugin {
            message: String,
            skill: serde_json::Value,
            hint: String,
        },
        Io(String),
    }

    impl ImportError {
        pub fn code(&self) -> &'static str {
            match self {
                ImportError::BadRequest(_) => "BAD_REQUEST",
                ImportError::NotFound(_) => "NOT_FOUND",
                ImportError::Conflict(_) => "PLUGIN_IMPORT_EXISTS",
                ImportError::TooLarge(_) => "PLUGIN_IMPORT_TOO_LARGE",
                ImportError::FetchFailed(_) => "PLUGIN_IMPORT_FETCH",
                ImportError::UnsupportedLayout { .. } => "UNSUPPORTED_LAYOUT",
                ImportError::NotAPlugin { .. } => "NOT_A_PLUGIN",
                ImportError::Io(_) => "PLUGIN_IMPORT_IO",
            }
        }

        pub fn message(&self) -> String {
            match self {
                ImportError::BadRequest(m)
                | ImportError::NotFound(m)
                | ImportError::Conflict(m)
                | ImportError::TooLarge(m)
                | ImportError::FetchFailed(m)
                | ImportError::Io(m) => m.clone(),
                ImportError::UnsupportedLayout { message, .. } => message.clone(),
                ImportError::NotAPlugin { message, .. } => message.clone(),
            }
        }
    }

    impl std::fmt::Display for ImportError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}: {}", self.code(), self.message())
        }
    }

    /// Injectable HTTP GET. Production passes [`fetch_url`]; tests pass a stub
    /// so no test ever touches the network.
    #[derive(Debug)]
    pub enum FetchError {
        Status(u16, String),
        Transport(String),
        TooLarge,
    }

    pub type FetchFn = dyn Fn(&str) -> Result<Vec<u8>, FetchError>;

    // ---------------------------------------------------------------------------
    // Source specs
    // ---------------------------------------------------------------------------

    /// Where the bundle comes from.
    #[derive(Debug, Clone)]
    pub enum SourceSpec {
        Github {
            owner: String,
            repo: String,
            gitref: String,
        },
        ClawHub {
            slug: String,
        },
    }

    /// Parse the `url` field of the import request: either
    /// `https://github.com/<owner>/<repo>[#<ref>]` or `clawhub:<slug>`.
    /// `ref_field` is the optional `ref` JSON field and wins over the URL fragment.
    pub fn parse_source_spec(
        raw: &str,
        ref_field: Option<&str>,
    ) -> Result<SourceSpec, ImportError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ImportError::BadRequest("url is empty".into()));
        }
        if let Some(slug) = raw.strip_prefix("clawhub:") {
            return parse_clawhub_slug(slug).map(|slug| SourceSpec::ClawHub { slug });
        }
        parse_github_url(raw, ref_field)
    }

    /// Parse a `clawhub:` slug: `<name>` or `<owner>/<name>`. Each segment
    /// must be path-safe (ascii letters/digits plus `.-_`, no leading dot or
    /// dash, not `.`/`..`); the packages API is addressed by the final
    /// segment.
    fn parse_clawhub_slug(slug: &str) -> Result<String, ImportError> {
        let bad = || {
            ImportError::BadRequest(
                "bad clawhub slug (want clawhub:<name> or clawhub:<owner>/<name>, ascii letters/digits/.-_)".into(),
            )
        };
        let s = slug.trim();
        if s.is_empty() || s.len() > 128 {
            return Err(bad());
        }
        let mut parts = s.split('/');
        let mut count = 0;
        for part in &mut parts {
            count += 1;
            if count > 2
                || part.is_empty()
                || part == "."
                || part == ".."
                || part.starts_with('.')
                || part.starts_with('-')
                || !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            {
                return Err(bad());
            }
        }
        if count == 0 {
            return Err(bad());
        }
        Ok(s.to_string())
    }

    /// URL whitelist: exactly `https://github.com/<owner>/<repo>`. Everything
    /// else - private IPs, loopback, other hosts, credentials, ports, deep
    /// paths - is rejected with 400.
    fn parse_github_url(raw: &str, ref_field: Option<&str>) -> Result<SourceSpec, ImportError> {
        let bad = |m: &str| ImportError::BadRequest(m.to_string());
        let u = url::Url::parse(raw).map_err(|_| bad("url is not a valid URL"))?;
        if u.scheme() != "https" {
            return Err(bad("only https GitHub URLs are accepted"));
        }
        if !u.username().is_empty() || u.password().is_some() {
            return Err(bad("URL must not embed credentials"));
        }
        let host = u.host_str().unwrap_or("");
        // Explicit SSRF-flavored rejections with a clear message. (The exact
        // host match below already excludes these; this is the clearer error.)
        if host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok() {
            return Err(bad(
                "private IPs, loopback, and non-GitHub hosts are rejected",
            ));
        }
        if !host.eq_ignore_ascii_case("github.com") {
            return Err(bad("only github.com URLs are accepted"));
        }
        if u.port().is_some() {
            return Err(bad("URL must not specify a port"));
        }
        // `path_segments` percent-decodes; the strict charset check below then
        // also defeats %2e%2e-style tricks.
        let segs: Vec<String> = u
            .path_segments()
            .map(|it| it.filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        if segs.len() != 2 {
            return Err(bad("URL must be https://github.com/<owner>/<repo>"));
        }
        let owner = segs[0].clone();
        let mut repo = segs[1].clone();
        if let Some(stripped) = repo.strip_suffix(".git") {
            repo = stripped.to_string();
        }
        for part in [&owner, &repo] {
            if part.is_empty()
                || part.len() > 100
                || part.contains("..")
                || !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            {
                return Err(bad("invalid owner/repo in URL"));
            }
        }
        let gitref = ref_field
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| u.fragment().map(str::trim).filter(|s| !s.is_empty()))
            .unwrap_or("main")
            .to_string();
        validate_gitref(&gitref)?;
        Ok(SourceSpec::Github {
            owner,
            repo,
            gitref,
        })
    }

    fn validate_gitref(r: &str) -> Result<(), ImportError> {
        let ok = !r.is_empty()
            && r.len() <= 128
            && !r.contains("..")
            && !r.starts_with('/')
            && !r.starts_with('.')
            && r.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
        if ok {
            Ok(())
        } else {
            Err(ImportError::BadRequest(
                "bad ref (ascii letters/digits/._-/, no leading dot or slash, no ..)".into(),
            ))
        }
    }

    // ---------------------------------------------------------------------------
    // Fetch
    // ---------------------------------------------------------------------------

    /// Plain HTTPS GET with a timeout and a byte cap, via the shared
    /// [`pantheon_exec::http::get`] helper: ureq first, one curl-CLI retry
    /// on transport-level failure only. HTTP error statuses and oversize
    /// bodies never trigger the fallback.
    ///
    /// Proxy env is honored for *transport*, the same posture as curl and
    /// git. That does not weaken the source whitelist: the URL is validated
    /// against the whitelist before any fetch happens, and TLS still
    /// verifies the origin server's certificate through the proxy tunnel
    /// a proxy cannot make a non-whitelisted host present a valid cert for
    /// a whitelisted name.
    ///
    /// TLS teardown tolerance lives in the shared [`pantheon_exec::http`]
    /// helper: some servers (codeload.github.com via Fastly, the Claude
    /// marketplace) close the TLS session without `close_notify`, which
    /// rustls surfaces as a transport error even when the framing says the
    /// body is already whole. Genuine truncation still errors, and the
    /// failure message is plain English (no rustls doc link).
    pub fn fetch_url(url: &str, timeout: Duration, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
        match pantheon_exec::http::get(url, timeout, max_bytes, "pantheon-plugin-import") {
            Ok(fetched) => Ok(fetched.body),
            Err(pantheon_exec::http::GetError::Status(code)) => {
                Err(FetchError::Status(code, String::new()))
            }
            Err(pantheon_exec::http::GetError::Transport(msg)) => Err(FetchError::Transport(msg)),
            Err(pantheon_exec::http::GetError::TooLarge) => Err(FetchError::TooLarge),
        }
    }

    fn percent_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    // ---------------------------------------------------------------------------
    // Tarball extraction (hand-rolled tar over flate2 gzip)
    // ---------------------------------------------------------------------------

    pub struct ExtractStats {
        pub files: usize,
        pub skipped_unsafe: usize,
    }

    fn read_octal(field: &[u8]) -> Option<u64> {
        let s = std::str::from_utf8(field).ok()?;
        let s = s.trim_matches(|c| c == '\0' || c == ' ');
        if s.is_empty() {
            return Some(0);
        }
        u64::from_str_radix(s.trim(), 8).ok()
    }

    /// ustar name: 100-byte name field plus the 155-byte prefix field.
    fn tar_entry_name(hdr: &[u8]) -> String {
        let raw = &hdr[0..100];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let mut name = String::from_utf8_lossy(&raw[..end]).into_owned();
        let praw = &hdr[345..500];
        let pend = praw.iter().position(|&b| b == 0).unwrap_or(praw.len());
        if pend > 0 {
            let prefix = String::from_utf8_lossy(&praw[..pend]);
            name = format!("{prefix}/{name}");
        }
        name
    }

    /// Map a tar entry name to a staging-relative path. Rejects absolute
    /// paths and any `..`/`.`/empty components; strips the tarball's single
    /// top-level directory (the GitHub codeload `<repo>-<ref>/` layout).
    fn safe_stage_path(name: &str) -> Option<PathBuf> {
        if name.is_empty() {
            return None;
        }
        let p = Path::new(name);
        if p.is_absolute() {
            return None;
        }
        let mut rel = PathBuf::new();
        for comp in p.components() {
            match comp {
                Component::Normal(c) => rel.push(c),
                _ => return None,
            }
        }
        let mut it = rel.components();
        it.next()?; // strip top-level dir
        let stripped: PathBuf = it.collect();
        if stripped.as_os_str().is_empty() {
            return None;
        }
        Some(stripped)
    }

    pub fn extract_tar_gz(data: &[u8], dest: &Path) -> Result<ExtractStats, ImportError> {
        if data.len() < 2 || data[0] != 0x1f || data[1] != 0x8b {
            return Err(ImportError::BadRequest(
                "downloaded bytes are not gzip (magic mismatch)".into(),
            ));
        }
        let dec = flate2::read::GzDecoder::new(data);
        let mut tar = Vec::new();
        dec.take(MAX_DECOMPRESSED_BYTES + 1)
            .read_to_end(&mut tar)
            .map_err(|e| ImportError::BadRequest(format!("gzip decode failed: {e}")))?;
        if tar.len() as u64 > MAX_DECOMPRESSED_BYTES {
            return Err(ImportError::TooLarge(format!(
                "decompressed tarball exceeds {} MiB",
                MAX_DECOMPRESSED_BYTES / 1024 / 1024
            )));
        }
        extract_tar(&tar, dest)
    }

    fn extract_tar(tar: &[u8], dest: &Path) -> Result<ExtractStats, ImportError> {
        let mut stats = ExtractStats {
            files: 0,
            skipped_unsafe: 0,
        };
        let mut off = 0usize;
        let mut long_name: Option<String> = None;
        while off + 512 <= tar.len() {
            let hdr = &tar[off..off + 512];
            if hdr.iter().all(|&b| b == 0) {
                break; // end-of-archive marker
            }
            let size = read_octal(&hdr[124..136])
                .ok_or_else(|| ImportError::BadRequest("tar: bad size field".into()))?
                as usize;
            let typeflag = hdr[156];
            off += 512;
            let data_end = off
                .checked_add(size)
                .ok_or_else(|| ImportError::BadRequest("tar: size overflow".into()))?;
            if data_end > tar.len() {
                return Err(ImportError::BadRequest("tar: truncated entry".into()));
            }
            if typeflag == b'L' {
                // GNU long-name entry: its payload names the *next* entry.
                let raw = &tar[off..data_end];
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                long_name = String::from_utf8(raw[..end].to_vec()).ok();
                off = data_end.next_multiple_of(512);
                continue;
            }
            let name = long_name.take().unwrap_or_else(|| tar_entry_name(hdr));
            let rel = safe_stage_path(&name);
            match typeflag {
                b'0' | 0 => {
                    match rel {
                        Some(rel) => {
                            let out = dest.join(&rel);
                            // Belt and braces: the component filter above makes
                            // escape impossible, but never trust a tarball.
                            if !out.starts_with(dest) {
                                stats.skipped_unsafe += 1;
                            } else {
                                if let Some(parent) = out.parent() {
                                    std::fs::create_dir_all(parent)
                                        .map_err(|e| ImportError::Io(format!("staging: {e}")))?;
                                }
                                std::fs::write(&out, &tar[off..data_end])
                                    .map_err(|e| ImportError::Io(format!("staging: {e}")))?;
                                stats.files += 1;
                            }
                        }
                        None => stats.skipped_unsafe += 1,
                    }
                }
                b'5' => match rel {
                    Some(rel) => {
                        let out = dest.join(&rel);
                        if out.starts_with(dest) {
                            std::fs::create_dir_all(&out)
                                .map_err(|e| ImportError::Io(format!("staging: {e}")))?;
                        } else {
                            stats.skipped_unsafe += 1;
                        }
                    }
                    None => stats.skipped_unsafe += 1,
                },
                // Symlinks (b'2'), hardlinks (b'1'), and anything exotic are
                // never materialized - a symlink pointing at /etc/passwd must
                // not survive extraction. Counted, not an error.
                _ => stats.skipped_unsafe += 1,
            }
            off = data_end.next_multiple_of(512);
        }
        Ok(stats)
    }

    // ---------------------------------------------------------------------------
    // Zip extraction (hand-rolled, for ClawHub package zips)
    // ---------------------------------------------------------------------------
    //
    // Parses the End of Central Directory + Central Directory (robust against
    // data-descriptor local headers), supports stored (method 0) and deflated
    // (method 8, via flate2) entries, and applies the same safety rules as
    // the tar path: no absolute paths, no `..`/`.` components, symlinks and
    // anything exotic are skipped and counted, never materialized.
    //
    // Unlike codeload tarballs, ClawHub zips have no guaranteed top-level
    // directory, so the single top-level dir is stripped only when every
    // entry lives under one common first component.

    struct ZipEntry {
        name: String,
        method: u16,
        comp_size: u64,
        uncomp_size: u64,
        local_offset: u64,
        is_symlink: bool,
        encrypted: bool,
    }

    fn u16le(b: &[u8]) -> u16 {
        u16::from_le_bytes([b[0], b[1]])
    }
    fn u32le(b: &[u8]) -> u32 {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    /// Locate the End of Central Directory record by scanning backwards over
    /// the last 64 KiB + 22 bytes (the EOCD comment can be up to 64 KiB).
    fn find_eocd(data: &[u8]) -> Option<usize> {
        if data.len() < 22 {
            return None;
        }
        let floor = data.len().saturating_sub(65_557 + 22);
        let mut i = data.len() - 22;
        loop {
            if data[i..].starts_with(b"PK\x05\x06") {
                return Some(i);
            }
            if i == floor {
                return None;
            }
            i -= 1;
        }
    }

    fn parse_central_dir(data: &[u8]) -> Result<Vec<ZipEntry>, ImportError> {
        let bad = |m: &str| ImportError::BadRequest(format!("zip: {m}"));
        let eocd = find_eocd(data).ok_or_else(|| bad("no end-of-central-directory record"))?;
        let e = &data[eocd..];
        if e.len() < 22 {
            return Err(bad("truncated end-of-central-directory"));
        }
        let count = u16le(&e[10..12]);
        let cd_size = u32le(&e[12..16]);
        let cd_off = u32le(&e[16..20]);
        if count == 0xffff || cd_size == 0xffff_ffff || cd_off == 0xffff_ffff {
            return Err(bad("zip64 is not supported"));
        }
        let count = count as usize;
        let cd_off = cd_off as usize;
        let cd_end = cd_off
            .checked_add(cd_size as usize)
            .ok_or_else(|| bad("central directory size overflow"))?;
        if cd_end > data.len() || cd_off > data.len() {
            return Err(bad("central directory outside the file"));
        }
        let mut entries = Vec::with_capacity(count.min(100_000));
        let mut off = cd_off;
        for _ in 0..count {
            if off + 46 > data.len() || !data[off..].starts_with(b"PK\x01\x02") {
                return Err(bad("bad central directory entry"));
            }
            let h = &data[off..];
            let made_by = u16le(&h[4..6]);
            let method = u16le(&h[10..12]);
            let flags = u16le(&h[8..10]);
            let comp_size = u32le(&h[20..24]) as u64;
            let uncomp_size = u32le(&h[24..28]) as u64;
            let name_len = u16le(&h[28..30]) as usize;
            let extra_len = u16le(&h[30..32]) as usize;
            let comment_len = u16le(&h[32..34]) as usize;
            let ext_attrs = u32le(&h[38..42]);
            let local_off = u32le(&h[42..46]) as u64;
            let name_start = off + 46;
            let name_end = name_start
                .checked_add(name_len)
                .ok_or_else(|| bad("entry name overflow"))?;
            let entry_end = name_end
                .checked_add(extra_len)
                .and_then(|x| x.checked_add(comment_len))
                .ok_or_else(|| bad("entry size overflow"))?;
            if entry_end > data.len() {
                return Err(bad("entry runs past the central directory"));
            }
            let name = String::from_utf8_lossy(&data[name_start..name_end]).into_owned();
            // Unix symlink: made-by OS 3 and S_IFLNK in the mode bits.
            let is_symlink = (made_by >> 8) == 3 && ((ext_attrs >> 16) & 0o170_000) == 0o120_000;
            let encrypted = flags & 0x1 != 0;
            entries.push(ZipEntry {
                name,
                method,
                comp_size,
                uncomp_size,
                local_offset: local_off,
                is_symlink,
                encrypted,
            });
            off = entry_end;
        }
        Ok(entries)
    }

    /// Map a zip entry name to a staging-relative path. Same component rules
    /// as the tar path; top-level stripping (when decided) is applied by the
    /// caller on the already-safe path.
    fn zip_stage_path(name: &str) -> Option<PathBuf> {
        if name.is_empty() || name.starts_with('/') {
            return None;
        }
        let mut rel = PathBuf::new();
        for comp in Path::new(name).components() {
            match comp {
                Component::Normal(c) => rel.push(c),
                _ => return None,
            }
        }
        if rel.as_os_str().is_empty() {
            return None;
        }
        Some(rel)
    }

    pub fn extract_zip(data: &[u8], dest: &Path) -> Result<ExtractStats, ImportError> {
        let bad = |m: &str| ImportError::BadRequest(format!("zip: {m}"));
        if data.len() < 4 || !data.starts_with(b"PK\x03\x04") && find_eocd(data).is_none() {
            return Err(bad("not a zip file (magic mismatch)"));
        }
        let entries = parse_central_dir(data)?;
        let mut stats = ExtractStats {
            files: 0,
            skipped_unsafe: 0,
        };
        // Phase 1: keep only entries with safe relative paths.
        let mut safe: Vec<(&ZipEntry, PathBuf)> = Vec::new();
        for e in &entries {
            match zip_stage_path(&e.name) {
                Some(rel) => safe.push((e, rel)),
                None => stats.skipped_unsafe += 1,
            }
        }
        // Phase 2: strip the single top-level dir, but only when every
        // surviving *file* entry lives under one common first component
        // (the codeload-tarball shape). Unsafe entries never influence this.
        let strip_top = {
            let files: Vec<&PathBuf> = safe
                .iter()
                .filter(|(e, _)| !e.name.ends_with('/'))
                .map(|(_, rel)| rel)
                .collect();
            !files.is_empty() && files.iter().all(|rel| rel.components().count() >= 2) && {
                let first: PathBuf = files[0].components().take(1).collect();
                files
                    .iter()
                    .all(|rel| rel.components().take(1).collect::<PathBuf>() == first)
            }
        };
        let mut total_uncomp: u64 = 0;
        for (e, rel) in &safe {
            let rel = if strip_top {
                // Re-strip: safe paths were built without stripping.
                let mut it = rel.components();
                it.next();
                let stripped: PathBuf = it.collect();
                if stripped.as_os_str().is_empty() {
                    continue;
                }
                stripped
            } else {
                rel.clone()
            };
            let is_dir = e.name.ends_with('/');
            let out = dest.join(&rel);
            // Belt and braces, same as the tar path.
            if !out.starts_with(dest) {
                stats.skipped_unsafe += 1;
                continue;
            }
            if is_dir {
                std::fs::create_dir_all(&out).map_err(|e| ImportError::Io(e.to_string()))?;
                continue;
            }
            if e.is_symlink || e.encrypted || !matches!(e.method, 0 | 8) {
                stats.skipped_unsafe += 1;
                continue;
            }
            total_uncomp = total_uncomp
                .checked_add(e.uncomp_size)
                .ok_or_else(|| bad("uncompressed size overflow"))?;
            if total_uncomp > MAX_DECOMPRESSED_BYTES {
                return Err(ImportError::TooLarge(format!(
                    "decompressed zip exceeds {} MiB",
                    MAX_DECOMPRESSED_BYTES / 1024 / 1024
                )));
            }
            // Payload bounds come from the central directory (authoritative
            // even when the local header uses a data descriptor).
            let lo = e.local_offset as usize;
            if lo + 30 > data.len() || !data[lo..].starts_with(b"PK\x03\x04") {
                return Err(bad("bad local file header"));
            }
            let fn_len = u16le(&data[lo + 26..lo + 28]) as usize;
            let ex_len = u16le(&data[lo + 28..lo + 30]) as usize;
            let data_off = lo
                .checked_add(30)
                .and_then(|x| x.checked_add(fn_len))
                .and_then(|x| x.checked_add(ex_len))
                .ok_or_else(|| bad("local header size overflow"))?;
            let data_end = (e.comp_size as usize)
                .checked_add(data_off)
                .ok_or_else(|| bad("entry data overflow"))?;
            if data_end > data.len() {
                return Err(bad("entry data runs past the file"));
            }
            let raw = &data[data_off..data_end];
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| ImportError::Io(e.to_string()))?;
            }
            match e.method {
                0 => {
                    if raw.len() as u64 != e.uncomp_size {
                        return Err(bad("stored entry size mismatch"));
                    }
                    std::fs::write(&out, raw).map_err(|e| ImportError::Io(e.to_string()))?;
                }
                8 => {
                    let dec = flate2::read::DeflateDecoder::new(raw);
                    let mut inflated = Vec::new();
                    dec.take(MAX_DECOMPRESSED_BYTES + 1)
                        .read_to_end(&mut inflated)
                        .map_err(|e| {
                            ImportError::BadRequest(format!("zip: deflate failed: {e}"))
                        })?;
                    if inflated.len() as u64 > MAX_DECOMPRESSED_BYTES {
                        return Err(ImportError::TooLarge(format!(
                            "decompressed zip exceeds {} MiB",
                            MAX_DECOMPRESSED_BYTES / 1024 / 1024
                        )));
                    }
                    std::fs::write(&out, &inflated).map_err(|e| ImportError::Io(e.to_string()))?;
                }
                _ => unreachable!("method filtered above"),
            }
            stats.files += 1;
        }
        Ok(stats)
    }

    // ---------------------------------------------------------------------------
    // Format detection
    // ---------------------------------------------------------------------------

    /// What the staged tree turned out to be.
    #[derive(Debug, Clone)]
    pub struct DetectedPlugin {
        pub kind: &'static str, // "tool" | "hook"
        pub name: String,
        pub version: String,
        pub description: String,
        /// Human-useful capability strings for the import report.
        pub capabilities: Vec<String>,
        /// Env vars declared by a native tool manifest (for the scanner's
        /// manifest-mismatch rule).
        pub declared_env: Vec<String>,
        /// Capability names declared by a native tool manifest.
        pub capability_names: Vec<String>,
        pub notes: Vec<String>,
    }

    /// Plugin dir names must be path-safe: they become a directory under
    /// `plugins/` or `extensions/`, and a URL path segment.
    fn check_plugin_name(name: &str) -> Result<(), ImportError> {
        let bad = name.is_empty()
            || name.len() > 64
            || name.starts_with('.')
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..");
        if bad {
            Err(ImportError::BadRequest(format!(
                "manifest name {name:?} is not a safe plugin name"
            )))
        } else {
            Ok(())
        }
    }

    /// Detect the plugin layout at the staged root:
    /// - `manifest.yaml` → Pantheon native tool plugin
    /// - `plugin.yaml` → Pantheon native hook plugin (also accepts the Hermes
    ///   shapes the in-tree loader accepts, since Pantheon's format derives
    ///   from them)
    /// - `openclaw.plugin.json` → OpenClaw plugin, converted
    /// - `.claude-plugin/plugin.json` → Claude Code plugin, converted
    /// - anything else → 422 listing what was actually found.
    pub fn detect_format(root: &Path) -> Result<DetectedPlugin, ImportError> {
        let manifest_path = root.join("manifest.yaml");
        if manifest_path.exists() {
            let text = std::fs::read_to_string(&manifest_path)
                .map_err(|e| ImportError::Io(e.to_string()))?;
            let m: pantheon_exec::plugins::PluginManifest =
                pantheon_exec::plugins::parse_manifest(&text)
                    .map_err(|e| ImportError::BadRequest(format!("manifest.yaml: {e}")))?;
            check_plugin_name(&m.name)?;
            let capabilities = m
                .capabilities
                .iter()
                .map(|c| format!("tool:{}", c.name))
                .chain(m.env_vars.iter().map(|v| format!("env:{}", v.name)))
                .collect::<Vec<_>>();
            return Ok(DetectedPlugin {
                kind: "tool",
                name: m.name.clone(),
                version: m.version.clone(),
                description: m.description.clone(),
                capabilities,
                declared_env: m.env_vars.iter().map(|v| v.name.clone()).collect(),
                capability_names: m.capabilities.iter().map(|c| c.name.clone()).collect(),
                notes: Vec::new(),
            });
        }
        let plugin_yaml = root.join("plugin.yaml");
        if plugin_yaml.exists() {
            let m = pantheon_extensions::manifest::PluginManifest::load(&plugin_yaml)
                .map_err(|e| ImportError::BadRequest(format!("plugin.yaml: {e}")))?;
            check_plugin_name(&m.name)?;
            let (known, unknown) = m.hook_list();
            let mut capabilities: Vec<String> = known
                .iter()
                .map(|h| {
                    // Hook serializes snake_case (serde rename_all); fall back to
                    // Debug if serialization ever fails.
                    let name = serde_json::to_value(h)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| format!("{h:?}"));
                    format!("hook:{name}")
                })
                .collect();
            let mut notes = Vec::new();
            if !unknown.is_empty() {
                notes.push(format!(
                    "unknown hook names (kept, non-fatal): {}",
                    unknown.join(", ")
                ));
                capabilities.extend(unknown.iter().map(|h| format!("hook?:{h}")));
            }
            return Ok(DetectedPlugin {
                kind: "hook",
                name: m.name.clone(),
                version: m.version.clone(),
                description: m.description.clone(),
                capabilities,
                declared_env: Vec::new(),
                capability_names: Vec::new(),
                notes,
            });
        }
        if root.join("openclaw.plugin.json").exists() {
            return convert_openclaw_plugin(root);
        }
        if root.join(".claude-plugin").join("plugin.json").exists() {
            return convert_claude_plugin(root);
        }
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            for e in entries.flatten().take(20) {
                found.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        found.sort();
        Err(ImportError::UnsupportedLayout {
            message: "unsupported plugin layout: expected manifest.yaml (tool), plugin.yaml (hook), openclaw.plugin.json (OpenClaw), or .claude-plugin/plugin.json (Claude Code) at the repo root".into(),
            found,
        })
    }

    /// Claude Code event → Pantheon hook mapping. Events with no Pantheon
    /// analog are kept verbatim in the manifest's non-validated `hooks` list.
    const CLAUDE_EVENT_MAP: &[(&str, &str)] = &[
        ("SessionStart", "on_session_start"),
        ("SessionEnd", "on_session_end"),
        ("PreToolUse", "pre_tool_call"),
        ("PostToolUse", "post_tool_call"),
        ("UserPromptSubmit", "pre_llm_call"),
        ("PreCompact", "on_compaction"),
        ("Stop", "subagent_stop"),
        ("SubagentStop", "subagent_stop"),
    ];

    /// Convert a Claude Code plugin (`.claude-plugin/plugin.json`) into a
    /// Pantheon hook plugin.
    ///
    /// Field mapping:
    /// - `name` → plugin.yaml `name` (sanitized to lowercase `[a-z0-9-_]`, ≤64 chars; required)
    /// - `description` → `description`
    /// - `version` → `version` (default `"0.0.0"`)
    /// - `author` (string or `{name}`) → `author`
    /// - `license` → `license`
    /// - `hooks` (map of Claude event → `[{matcher, hooks: [{type, command}]}]`)
    ///   → Pantheon `provides_hooks` for mapped events; unmapped event names
    ///   are kept verbatim in the non-validated `hooks` list. Hook *bodies*
    ///   (shell commands) are preserved verbatim in `claude-hooks.json`
    ///   Pantheon's runner executes `__init__.py`, not shell, so they need
    ///   manual porting before the plugin can do anything.
    /// - `commands` / `agents` / `skills` (dirs) and `mcpServers` are NOT
    ///   converted; they surface in `detected_capabilities` and the notes.
    /// - The original `plugin.json` is preserved as `imported-plugin.json`.
    fn convert_claude_plugin(root: &Path) -> Result<DetectedPlugin, ImportError> {
        let raw = std::fs::read(root.join(".claude-plugin").join("plugin.json"))
            .map_err(|e| ImportError::Io(e.to_string()))?;
        let v: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|e| ImportError::BadRequest(format!("plugin.json: {e}")))?;

        let name_raw = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let name = sanitize_plugin_name(name_raw)?;
        let description = manifest_str(&v, "description", "");
        let version = manifest_str(&v, "version", "0.0.0");
        let author = manifest_author(&v);
        let license = manifest_str(&v, "license", "");

        let mut provides_hooks: Vec<String> = Vec::new();
        let mut raw_hooks: Vec<String> = Vec::new();
        let mut capabilities: Vec<String> = Vec::new();
        if let Some(hooks) = v.get("hooks").and_then(|h| h.as_object()) {
            let mut events: Vec<&String> = hooks.keys().collect();
            events.sort();
            for event in events {
                capabilities.push(format!("claude-hook:{event}"));
                match CLAUDE_EVENT_MAP.iter().find(|(k, _)| k == event) {
                    Some((_, pantheon)) => {
                        let p = pantheon.to_string();
                        if !provides_hooks.contains(&p) {
                            provides_hooks.push(p);
                        }
                    }
                    None => raw_hooks.push(event.clone()),
                }
            }
        }
        for (field, label) in [
            ("commands", "claude-commands"),
            ("agents", "claude-agents"),
            ("skills", "claude-skills"),
        ] {
            match v.get(field) {
                Some(serde_json::Value::Array(a)) => {
                    capabilities.push(format!("{label}:{}", a.len()))
                }
                Some(_) => capabilities.push(label.to_string()),
                None => {}
            }
        }
        if let Some(mcp) = v.get("mcpServers").and_then(|x| x.as_object()) {
            capabilities.push(format!("claude-mcp-servers:{}", mcp.len()));
        }

        write_converted_manifest(
            root,
            ConvertedMeta {
                name: &name,
                version: &version,
                description: &description,
                author: &author,
                license: &license,
                provides_hooks: &provides_hooks,
                raw_hooks: &raw_hooks,
            },
            &raw,
        )?;
        if let Some(hooks) = v.get("hooks") {
            let _ = std::fs::write(
                root.join("claude-hooks.json"),
                serde_json::to_string_pretty(hooks).unwrap_or_default(),
            );
        }

        let mut notes = vec![
            "Converted from a Claude Code plugin. Hook bodies are preserved in claude-hooks.json / imported-plugin.json; Pantheon hook plugins run __init__.py via the Python runner, so port the shell commands manually before approving."
                .to_string(),
        ];
        if name_raw != name {
            notes.push(format!(
                "plugin name sanitized from {name_raw:?} to {name:?}"
            ));
        }
        if capabilities
            .iter()
            .any(|c| c.starts_with("claude-mcp-servers"))
        {
            notes.push(
                "mcpServers are not converted; configure them as Pantheon MCP servers instead."
                    .to_string(),
            );
        }

        Ok(DetectedPlugin {
            kind: "hook",
            name,
            version,
            description,
            capabilities,
            declared_env: Vec::new(),
            capability_names: Vec::new(),
            notes,
        })
    }

    /// Shared string-field extraction for the foreign-manifest converters.
    fn manifest_str(v: &serde_json::Value, key: &str, default: &str) -> String {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or(default)
            .to_string()
    }

    fn manifest_author(v: &serde_json::Value) -> String {
        match v.get("author") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(a) => a
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            None => String::new(),
        }
    }

    /// Write the Pantheon `plugin.yaml` for a converted foreign manifest and
    /// preserve the original as `imported-plugin.json` for manual porting.
    /// The manifest header fields carried from a converted foreign plugin.
    struct ConvertedMeta<'a> {
        name: &'a str,
        version: &'a str,
        description: &'a str,
        author: &'a str,
        license: &'a str,
        provides_hooks: &'a [String],
        raw_hooks: &'a [String],
    }

    fn write_converted_manifest(
        root: &Path,
        meta: ConvertedMeta<'_>,
        original_raw: &[u8],
    ) -> Result<(), ImportError> {
        // Flat keys, exactly as `pantheon_extensions::manifest::PluginManifest`
        // parses them (`name`, `version`, ...). Dotted keys (`"meta.name"`)
        // used to land in the manifest's flattened `extra` map while the
        // required `name` field stayed missing, so the manifest failed to
        // parse and the promoted plugin was silently unloaded (stderr only).
        let yaml = serde_yaml::to_string(&serde_json::json!({
            "name": meta.name,
            "version": meta.version,
            "description": meta.description,
            "author": meta.author,
            "license": meta.license,
            "provides_hooks": meta.provides_hooks,
            "hooks": meta.raw_hooks,
        }))
        .map_err(|e| ImportError::Io(e.to_string()))?;
        std::fs::write(root.join("plugin.yaml"), yaml)
            .map_err(|e| ImportError::Io(e.to_string()))?;
        let _ = std::fs::write(root.join("imported-plugin.json"), original_raw);
        Ok(())
    }

    /// Convert an OpenClaw plugin (`openclaw.plugin.json`, `{id, meta.name,
    /// meta.description, meta.version, ...}`) into a Pantheon hook plugin.
    ///
    /// Field mapping mirrors the Claude Code converter:
    /// - `meta.name` (fallback: `id`) → plugin.yaml `name` (sanitized to
    ///   lowercase `[a-z0-9-_]`, ≤64 chars; required)
    /// - `meta.description` → `description`
    /// - `meta.version` → `version` (default `"0.0.0"`)
    /// - `meta.author` (string or `{meta.name}`) → `author`
    /// - `meta.license` → `license`
    /// - `commands` / `agents` / `skills` (arrays) → `openclaw-*`
    ///   capability counters
    /// - `hooks` (object of event → handler, or array) → event names kept
    ///   verbatim in the non-validated `hooks` list (no Pantheon event
    ///   mapping exists for OpenClaw events) and surfaced as
    ///   `openclaw-hook:<event>` capabilities
    /// - `mcpServers` → capability counter; not converted (note points at
    ///   Pantheon MCP servers instead)
    /// - The original manifest is preserved as `imported-plugin.json`.
    fn convert_openclaw_plugin(root: &Path) -> Result<DetectedPlugin, ImportError> {
        let raw = std::fs::read(root.join("openclaw.plugin.json"))
            .map_err(|e| ImportError::Io(e.to_string()))?;
        let v: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|e| ImportError::BadRequest(format!("openclaw.plugin.json: {e}")))?;

        let name_raw = v
            .get("name")
            .and_then(|x| x.as_str())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| v.get("id").and_then(|x| x.as_str()))
            .unwrap_or("");
        let name = sanitize_plugin_name(name_raw)?;
        let description = manifest_str(&v, "description", "");
        let version = manifest_str(&v, "version", "0.0.0");
        let author = manifest_author(&v);
        let license = manifest_str(&v, "license", "");

        let mut capabilities: Vec<String> = Vec::new();
        let mut raw_hooks: Vec<String> = Vec::new();
        for (field, label) in [
            ("commands", "openclaw-commands"),
            ("agents", "openclaw-agents"),
            ("skills", "openclaw-skills"),
        ] {
            match v.get(field) {
                Some(serde_json::Value::Array(a)) => {
                    capabilities.push(format!("{label}:{}", a.len()))
                }
                Some(_) => capabilities.push(label.to_string()),
                None => {}
            }
        }
        match v.get("hooks") {
            Some(serde_json::Value::Object(map)) => {
                let mut events: Vec<&String> = map.keys().collect();
                events.sort();
                for event in events {
                    capabilities.push(format!("openclaw-hook:{event}"));
                    raw_hooks.push(event.clone());
                }
            }
            Some(serde_json::Value::Array(a)) => {
                capabilities.push(format!("openclaw-hooks:{}", a.len()));
            }
            Some(_) => capabilities.push("openclaw-hooks".to_string()),
            None => {}
        }
        if let Some(mcp) = v.get("mcpServers").and_then(|x| x.as_object()) {
            capabilities.push(format!("openclaw-mcp-servers:{}", mcp.len()));
        }

        write_converted_manifest(
            root,
            ConvertedMeta {
                name: &name,
                version: &version,
                description: &description,
                author: &author,
                license: &license,
                provides_hooks: &[],
                raw_hooks: &raw_hooks,
            },
            &raw,
        )?;

        let mut notes = vec![
            "Converted from an OpenClaw plugin (openclaw.plugin.json). Command/agent/skill bodies are preserved in imported-plugin.json; Pantheon hook plugins run __init__.py via the Python runner, so port them manually before approving."
                .to_string(),
        ];
        if name_raw != name {
            notes.push(format!(
                "plugin name sanitized from {name_raw:?} to {name:?}"
            ));
        }
        if capabilities
            .iter()
            .any(|c| c.starts_with("openclaw-mcp-servers"))
        {
            notes.push(
                "mcpServers are not converted; configure them as Pantheon MCP servers instead."
                    .to_string(),
            );
        }

        Ok(DetectedPlugin {
            kind: "hook",
            name,
            version,
            description,
            capabilities,
            declared_env: Vec::new(),
            capability_names: Vec::new(),
            notes,
        })
    }

    fn sanitize_plugin_name(raw: &str) -> Result<String, ImportError> {
        let clean: String = raw
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if clean.is_empty() || clean.len() > 64 {
            Err(ImportError::BadRequest(
                "plugin.json 'name' is missing or unusable as a plugin name".into(),
            ))
        } else {
            Ok(clean)
        }
    }

    // ---------------------------------------------------------------------------
    // Static security scanner
    // ---------------------------------------------------------------------------
    //
    // Heuristic static analysis over the staged files - NOT a sandbox, NOT a
    // guarantee. It catches low-effort malicious patterns and flags
    // suspicious ones for human review. A clean verdict means "nothing
    // obviously bad found", not "safe". Clever attackers evade static
    // heuristics; the quarantine dir (never on a load path) and the
    // approval gate are the real enforcement.

    /// One scanner hit.
    #[derive(Debug, Clone)]
    pub struct ScanFinding {
        pub severity: &'static str, // "critical" | "high" | "medium" | "low"
        pub file: String,           // staged-relative path
        pub line: Option<u64>,      // 1-based, None for aggregate rules
        pub rule: &'static str,
        pub description: String,
    }

    /// Whole-tree verdict: "clean" | "suspicious" | "malicious".
    /// critical → malicious; high/medium → suspicious; low-only → clean.
    #[derive(Debug, Clone)]
    pub struct ScanReport {
        pub verdict: &'static str,
        pub findings: Vec<ScanFinding>,
    }

    impl ScanReport {
        pub fn to_json(&self) -> serde_json::Value {
            serde_json::json!({
                "verdict": self.verdict,
                "findings": self.findings.iter().map(|f| serde_json::json!({
                    "severity": f.severity,
                    "file": f.file,
                    "line": f.line,
                    "rule": f.rule,
                    "description": f.description,
                })).collect::<Vec<_>>(),
            })
        }

        pub fn from_json(v: &serde_json::Value) -> Option<ScanReport> {
            let verdict = v.get("verdict")?.as_str()?;
            if !matches!(verdict, "clean" | "suspicious" | "malicious") {
                return None;
            }
            // 'static coercion: only the three known verdicts pass the check above.
            let verdict: &'static str = match verdict {
                "clean" => "clean",
                "suspicious" => "suspicious",
                _ => "malicious",
            };
            let mut findings = Vec::new();
            for f in v.get("findings")?.as_array()? {
                let severity = f.get("severity")?.as_str()?;
                let severity: &'static str = match severity {
                    "critical" => "critical",
                    "high" => "high",
                    "medium" => "medium",
                    _ => "low",
                };
                let rule = f.get("rule")?.as_str()?;
                let rule: &'static str = match rule {
                    "miner" => "miner",
                    "download_execute" => "download_execute",
                    "obfuscated_exec" => "obfuscated_exec",
                    "credential_harvest" => "credential_harvest",
                    "exfiltration" => "exfiltration",
                    "persistence" => "persistence",
                    "base64_blob" => "base64_blob",
                    "manifest_mismatch" => "manifest_mismatch",
                    _ => "unknown",
                };
                findings.push(ScanFinding {
                    severity,
                    file: f.get("file")?.as_str()?.to_string(),
                    line: f.get("line").and_then(|x| x.as_u64()),
                    rule,
                    description: f.get("description")?.as_str()?.to_string(),
                });
            }
            Some(ScanReport { verdict, findings })
        }
    }

    /// Manifest context for the manifest-mismatch rule. Only native tool
    /// plugins declare env vars / capabilities, so the rule only applies to them.
    pub struct ScanContext {
        pub tool_manifest: bool,
        pub declared_env: Vec<String>,
        pub capability_names: Vec<String>,
    }

    #[derive(Default)]
    struct ScanState {
        env_reads: HashMap<String, String>, // var -> first file that read it
        network_files: Vec<String>,         // files with network use (capped)
    }

    const MAX_FINDINGS: usize = 200;

    /// Scan every text file under `root`.
    pub fn scan_tree(root: &Path, ctx: &ScanContext) -> ScanReport {
        let mut findings: Vec<ScanFinding> = Vec::new();
        let mut state = ScanState::default();
        let mut files_scanned = 0usize;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for path in paths {
                if findings.len() >= MAX_FINDINGS {
                    break;
                }
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if rel == ".scan-report.json" || rel == ".import-meta.json" {
                    continue;
                }
                let meta = match std::fs::metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.len() > MAX_SCANNED_FILE_BYTES {
                    continue;
                }
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // Binary sniff: NUL in the first 4 KiB.
                let head = &bytes[..bytes.len().min(4096)];
                if head.contains(&0) {
                    continue;
                }
                files_scanned += 1;
                if files_scanned > 5000 {
                    break;
                }
                let text = String::from_utf8_lossy(&bytes);
                scan_text(&rel, &text, &mut state, &mut findings);
            }
        }

        // Aggregate rule: manifest-vs-code mismatch (tool plugins only).
        if ctx.tool_manifest {
            for (var, file) in &state.env_reads {
                if findings.len() >= MAX_FINDINGS {
                    break;
                }
                if !ctx.declared_env.iter().any(|d| d == var) {
                    findings.push(ScanFinding {
                        severity: "medium",
                        file: file.clone(),
                        line: None,
                        rule: "manifest_mismatch",
                        description: format!(
                            "code reads env var {var:?} which the manifest does not declare in env_vars"
                        ),
                    });
                }
            }
            let declares_network = ctx.capability_names.iter().any(|c| {
                let cl = c.to_lowercase();
                cl.contains("net")
                    || cl.contains("http")
                    || cl.contains("fetch")
                    || cl.contains("socket")
            });
            if !declares_network {
                for file in state.network_files.iter().take(5) {
                    if findings.len() >= MAX_FINDINGS {
                        break;
                    }
                    findings.push(ScanFinding {
                        severity: "medium",
                        file: file.clone(),
                        line: None,
                        rule: "manifest_mismatch",
                        description:
                            "code uses the network but the manifest declares no network capability"
                                .to_string(),
                    });
                }
            }
        }

        let verdict = if findings.iter().any(|f| f.severity == "critical") {
            "malicious"
        } else if findings
            .iter()
            .any(|f| f.severity == "high" || f.severity == "medium")
        {
            "suspicious"
        } else {
            "clean"
        };
        ScanReport { verdict, findings }
    }

    fn scan_text(rel: &str, text: &str, state: &mut ScanState, findings: &mut Vec<ScanFinding>) {
        for (idx, line) in text.lines().enumerate() {
            if findings.len() >= MAX_FINDINGS {
                return;
            }
            let line_no = (idx + 1) as u64;
            let l = line.to_lowercase();
            macro_rules! hit {
                ($rule:expr, $sev:expr, $desc:expr) => {
                    findings.push(ScanFinding {
                        severity: $sev,
                        file: rel.to_string(),
                        line: Some(line_no),
                        rule: $rule,
                        description: $desc.to_string(),
                    })
                };
            }
            // --- miners (critical) ---
            for pat in [
                "xmrig",
                "minerd",
                "cryptonight",
                "stratum+tcp",
                "nicehash",
                "ethminer",
                "cpuminer",
                "kawpow",
                "randomx",
            ] {
                if l.contains(pat) {
                    hit!("miner", "critical", format!("known miner string {pat:?}"));
                    break;
                }
            }
            // --- download-and-execute (critical) ---
            if (l.contains("curl") || l.contains("wget"))
                && (l.contains("| sh")
                    || l.contains("|sh")
                    || l.contains("| bash")
                    || l.contains("|bash")
                    || l.contains("| zsh"))
            {
                hit!(
                    "download_execute",
                    "critical",
                    "download piped directly into a shell"
                );
            }
            if l.contains("iex(") || l.contains("invoke-expression") {
                hit!(
                    "download_execute",
                    "critical",
                    "PowerShell Invoke-Expression"
                );
            }
            if l.contains("eval(")
                && (l.contains("curl")
                    || l.contains("wget")
                    || l.contains("requests.get")
                    || l.contains("urlopen")
                    || l.contains("fetch("))
            {
                hit!(
                    "download_execute",
                    "critical",
                    "eval() applied to fetched content"
                );
            }
            // --- obfuscated execution (critical) ---
            if (l.contains("eval(") || l.contains("exec("))
                && (l.contains("base64") || l.contains("b64decode") || l.contains("atob("))
            {
                hit!(
                    "obfuscated_exec",
                    "critical",
                    "eval/exec of a base64-decoded payload"
                );
            }
            if l.contains("new function(") && l.contains("fromcharcode") {
                hit!(
                    "obfuscated_exec",
                    "critical",
                    "Function constructor with char-code obfuscation"
                );
            }
            // --- credential harvesting (high) ---
            for pat in [
                ".ssh/",
                ".aws/credentials",
                ".gnupg/",
                "keychain",
                "login data",
                "cookies",
                ".mozilla/",
                "application support/google/chrome",
                "application support/firefox",
                "security find",
                "secret-tool",
                "lsass",
                "ntds.dit",
                "/etc/shadow",
            ] {
                if l.contains(pat) {
                    hit!(
                        "credential_harvest",
                        "high",
                        format!("touches credential store {pat:?}")
                    );
                    break;
                }
            }
            if is_env_read(&l) && contains_secret_word(&l) {
                hit!(
                    "credential_harvest",
                    "high",
                    "reads a secret-looking environment variable"
                );
            }
            // --- exfiltration (high): network send carrying sensitive data ---
            if is_network_send(&l)
                && (contains_secret_word(&l) || l.contains(".ssh") || l.contains("keychain"))
            {
                hit!(
                    "exfiltration",
                    "high",
                    "sends sensitive data over the network"
                );
            }
            // --- persistence (medium) ---
            for pat in [
                "crontab",
                "/etc/cron",
                "systemctl enable",
                "launchagents",
                "launchdaemons",
                "currentversion\\run",
                "shell:startup",
                ".config/autostart",
                "schtasks",
                "update-rc.d",
            ] {
                if l.contains(pat) {
                    hit!(
                        "persistence",
                        "medium",
                        format!("persistence mechanism {pat:?}")
                    );
                    break;
                }
            }
            // --- large base64 blobs (medium): possible packed payloads ---
            if has_long_base64_token(line) {
                hit!(
                    "base64_blob",
                    "medium",
                    "large base64 blob (possible packed payload)"
                );
            }
            // --- aggregate-rule inputs ---
            for var in extract_env_reads(line) {
                state
                    .env_reads
                    .entry(var)
                    .or_insert_with(|| rel.to_string());
            }
            if is_network_use(&l) && !state.network_files.contains(&rel.to_string()) {
                state.network_files.push(rel.to_string());
            }
        }
    }

    fn is_env_read(l: &str) -> bool {
        l.contains("os.environ")
            || l.contains("os.getenv")
            || l.contains("process.env")
            || l.contains("getenv(")
            || l.contains("env::var")
    }

    fn contains_secret_word(l: &str) -> bool {
        [
            "api_key",
            "apikey",
            "secret",
            "passwd",
            "password",
            "token",
            "private_key",
            "privatekey",
            "aws_",
            "github_token",
            "openai",
            "bearer",
            "credentials",
        ]
        .iter()
        .any(|w| l.contains(w))
    }

    fn is_network_send(l: &str) -> bool {
        [
            "requests.post",
            "requests.put",
            "requests.patch",
            "urllib.request",
            "http.client",
            "fetch(",
            "axios.post",
            "axios.put",
            "curl ",
            "--data",
            "wget --post-data",
            "invoke-webrequest",
            "invoke-restmethod",
        ]
        .iter()
        .any(|w| l.contains(w))
    }

    fn is_network_use(l: &str) -> bool {
        is_network_send(l)
            || [
                "requests.get",
                "requests.",
                "urllib",
                "http.get",
                "axios",
                "curl",
                "wget",
                "socket.",
                "reqwest",
                "net/http",
                "urlopen",
            ]
            .iter()
            .any(|w| l.contains(w))
    }

    /// A run of ≥200 base64-alphabet characters - rare in honest code,
    ///
    /// common in packed payloads.
    fn has_long_base64_token(line: &str) -> bool {
        let mut run = 0usize;
        for c in line.chars() {
            if c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' {
                run += 1;
                if run >= 200 {
                    return true;
                }
            } else {
                run = 0;
            }
        }
        false
    }

    /// Extract env var names read in one source line. Heuristic patterns for
    /// Python / Node / C / Rust / Ruby.
    fn extract_env_reads(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        for marker in [
            "os.environ[",
            "os.environ.get(",
            "os.getenv(",
            "getenv(",
            "env::var(",
            "ENV[",
        ] {
            let mut rest = line;
            while let Some(i) = rest.find(marker) {
                rest = &rest[i + marker.len()..];
                let t = rest.trim_start();
                let mut chars = t.chars();
                let quote = chars.next();
                if quote == Some('"') || quote == Some('\'') {
                    let q = quote.unwrap();
                    if let Some(end) = t[1..].find(q) {
                        let var: String = t[1..1 + end]
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                            .collect();
                        if !var.is_empty() {
                            out.push(var);
                        }
                    }
                }
            }
        }
        let mut rest = line;
        while let Some(i) = rest.find("process.env.") {
            rest = &rest[i + 13..];
            let var: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !var.is_empty() {
                out.push(var);
            }
        }
        out
    }

    // ---------------------------------------------------------------------------
    // Quarantine
    // ---------------------------------------------------------------------------

    /// Staging/quarantine live here - never on any plugin load path.
    /// (The tool loader scans `<data_dir>/plugins` immediate subdirs for
    /// `manifest.yaml`; `.quarantine` contains none directly, and the
    /// loader additionally skips dot-directories. The hook loader only
    /// scans `<data_dir>/extensions`.)
    pub fn quarantine_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("plugins").join(".quarantine")
    }

    fn staging_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("plugins").join(".staging")
    }

    /// Metadata persisted beside a quarantined plugin.
    #[derive(Debug, Clone)]
    pub struct ImportMeta {
        pub name: String,
        pub kind: String, // "tool" | "hook"
        pub version: String,
        pub description: String,
        pub source: String, // "github"
        pub source_ref: String,
        pub imported_at: u64,
    }

    impl ImportMeta {
        fn file(dir: &Path) -> PathBuf {
            dir.join(".import-meta.json")
        }

        pub fn load(data_dir: &Path, name: &str) -> Option<ImportMeta> {
            Self::load_from(&quarantine_dir(data_dir).join(name))
        }

        pub fn load_from(dir: &Path) -> Option<ImportMeta> {
            let text = std::fs::read_to_string(Self::file(dir)).ok()?;
            let v: serde_json::Value = serde_json::from_str(&text).ok()?;
            Some(ImportMeta {
                name: v.get("name")?.as_str()?.to_string(),
                kind: v.get("kind")?.as_str()?.to_string(),
                version: v.get("version")?.as_str().unwrap_or("").to_string(),
                description: v.get("description")?.as_str().unwrap_or("").to_string(),
                source: v.get("source")?.as_str().unwrap_or("").to_string(),
                source_ref: v.get("source_ref")?.as_str().unwrap_or("").to_string(),
                imported_at: v.get("imported_at")?.as_u64().unwrap_or(0),
            })
        }

        fn write_to(&self, dir: &Path) -> std::io::Result<()> {
            std::fs::write(
                Self::file(dir),
                serde_json::to_string_pretty(&serde_json::json!({
                    "name": self.name,
                    "kind": self.kind,
                    "version": self.version,
                    "description": self.description,
                    "source": self.source,
                    "source_ref": self.source_ref,
                    "imported_at": self.imported_at,
                }))
                .unwrap_or_default(),
            )
        }
    }

    pub fn is_quarantined(data_dir: &Path, name: &str) -> bool {
        quarantine_dir(data_dir).join(name).is_dir()
    }

    /// The persisted scan verdict for a quarantined plugin, if any.
    pub fn read_scan_verdict(data_dir: &Path, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(
            quarantine_dir(data_dir)
                .join(name)
                .join(".scan-report.json"),
        )
        .ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        Some(v.get("verdict")?.as_str()?.to_string())
    }

    /// The full persisted scan report for a quarantined plugin, if any.
    pub fn read_scan_report(data_dir: &Path, name: &str) -> Option<ScanReport> {
        let text = std::fs::read_to_string(
            quarantine_dir(data_dir)
                .join(name)
                .join(".scan-report.json"),
        )
        .ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        ScanReport::from_json(&v)
    }

    /// Promote a quarantined plugin into its live dir on approval. The scan
    /// report and import meta travel with it as an audit trail.
    pub fn promote_from_quarantine(data_dir: &Path, meta: &ImportMeta) -> Result<(), String> {
        let src = quarantine_dir(data_dir).join(&meta.name);
        if !src.is_dir() {
            return Err("quarantined plugin disappeared".into());
        }
        let dst = match meta.kind.as_str() {
            "tool" => data_dir.join("plugins").join(&meta.name),
            "hook" => data_dir.join("extensions").join(&meta.name),
            k => return Err(format!("unknown plugin kind {k:?}")),
        };
        if dst.exists() {
            return Err(format!(
                "a live plugin named {:?} already exists",
                meta.name
            ));
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if let Err(rename_err) = std::fs::rename(&src, &dst) {
            // Cross-device fallback: copy then remove.
            copy_dir_recursive(&src, &dst)
                .and_then(|_| std::fs::remove_dir_all(&src).map_err(|e| e.to_string()))
                .map_err(|e| format!("promote failed (rename: {rename_err}): {e}"))?;
        }
        Ok(())
    }

    fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        let entries = std::fs::read_dir(src).map_err(|e| e.to_string())?;
        for e in entries.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(src).map_err(|e| e.to_string())?;
            let target = dst.join(rel);
            if p.is_dir() {
                copy_dir_recursive(&p, &target)?;
            } else {
                std::fs::copy(&p, &target).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    // ---------------------------------------------------------------------------
    // ClawHub
    // ---------------------------------------------------------------------------

    /// ClawHub → registry entry field mapping (verified against the live API
    /// 2026-09-30):
    /// - `slug` ← item.slug
    /// - `name` ← item.displayName
    /// - `description` ← item.summary (fallback: item.description, often null)
    /// - `version` ← item.tags.latest (fallback: item.latestVersion.version)
    /// - `kind` ← "skill" (constant: ClawHub lists skills only; there is no
    ///   plugin family, so every result is a skill, never a plugin)
    #[derive(Debug, Clone)]
    pub struct RegistryEntry {
        pub slug: String,
        pub name: String,
        pub description: String,
        pub version: String,
        pub kind: &'static str,
    }

    impl RegistryEntry {
        pub fn to_json(&self) -> serde_json::Value {
            serde_json::json!({
                "slug": self.slug,
                "name": self.name,
                "description": self.description,
                "version": self.version,
                "kind": self.kind,
            })
        }
    }

    fn registry_entry_from(item: &serde_json::Value) -> RegistryEntry {
        let slug = item
            .get("slug")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let name = item
            .get("displayName")
            .and_then(|x| x.as_str())
            .unwrap_or(&slug)
            .to_string();
        let description = item
            .get("summary")
            .and_then(|x| x.as_str())
            .or_else(|| item.get("description").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string();
        let version = item
            .get("tags")
            .and_then(|t| t.get("latest"))
            .and_then(|x| x.as_str())
            .or_else(|| {
                item.get("latestVersion")
                    .and_then(|x| x.get("version"))
                    .and_then(|x| x.as_str())
            })
            .unwrap_or("")
            .to_string();
        RegistryEntry {
            slug,
            name,
            description,
            version,
            kind: "skill",
        }
    }

    /// Proxy ClawHub skill search for the registry picker UI.
    pub fn clawhub_search(fetch: &FetchFn, query: &str) -> Result<Vec<RegistryEntry>, ImportError> {
        let q = query.trim();
        if q.is_empty() {
            return Err(ImportError::BadRequest("q is required".into()));
        }
        if q.len() > 200 {
            return Err(ImportError::BadRequest("q is too long".into()));
        }
        let url = format!("{CLAWHUB_API}/skills?q={}&limit=25", percent_encode(q));
        let body = fetch(&url).map_err(|e| match e {
            FetchError::Status(code, _) => {
                ImportError::FetchFailed(format!("ClawHub search failed: HTTP {code}"))
            }
            other => ImportError::FetchFailed(format!("ClawHub search failed: {other:?}")),
        })?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| ImportError::FetchFailed(format!("ClawHub returned bad JSON: {e}")))?;
        let items = v
            .get("items")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items.iter().map(registry_entry_from).collect())
    }

    /// A ClawHub package resolved to downloadable bytes.
    struct ClawHubPackage {
        bytes: Vec<u8>,
        version: String,
    }

    /// Resolve `clawhub:<slug>` via the packages API and download the package
    /// zip (verified live 2026-09-30: `GET /api/v1/packages/{name}` →
    /// `{"package": {family, latestVersion, ...}}`,
    /// `GET /api/v1/packages/{name}/download?version={v}` → zip).
    ///
    /// The slug may be `<name>` or `<owner>/<name>`; the API is addressed by
    /// the final segment. A package whose `family` is `skill` still ends in
    /// 422 NOT_A_PLUGIN pointing at the skills importer; a slug that 404s on
    /// the packages endpoint is checked against the skills endpoint before
    /// giving up, so the hint survives for skill slugs.
    fn fetch_clawhub_package(fetch: &FetchFn, slug: &str) -> Result<ClawHubPackage, ImportError> {
        let api_name = slug.rsplit('/').next().unwrap_or(slug);
        let meta_url = format!("{CLAWHUB_API}/packages/{}", percent_encode(api_name));
        let body = match fetch(&meta_url) {
            Ok(b) => b,
            Err(FetchError::Status(404, _)) => return Err(clawhub_skill_hint(fetch, slug)),
            Err(e) => {
                return Err(ImportError::FetchFailed(format!(
                    "ClawHub package lookup failed: {e:?}"
                )))
            }
        };
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| ImportError::FetchFailed(format!("ClawHub returned bad JSON: {e}")))?;
        let pkg = v.get("package").unwrap_or(&v);
        if pkg
            .get("family")
            .and_then(|x| x.as_str())
            .map(|f| f.eq_ignore_ascii_case("skill"))
            .unwrap_or(false)
        {
            return Err(not_a_plugin_error(pkg, slug));
        }
        // `latestVersion` is a plain string on the packages endpoint
        // ("2.5.0"); accept the object shape too, then the other known
        // shapes, defensively.
        let version = pkg
            .get("latestVersion")
            .and_then(|x| {
                x.as_str().map(str::to_string).or_else(|| {
                    x.get("version")
                        .and_then(|y| y.as_str())
                        .map(str::to_string)
                })
            })
            .or_else(|| {
                pkg.get("tags")
                    .and_then(|t| t.get("latest"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
            })
            .or_else(|| {
                let s = manifest_str(pkg, "version", "");
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            })
            .ok_or_else(|| {
                ImportError::FetchFailed(format!(
                    "ClawHub package {slug:?} has no published version"
                ))
            })?;
        let dl_url = format!(
            "{CLAWHUB_API}/packages/{}/download?version={}",
            percent_encode(api_name),
            percent_encode(&version)
        );
        let bytes = fetch(&dl_url).map_err(|e| match e {
            FetchError::Status(404, _) => ImportError::NotFound(format!(
                "ClawHub package {slug:?} has no download for version {version:?}"
            )),
            other => ImportError::FetchFailed(format!("ClawHub download failed: {other:?}")),
        })?;
        Ok(ClawHubPackage { bytes, version })
    }

    /// A slug that 404s on the packages endpoint might be a ClawHub skill
    /// check the skills endpoint so the operator still gets the helpful
    /// NOT_A_PLUGIN hint instead of a bare 404.
    fn clawhub_skill_hint(fetch: &FetchFn, slug: &str) -> ImportError {
        let api_name = slug.rsplit('/').next().unwrap_or(slug);
        let url = format!("{CLAWHUB_API}/skills/{}", percent_encode(api_name));
        match fetch(&url) {
            Ok(body) => {
                let v: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let skill = v.get("skill").unwrap_or(&v);
                not_a_plugin_error(skill, slug)
            }
            Err(FetchError::Status(404, _)) => {
                ImportError::NotFound(format!("no ClawHub package or skill for slug {slug:?}"))
            }
            Err(e) => ImportError::FetchFailed(format!("ClawHub lookup failed: {e:?}")),
        }
    }

    fn not_a_plugin_error(entry_json: &serde_json::Value, slug: &str) -> ImportError {
        let mut entry = registry_entry_from(entry_json);
        // The packages-metadata shape has no `slug` field - the slug the
        // operator typed is authoritative.
        if entry.slug.is_empty() {
            entry.slug = slug.to_string();
        }
        if entry.name.is_empty() {
            entry.name = entry_json
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or(slug)
                .to_string();
        }
        ImportError::NotAPlugin {
            message: format!(
                "clawhub:{slug} is a ClawHub skill (SKILL.md bundle), not a Pantheon plugin - refusing to install it as a plugin"
            ),
            skill: entry.to_json(),
            hint: "POST /api/skills/import".into(),
        }
    }

    // ---------------------------------------------------------------------------
    // Import orchestration
    // ---------------------------------------------------------------------------

    /// The successful import report, serialized as the endpoint response.
    #[derive(Debug)]
    pub struct ImportReport {
        pub name: String,
        pub kind: &'static str,
        pub version: String,
        pub source: &'static str,
        pub source_ref: String,
        pub capabilities: Vec<String>,
        pub notes: Vec<String>,
        pub scan: ScanReport,
    }

    impl ImportReport {
        pub fn to_json(&self) -> serde_json::Value {
            serde_json::json!({
                "ok": true,
                "name": self.name,
                "kind": self.kind,
                "version": self.version,
                "source": self.source,
                "source_ref": self.source_ref,
                "detected_capabilities": self.capabilities,
                "notes": self.notes,
                "approval_required": true,
                "quarantined": true,
                "scan_report": self.scan.to_json(),
            })
        }
    }

    fn map_fetch_error(e: FetchError, url: &str) -> ImportError {
        match e {
            FetchError::Status(404, _) => ImportError::NotFound(format!(
                "GitHub returned 404 for {url} (bad owner/repo/ref?)"
            )),
            FetchError::Status(code, text) => {
                let text = text.trim();
                let detail = if text.is_empty() {
                    format!("HTTP {code}")
                } else {
                    format!("HTTP {code} ({text})")
                };
                ImportError::FetchFailed(format!("download failed: {detail}"))
            }
            FetchError::Transport(msg) => ImportError::FetchFailed(format!(
                "download failed: {}",
                pantheon_exec::http::fetch_error_message(url, &msg)
            )),
            FetchError::TooLarge => ImportError::TooLarge(format!(
                "download exceeds {} MiB",
                MAX_DOWNLOAD_BYTES / 1024 / 1024
            )),
        }
    }

    /// Which archive layout the downloaded bundle uses.
    #[derive(Debug, Clone, Copy)]
    enum ArchiveKind {
        TarGz,
        Zip,
    }

    /// Full import: validate → fetch → extract → detect → scan → quarantine.
    /// `fetch` is injectable so tests never touch the network.
    pub fn run_import(
        data_dir: &Path,
        url: &str,
        ref_field: Option<&str>,
        fetch: &FetchFn,
    ) -> Result<ImportReport, ImportError> {
        let spec = parse_source_spec(url, ref_field)?;
        // (bundle bytes, archive layout, source label, source ref)
        let (bytes, kind, source, source_ref): (Vec<u8>, ArchiveKind, &'static str, String) =
            match spec {
                SourceSpec::ClawHub { slug } => {
                    let pkg = fetch_clawhub_package(fetch, &slug)?;
                    (
                        pkg.bytes,
                        ArchiveKind::Zip,
                        "clawhub",
                        format!("clawhub:{slug}@{}", pkg.version),
                    )
                }
                SourceSpec::Github {
                    owner,
                    repo,
                    gitref,
                } => {
                    // Download. Default ref `main`; one retry with `master` on 404.
                    let tarball_url =
                        format!("https://codeload.github.com/{owner}/{repo}/tar.gz/{gitref}");
                    let (gitref_used, bytes) = match fetch(&tarball_url) {
                        Ok(b) => (gitref.clone(), b),
                        Err(FetchError::Status(404, _)) if gitref == "main" => {
                            let alt =
                                format!("https://codeload.github.com/{owner}/{repo}/tar.gz/master");
                            match fetch(&alt) {
                                Ok(b) => ("master".to_string(), b),
                                Err(e) => return Err(map_fetch_error(e, &alt)),
                            }
                        }
                        Err(e) => return Err(map_fetch_error(e, &tarball_url)),
                    };
                    (
                        bytes,
                        ArchiveKind::TarGz,
                        "github",
                        format!("{owner}/{repo}@{gitref_used}"),
                    )
                }
            };

        // The cap holds no matter which fetch impl produced the bytes.
        if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
            return Err(ImportError::TooLarge(format!(
                "download exceeds {} MiB",
                MAX_DOWNLOAD_BYTES / 1024 / 1024
            )));
        }

        // Stage under .staging/; always cleaned up (rename moves it away on success).
        let stage_root = staging_dir(data_dir);
        std::fs::create_dir_all(&stage_root).map_err(|e| ImportError::Io(e.to_string()))?;
        let stage = stage_root.join(format!("import-{}-{}", now_unix(), std::process::id()));
        std::fs::create_dir_all(&stage).map_err(|e| ImportError::Io(e.to_string()))?;

        let result = run_import_staged(data_dir, &stage, &bytes, kind, source, &source_ref);
        let _ = std::fs::remove_dir_all(&stage);
        result
    }

    fn run_import_staged(
        data_dir: &Path,
        stage: &Path,
        bytes: &[u8],
        kind: ArchiveKind,
        source: &'static str,
        source_ref: &str,
    ) -> Result<ImportReport, ImportError> {
        let stats = match kind {
            ArchiveKind::TarGz => extract_tar_gz(bytes, stage)?,
            ArchiveKind::Zip => extract_zip(bytes, stage)?,
        };
        if stats.files == 0 {
            return Err(ImportError::BadRequest(
                "bundle contained no usable files".into(),
            ));
        }
        let detected = detect_format(stage)?;

        let ctx = ScanContext {
            tool_manifest: detected.kind == "tool",
            declared_env: detected.declared_env.clone(),
            capability_names: detected.capability_names.clone(),
        };
        let scan = scan_tree(stage, &ctx);

        // Quarantine: never on a load path, never approved by default.
        let qdir = quarantine_dir(data_dir);
        std::fs::create_dir_all(&qdir).map_err(|e| ImportError::Io(e.to_string()))?;
        let target = qdir.join(&detected.name);
        if target.exists() {
            return Err(ImportError::Conflict(format!(
                "a quarantined plugin named {:?} already exists",
                detected.name
            )));
        }
        let live_clash = data_dir.join("plugins").join(&detected.name).exists()
            || data_dir.join("extensions").join(&detected.name).exists();
        if live_clash {
            return Err(ImportError::Conflict(format!(
                "a live plugin named {:?} already exists",
                detected.name
            )));
        }
        std::fs::rename(stage, &target).map_err(|e| ImportError::Io(format!("quarantine: {e}")))?;

        std::fs::write(
            target.join(".scan-report.json"),
            serde_json::to_string_pretty(&scan.to_json()).unwrap_or_default(),
        )
        .map_err(|e| ImportError::Io(e.to_string()))?;
        ImportMeta {
            name: detected.name.clone(),
            kind: detected.kind.to_string(),
            version: detected.version.clone(),
            description: detected.description.clone(),
            source: source.to_string(),
            source_ref: source_ref.to_string(),
            imported_at: now_unix(),
        }
        .write_to(&target)
        .map_err(|e| ImportError::Io(e.to_string()))?;

        let mut notes = detected.notes;
        if stats.skipped_unsafe > 0 {
            notes.push(format!(
                "skipped {} unsafe tarball entries (parent-dir, absolute, or symlink)",
                stats.skipped_unsafe
            ));
        }
        match scan.verdict {
            "malicious" => notes.push(
                "scan verdict MALICIOUS: approval is blocked; remove it from quarantine manually if this is a false positive."
                    .to_string(),
            ),
            "suspicious" => notes.push(
                "scan verdict SUSPICIOUS: approving requires {\"acknowledge_risk\": true}."
                    .to_string(),
            ),
            _ => {}
        }

        Ok(ImportReport {
            name: detected.name,
            kind: detected.kind,
            version: detected.version,
            source,
            source_ref: source_ref.to_string(),
            capabilities: detected.capabilities,
            notes,
            scan,
        })
    }

    // ---------------------------------------------------------------------------
    // Tests (no network: every fetch is a stub behind the FetchFn trait object)
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{Cursor, Write};
        use std::sync::atomic::{AtomicU64, Ordering};

        static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

        fn temp_data_dir() -> std::path::PathBuf {
            let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "pantheon-plugin-import-test-{}-{}",
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp data dir");
            dir
        }

        /// Minimal plugin bundle: a root `manifest.yaml` (native tool plugin).
        fn fixture_zip() -> Vec<u8> {
            let mut buf = Cursor::new(Vec::new());
            {
                let mut zip = zip::ZipWriter::new(&mut buf);
                zip.start_file("manifest.yaml", zip::write::SimpleFileOptions::default())
                    .expect("start manifest.yaml");
                zip.write_all(b"name: my-plugin\ndescription: fixture\nversion: 1.2.3\n")
                    .expect("write manifest.yaml");
                zip.finish().expect("finish zip");
            }
            buf.into_inner()
        }

        /// P1 #3 (server side of the contract): the value the TUI now sends as
        /// `{url}` - `clawhub:<slug>` - must resolve through the ClawHub
        /// packages API instead of 400ing with "url is required". Fetch is
        /// stubbed; no network.
        #[test]
        fn clawhub_slug_in_url_field_resolves() {
            let zip_bytes = fixture_zip();
            let fetch = move |url: &str| -> Result<Vec<u8>, FetchError> {
                match url {
                    "https://clawhub.ai/api/v1/packages/my-plugin" => {
                        Ok(br#"{"package":{"latestVersion":"1.2.3"}}"#.to_vec())
                    }
                    "https://clawhub.ai/api/v1/packages/my-plugin/download?version=1.2.3" => {
                        Ok(zip_bytes.clone())
                    }
                    other => Err(FetchError::Status(404, other.to_string())),
                }
            };
            let dir = temp_data_dir();
            let report = run_import(&dir, "clawhub:owner/my-plugin", None, &fetch)
                .expect("clawhub:<slug> in the url field must resolve");
            assert_eq!(report.source, "clawhub");
            assert_eq!(report.name, "my-plugin");
            assert_eq!(report.source_ref, "clawhub:owner/my-plugin@1.2.3");
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Cluster 1 (f): converted foreign manifests must write flat
        /// `plugin.yaml` keys. Before the fix the converter wrote dotted
        /// keys (`"meta.name"`), which the manifest parser files under
        /// `extra` while the required `name` field stays missing - so the
        /// manifest failed to parse and the promoted plugin was silently
        /// unloaded. Both converters (OpenClaw and Claude Code) share
        /// `write_converted_manifest`, so both are covered here.
        #[test]
        fn converted_openclaw_manifest_parses_with_flat_keys() {
            let root = temp_data_dir();
            std::fs::write(
                root.join("openclaw.plugin.json"),
                r#"{"id": "demo-claw", "name": "demo-claw", "description": "d", "version": "2.0.1"}"#,
            )
            .unwrap();
            let detected = detect_format(&root).expect("openclaw fixture must detect");
            assert_eq!(detected.name, "demo-claw");
            let m = pantheon_extensions::manifest::PluginManifest::load(&root.join("plugin.yaml"))
                .expect("converted plugin.yaml must parse");
            assert_eq!(m.name, "demo-claw");
            assert_eq!(m.version, "2.0.1");
            let raw = std::fs::read_to_string(root.join("plugin.yaml")).unwrap();
            assert!(
                !raw.contains("meta.name"),
                "dotted key leaked back in: {raw}"
            );
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn converted_claude_manifest_parses_with_flat_keys() {
            let root = temp_data_dir();
            let dir = root.join(".claude-plugin");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("plugin.json"),
                r#"{"name": "demo-claude", "description": "d", "version": "0.3.0", "hooks": {}}"#,
            )
            .unwrap();
            let detected = detect_format(&root).expect("claude fixture must detect");
            assert_eq!(detected.name, "demo-claude");
            let m = pantheon_extensions::manifest::PluginManifest::load(&root.join("plugin.yaml"))
                .expect("converted plugin.yaml must parse");
            assert_eq!(m.name, "demo-claude");
            assert_eq!(m.version, "0.3.0");
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}

pub mod mcp {
    //! MCP server management: the dashboard half of the Tools & MCPs view.
    //!
    //! ## One enablement state
    //!
    //! The config file is the source of truth. `[mcp.servers.<name>]` with
    //! `enabled` (default false) is read and written identically by the
    //! config file, the dashboard (`GET /api/mcp/servers`,
    //! `POST /api/mcp/servers/:name/{enable,disable,toggle}`), the mobile
    //! app (which talks to the same dashboard endpoints), and the agent's
    //! `enable_mcp` tool. Migration declaration files
    //! (`<data_dir>/mcp/*.json`) are legacy: they are still *read* (a name
    //! the config section does not define keeps its declaration), but every
    //! write - add, enable, disable, toggle, delete - lands in `config.toml`.
    //! A config entry shadows a declaration of the same name everywhere.
    //!
    //! ## Which gate wins: `enabled` vs approval
    //!
    //! Two different gates, two different questions:
    //!
    //! - `enabled` (this file's flag) is the **operator's switch**: is this
    //!   server supposed to run? Fresh installs enable zero servers; every
    //!   enable is explicit.
    //! - The approval store (`<data_dir>/mcp/.approvals.json`,
    //!   `pantheon_api::approval`) is **consent to run third-party code**:
    //!   it binds the server's name to the content hash of its binary (or
    //!   URL). An unapproved custom server never connects no matter what the
    //!   flag says.
    //!
    //! A server launches iff it is enabled AND (bundled OR approved).
    //! Bundled catalog servers are first-party: they skip the consent store,
    //! so for them the `enabled` flag is the only gate. Disabling flips the
    //! switch only - a recorded approval persists, so re-enabling the same
    //! binary/URL resumes without re-consent; if the binary or URL changes,
    //! the approval lapses and consent is asked again.
    //!
    //! ## Custom servers: validated, never handshaked here
    //!
    //! `POST /api/mcp/servers` validates the *shape* without a live
    //! handshake: a stdio command must resolve on PATH (or be an existing
    //! path), an sse/http URL must parse (http/https scheme, non-empty
    //! host). No process is spawned, no TCP connection is opened - that is
    //! `pantheon doctor`'s job, and the `test` probe's. New servers are
    //! added disabled; consent (for custom servers) and enabling are
    //! separate explicit acts.
    //!
    //! This surface never launches a server for browsing. A full handshake
    //! happens on the `approve` endpoint below (which connects exactly like
    //! `pantheon mcp approve` and records name + reported version + hash),
    //! and `pantheon doctor`'s `test` probe. New servers are added disabled;
    //! consent (for custom servers) and enabling are separate explicit acts.
    //!
    //! ## Catalog source
    //!
    //! Bundled-catalog types come from the canonical
    //! `pantheon_api::mcp_catalog` - consumed directly, never redefined here.
    //! (`pantheon_api::mcp_catalog` remains only for `pantheon-tools`' agent
    //! `enable_mcp` tool, which cannot depend on `pantheon-mcp` without a
    //! dependency cycle.)

    use crate::{bad_json, body_json, err_json, json_ok, App};
    use pantheon_api::approval::{self, ApprovalStore};
    use pantheon_api::mcp_catalog;
    use pantheon_gateway::http::{Request, Response};

    /// Bundled-catalog membership: the canonical catalog in
    /// `pantheon_api::mcp_catalog` is the only list of first-party servers.
    /// Bundled servers are first-party - the `enabled` flag is their only
    /// gate - so this exemption mirrors the launcher's manager and the
    /// dashboard's "enabled AND (bundled OR approved)" model.
    fn is_bundled(name: &str) -> bool {
        mcp_catalog::find(name).is_some()
    }

    /// Canonical-recipe comparison, mirroring the manager's
    /// `is_canonical_bundled`: a config entry counts as bundled only when
    /// it matches the catalog recipe exactly (transport, command, args,
    /// env, url). A name-colliding custom entry is a custom server, not
    /// first-party, even though it borrows a bundled name.
    fn is_canonical_bundled_entry(name: &str, e: &pantheon_api::config::McpServerEntry) -> bool {
        let Some(recipe) = mcp_catalog::find(name) else {
            return false;
        };
        let canonical = recipe.to_config_entry();
        canonical.transport == e.transport
            && canonical.command == e.command
            && canonical.args == e.args
            && canonical.env == e.env
            && canonical.url == e.url
    }
    use pantheon_migration::{read_mcp_declarations, server_readiness, McpServer};
    use std::collections::HashMap;
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    fn mcp_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("mcp")
    }

    fn config_path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.toml")
    }

    /// The launcher's live snapshot, keyed by server name. `None` when no
    /// session has written one yet.
    fn live_map(data_dir: &Path) -> HashMap<String, serde_json::Value> {
        let mut out = HashMap::new();
        let text = std::fs::read_to_string(mcp_dir(data_dir).join("live.json")).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if let Some(arr) = v.get("servers").and_then(|s| s.as_array()) {
            for s in arr {
                if let Some(name) = s.get("name").and_then(|n| n.as_str()) {
                    out.insert(name.to_string(), s.clone());
                }
            }
        }
        out
    }

    /// Approved server names (names only - records never leave the vault path).
    fn approved_names(data_dir: &Path) -> Vec<String> {
        ApprovalStore::open(&mcp_dir(data_dir)).names()
    }

    fn safe_name(name: &str) -> bool {
        !(name.trim().is_empty()
            || name.len() > 64
            || name.contains('/')
            || name.contains('\\')
            || name.contains(".."))
    }

    /// Parse `config.toml` as a TOML document for surgical edits. A missing
    /// file is an empty document; a file that exists but does not parse is
    /// an error (fail closed - never guess at enablement).
    fn read_config_doc(data_dir: &Path) -> Result<toml::Value, String> {
        let path = config_path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(toml::Value::Table(toml::map::Map::new()))
            }
            Err(e) => Err(format!("read {}: {e}", path.display())),
        }
    }

    /// Land an edited document onto `config.toml` without losing
    /// comments, blank lines, or key order elsewhere: the change set
    /// between the file's current document and the edited one is applied
    /// segment by segment to the raw text (server names are literal keys,
    /// so dots inside a name are not path separators). Only if that
    /// preserving edit cannot land does the write fall back to the valid
    /// re-serialization, which loses comments but never corrupts.
    fn write_config_doc(data_dir: &Path, doc: &toml::Value) -> Result<(), String> {
        let path = config_path(data_dir);
        let raw = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        let old: toml::Value = if raw.is_empty() {
            toml::Value::Table(toml::map::Map::new())
        } else {
            toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?
        };
        let text = match crate::config::write_doc_preserving(&raw, &old, doc) {
            Ok(text) => text,
            Err(_) => toml::to_string_pretty(doc).map_err(|e| format!("serialize: {e}"))?,
        };
        crate::util::atomic_write(&path, &text)
    }

    /// The `[mcp.servers.<name>]` table, creating parents as needed.
    fn server_table<'a>(
        doc: &'a mut toml::Value,
        name: &str,
    ) -> Result<&'a mut toml::map::Map<String, toml::Value>, String> {
        // The fixed-prefix walk is the shared `parent_of`; the server name is
        // appended as a literal leaf key because it may itself contain dots,
        // which must not be treated as path separators.
        let (parent, leaf) = crate::util::parent_of(doc, "mcp.servers")?;
        let servers = parent
            .entry(leaf)
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let servers_table = servers
            .as_table_mut()
            .ok_or_else(|| "config has a non-table [mcp.servers] value".to_string())?;
        let entry = servers_table
            .entry(name.to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        entry
            .as_table_mut()
            .ok_or_else(|| format!("[mcp.servers.{name}] is not a table"))
    }

    /// Fill an empty `[mcp.servers.<name>]` table from a shape. Never
    /// overwrites keys the operator already set: materialization only fills
    /// gaps.
    fn materialize_shape(table: &mut toml::map::Map<String, toml::Value>, shape: &ServerShape) {
        if !table.contains_key("transport") {
            table.insert(
                "transport".to_string(),
                toml::Value::String(shape.transport.clone()),
            );
        }
        if let Some(command) = &shape.command {
            if !table.contains_key("command") {
                table.insert("command".to_string(), toml::Value::String(command.clone()));
            }
        }
        if !shape.args.is_empty() && !table.contains_key("args") {
            table.insert(
                "args".to_string(),
                toml::Value::Array(
                    shape
                        .args
                        .iter()
                        .map(|a| toml::Value::String(a.clone()))
                        .collect(),
                ),
            );
        }
        if let Some(url) = &shape.url {
            if !table.contains_key("url") {
                table.insert("url".to_string(), toml::Value::String(url.clone()));
            }
        }
        if !shape.requires_env.is_empty() && !table.contains_key("env") {
            let mut env_table = toml::map::Map::new();
            for name in &shape.requires_env {
                env_table.insert(name.clone(), toml::Value::String(format!("env:{name}")));
            }
            table.insert("env".to_string(), toml::Value::Table(env_table));
        }
    }

    /// The spawn-relevant shape of a server, from any source (catalog,
    /// config, declaration).
    struct ServerShape {
        transport: String,
        command: Option<String>,
        args: Vec<String>,
        url: Option<String>,
        requires_env: Vec<String>,
    }

    impl ServerShape {
        fn from_catalog(s: &mcp_catalog::BundledMcpServer) -> Self {
            ServerShape {
                transport: s.transport.clone(),
                command: s.command.clone(),
                args: s.args.clone(),
                url: s.url.clone(),
                // Secret *names* the recipe needs; values are never stored.
                requires_env: s.requires_env.clone(),
            }
        }

        fn from_config(e: &pantheon_api::config::McpServerEntry) -> Self {
            ServerShape {
                transport: e.transport.clone(),
                command: e.command.clone(),
                args: e.args.clone(),
                url: e.url.clone(),
                requires_env: e.env.keys().cloned().collect(),
            }
        }

        fn from_declaration(s: &McpServer) -> Self {
            ServerShape {
                transport: s.transport.clone(),
                command: s.command.clone(),
                args: s.args.clone(),
                url: s.url.clone(),
                requires_env: s.requires_env.clone(),
            }
        }

        /// Shape problems, without any live handshake: stdio needs a
        /// command, remote transports need a URL, and the transport must be
        /// known. This is the add-time validation.
        fn problems(&self, name: &str) -> Vec<String> {
            let mut out = Vec::new();
            match self.transport.as_str() {
                "stdio" => {
                    if self
                        .command
                        .as_deref()
                        .map(str::trim)
                        .unwrap_or("")
                        .is_empty()
                    {
                        out.push(format!(
                            "mcp.servers.{name}: transport is stdio but no command is set"
                        ));
                    } else if resolve_command(self.command.as_deref().unwrap_or("")).is_none() {
                        out.push(format!(
                            "mcp.servers.{name}: command {:?} does not resolve on PATH",
                            self.command.as_deref().unwrap_or("")
                        ));
                    }
                    if self.url.is_some() {
                        out.push(format!(
                            "mcp.servers.{name}: url is ignored for stdio transport"
                        ));
                    }
                }
                "sse" | "http" => {
                    match self.url.as_deref() {
                        Some(url) if url_shape_ok(url) => {}
                        _ => out.push(format!(
                            "mcp.servers.{name}: transport is {} but no valid url is set (want http(s)://host/...)",
                            self.transport
                        )),
                    }
                    if self.command.is_some() {
                        out.push(format!(
                            "mcp.servers.{name}: command is ignored for {} transport",
                            self.transport
                        ));
                    }
                }
                other => out.push(format!(
                    "mcp.servers.{name}: unknown transport {other:?} (stdio|sse|http)"
                )),
            }
            out
        }
    }

    /// URL shape check without any network: http/https scheme and a
    /// non-empty host. DNS and TCP are `test`'s / `pantheon doctor`'s job.
    fn url_shape_ok(url: &str) -> bool {
        let url = url.trim();
        let after = match url.split("://").next() {
            Some("http") | Some("https") => url.split("://").nth(1).unwrap_or(""),
            _ => return false,
        };
        let host = after.split('/').next().unwrap_or("");
        let host = host.rsplit('@').next().unwrap_or("");
        !host.trim().is_empty() && !host.contains(char::is_whitespace)
    }

    /// True when `command` is a launcher shim rather than the server
    /// itself: the program on the command line fetches (npx/uvx/bunx) or
    /// runs (docker/podman) a packaged server instead of being the server
    /// code. Mirrors `pantheon_mcp::manager`'s fingerprint - the two must
    /// stay in lockstep, because the approve endpoint records the hash and
    /// the launcher's approval gate checks it.
    fn is_launcher_shim(command: &str) -> bool {
        let base = Path::new(command)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(command);
        matches!(base, "npx" | "bunx" | "pnpx" | "uvx" | "docker" | "podman")
    }

    /// Content hash the approval binds, mirroring
    /// `pantheon_mcp::manager`'s fingerprint: the server binary (plus script
    /// args) for stdio - except when the command is a launcher shim
    /// (`npx`, `uvx`, `docker`, ...), where the hash covers the command
    /// plus the full argument list instead, so a pin bump or an arg change
    /// lapses the approval - or the endpoint URL for remote transports.
    /// Computed locally - no process spawned, no connection opened.
    ///
    /// Honest limit: for launcher shims this binds the *requested* package
    /// spec, not the bytes the registry served - registry-fetched code can
    /// change under a pin, so the approval binds intent, not bytes.
    fn content_hash(shape: &ServerShape) -> Result<String, String> {
        match shape.transport.as_str() {
            "stdio" => {
                let cmd = shape.command.as_deref().unwrap_or("");
                let mut bytes = Vec::new();
                if is_launcher_shim(cmd) {
                    // Bind what determines the code, not the shim binary.
                    bytes.extend_from_slice(b"mcp-launcher-shim\0");
                    bytes.extend_from_slice(cmd.as_bytes());
                    for a in &shape.args {
                        bytes.push(0);
                        bytes.extend_from_slice(a.as_bytes());
                    }
                    return Ok(approval::bytes_hash(&bytes));
                }
                match resolve_command(cmd) {
                    Some(p) => match std::fs::read(&p) {
                        Ok(b) => bytes.extend_from_slice(&b),
                        Err(e) => {
                            return Err(format!("hash {}: {e}", p.display()));
                        }
                    },
                    None => {
                        bytes.extend_from_slice(cmd.as_bytes());
                        bytes.push(0);
                    }
                }
                for a in &shape.args {
                    let p = Path::new(a);
                    if p.is_file() {
                        if let Ok(b) = std::fs::read(p) {
                            bytes.push(0);
                            bytes.extend_from_slice(&b);
                        }
                    }
                }
                Ok(approval::bytes_hash(&bytes))
            }
            "sse" | "http" => {
                let url = shape.url.as_deref().unwrap_or("");
                Ok(approval::bytes_hash(url.as_bytes()))
            }
            other => Err(format!("unknown transport {other:?}")),
        }
    }

    fn server_json(
        s: &McpServer,
        live: &HashMap<String, serde_json::Value>,
        approved: &[String],
    ) -> serde_json::Value {
        serde_json::json!({
            "name": s.name,
            "transport": s.transport,
            "command": s.command,
            "args": s.args,
            "url": s.url,
            "requires_env": s.requires_env,
            "needs_credentials": s.needs_credentials,
            "enabled": s.enabled,
            "bundled": false,
            "readiness": server_readiness(s),
            "approved": approved.iter().any(|a| a == &s.name),
            "health": live.get(&s.name).cloned().unwrap_or(serde_json::Value::Null),
        })
    }

    /// A config-section server rendered in the same shape as a declaration
    /// server, so the UI shows one merged view.
    fn config_server_json(
        name: &str,
        e: &pantheon_api::config::McpServerEntry,
        live: &HashMap<String, serde_json::Value>,
        approved: &[String],
    ) -> serde_json::Value {
        let bundled = is_canonical_bundled_entry(name, e);
        serde_json::json!({
            "name": name,
            "transport": e.transport,
            "command": e.command,
            "args": e.args,
            "url": e.url,
            "requires_env": e.env.keys().cloned().collect::<Vec<_>>(),
            "needs_credentials": false,
            "enabled": e.enabled,
            "bundled": bundled,
            "readiness": e.problems(name).into_iter().next(),
            // Only a canonical bundled recipe is first-party: no consent
            // record needed, the enabled flag is the only gate. A
            // name-colliding custom entry needs approval like any other.
            "approved": bundled || approved.iter().any(|a| a == name),
            "health": live.get(name).cloned().unwrap_or(serde_json::Value::Null),
        })
    }

    /// A bundled catalog server with no config entry yet: always disabled,
    /// first-party (no consent record needed).
    fn catalog_server_json(s: &mcp_catalog::BundledMcpServer) -> serde_json::Value {
        let shape = ServerShape::from_catalog(s);
        let mut readiness = shape.problems(&s.name).into_iter().next();
        // Surface missing secrets as the readiness blocker: the row is
        // pinned and shape-valid, so env is the usual reason it cannot run.
        if readiness.is_none() {
            if let Some(missing) = s.requires_env.iter().find(|n| std::env::var(n).is_err()) {
                readiness = Some(format!("env {missing} is not set"));
            }
        }
        let mut privilege_notes = s.privilege_notes.clone();
        if let Some(w) = &s.deprecated_warning {
            privilege_notes.push_str("\n\nDeprecation warning: ");
            privilege_notes.push_str(w);
        }
        serde_json::json!({
            "name": s.name.clone(),
            "transport": s.transport.clone(),
            "command": s.command.clone(),
            "args": s.args.clone(),
            "url": s.url.clone(),
            "requires_env": s.requires_env.clone(),
            "needs_credentials": !s.requires_env.is_empty(),
            "enabled": false,
            "bundled": true,
            // Package-only pin: unpinned (remote) recipes get no stamp.
            "pinned_version": s.package.clone(),
            "description": s.description.clone(),
            "privilege_notes": privilege_notes,
            "readiness": readiness,
            "approved": true,
            "health": serde_json::Value::Null,
        })
    }

    /// Config-defined servers, primary over declarations.
    fn config_servers(data_dir: &Path) -> Vec<(String, pantheon_api::config::McpServerEntry)> {
        let raw = std::fs::read_to_string(config_path(data_dir)).unwrap_or_default();
        let cfg: Result<pantheon_api::config::Config, _> = toml::from_str(&raw);
        cfg.map(|c| {
            c.mcp
                .map(|m| m.servers.into_iter().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// `GET /api/mcp/servers` - bundled catalog entries, config-section
    /// servers, and legacy declarations, each merged with its live health
    /// snapshot and approval state. The config entry is the enablement
    /// state; declarations shadowed by the config show under the config
    /// group instead of twice.
    pub fn list(app: &App) -> Response {
        json_ok(list_json(app))
    }

    fn list_json(app: &App) -> serde_json::Value {
        let decls = read_mcp_declarations(&app.data_dir);
        let live = live_map(&app.data_dir);
        let approved = approved_names(&app.data_dir);
        let cfg_servers = config_servers(&app.data_dir);
        let config_names: Vec<&str> = cfg_servers.iter().map(|(n, _)| n.as_str()).collect();
        let catalog = mcp_catalog::bundled_catalog();
        let bundled_group: Vec<serde_json::Value> = catalog
            .iter()
            .filter(|s| !config_names.contains(&s.name.as_str()))
            .map(catalog_server_json)
            .collect();
        let mut out: Vec<serde_json::Value> = Vec::new();
        if !bundled_group.is_empty() {
            out.push(serde_json::json!({
                "source": "bundled catalog",
                "origin": "bundled",
                "note": "first-party servers, all disabled by default; enabling materializes the pinned row into config.toml",
                "servers": bundled_group,
            }));
        }
        let cfg_group: Vec<serde_json::Value> = cfg_servers
            .iter()
            .map(|(n, e)| config_server_json(n, e, &live, &approved))
            .collect();
        if !cfg_group.is_empty() {
            out.push(serde_json::json!({
                "source": "config.toml",
                "origin": "config",
                "note": "the enablement state: this is what the dashboard, the mobile app, and the agent's enable_mcp tool all read and write",
                "servers": cfg_group,
            }));
        }
        let decl_groups: Vec<serde_json::Value> = decls
            .iter()
            .map(|d| {
                // Declarations shadowed by the config section show under the
                // config group instead of twice.
                let servers: Vec<serde_json::Value> = d
                    .servers
                    .iter()
                    .filter(|s| !config_names.contains(&s.name.as_str()))
                    .map(|s| server_json(s, &live, &approved))
                    .collect();
                serde_json::json!({
                    "source": d.source,
                    "origin": "declaration",
                    "note": "legacy: read-only here; enabling a declaration server materializes it into config.toml",
                    "servers": servers,
                })
            })
            .filter(|g| !g["servers"].as_array().map(|a| a.is_empty()).unwrap_or(true))
            .collect();
        out.extend(decl_groups);
        let pending: Vec<&str> = live
            .values()
            .filter(|s| s.get("status").and_then(|v| v.as_str()) == Some("unapproved"))
            .filter_map(|s| s.get("name").and_then(|v| v.as_str()))
            .collect();
        serde_json::json!({
            "servers": out,
            "pending_approval": pending,
            "note": "a server launches iff enabled AND (bundled OR approved); live health comes from the session launcher's mcp/live.json snapshot",
        })
    }

    /// `GET /api/mcp/health` - the launcher's live snapshot verbatim
    /// (status, tool counts, connects, failures, backoff errors), plus a
    /// freshness note. No session has run yet = empty servers with a note.
    pub fn health(app: &App) -> Response {
        let text =
            std::fs::read_to_string(mcp_dir(&app.data_dir).join("live.json")).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({}));
        let empty = v
            .get("servers")
            .and_then(|s| s.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true);
        json_ok(serde_json::json!({
            "live": v,
            "note": if empty {
                "no live MCP state yet - start a session to launch servers"
            } else {
                "snapshot written by the session's MCP manager"
            },
        }))
    }

    /// Find a server's shape from any source: config first, then the
    /// bundled catalog, then declarations.
    fn find_shape(data_dir: &Path, name: &str) -> Option<(ServerShape, &'static str)> {
        if let Some((_, e)) = config_servers(data_dir)
            .into_iter()
            .find(|(n, _)| n == name)
        {
            return Some((ServerShape::from_config(&e), "config"));
        }
        if let Some(s) = mcp_catalog::find(name) {
            return Some((ServerShape::from_catalog(&s), "bundled"));
        }
        for d in read_mcp_declarations(data_dir) {
            if let Some(s) = d.servers.iter().find(|s| s.name == name) {
                return Some((ServerShape::from_declaration(s), "declaration"));
            }
        }
        None
    }

    /// Write the bundled-catalog enablement state. Materialization funnels
    /// through the canonical writer
    /// [`pantheon_api::mcp_catalog::materialize_recipe_table`] - the same
    /// writer the agent's `enable_mcp` tool uses - so both surfaces produce
    /// byte-identical tables and first-writer-wins divergence is impossible.
    /// This is the no-arbitrary-command boundary: only catalog names are
    /// ever written. An empty table is materialized from the canonical
    /// recipe (disabled by default), so the launcher never sees a bare
    /// `enabled = true` with no command; keys the operator already set are
    /// left alone.
    fn write_bundled_enabled(data_dir: &Path, name: &str, enabled: bool) -> Result<(), String> {
        let recipe = mcp_catalog::find(name)
            .ok_or_else(|| format!("'{name}' is not a bundled MCP server"))?;
        let mut doc = read_config_doc(data_dir)?;
        let table = server_table(&mut doc, name)?;
        if table.is_empty() {
            mcp_catalog::materialize_recipe_table(table, &recipe);
        }
        table.insert("enabled".to_string(), toml::Value::Boolean(enabled));
        write_config_doc(data_dir, &doc)?;
        Ok(())
    }

    /// Write the ONE enablement state: `[mcp.servers.<name>].enabled` in
    /// `config.toml`. A name unknown to the config, the bundled catalog,
    /// and the declarations is a 404 - there is no path that enables an
    /// arbitrary command. Enabling a bundled or declaration-only server
    /// materializes its full shape into the config table first, so the
    /// launcher never sees a bare `enabled = true` with no command.
    fn write_enabled(data_dir: &Path, name: &str, enabled: bool) -> Result<&'static str, String> {
        if !safe_name(name) {
            return Err("unsafe server name".to_string());
        }
        // Bundled names funnel through the sibling catalog: the pinned row
        // is the only shape the agent, dashboard, and app may ever write.
        if is_bundled(name) {
            write_bundled_enabled(data_dir, name, enabled)?;
            return Ok("bundled");
        }
        let mut doc = read_config_doc(data_dir)?;
        let table = server_table(&mut doc, name)?;
        if table.is_empty() {
            // Declaration-only server: materialize its shape so the config
            // entry is complete on its own.
            let mut shape_opt = None;
            for d in read_mcp_declarations(data_dir) {
                if let Some(s) = d.servers.iter().find(|s| s.name == name) {
                    shape_opt = Some(ServerShape::from_declaration(s));
                    break;
                }
            }
            match shape_opt {
                Some(shape) => materialize_shape(table, &shape),
                None => return Err("no server by that name".to_string()),
            }
        }
        table.insert("enabled".to_string(), toml::Value::Boolean(enabled));
        write_config_doc(data_dir, &doc)?;
        Ok("config")
    }

    /// `POST /api/mcp/servers/:name/enable` (or `/disable`, `/toggle`).
    /// Writes the config enablement state; the response is the refreshed
    /// server list.
    pub fn set_enabled(app: &App, name: &str, enabled: bool) -> Response {
        match write_enabled(&app.data_dir, name, enabled) {
            Ok(origin) => {
                let mut body = list_json(app);
                body["toggled"] =
                    serde_json::json!({"name": name, "enabled": enabled, "via": origin});
                json_ok(body)
            }
            Err(e) if e == "no server by that name" => err_json(404, "NOT_FOUND", &e),
            Err(e) => bad_json(&e),
        }
    }

    /// `POST /api/mcp/servers/:name/toggle` - flip the effective enabled
    /// state, whatever source currently defines it.
    pub fn toggle(app: &App, name: &str) -> Response {
        if !safe_name(name) {
            return bad_json("unsafe server name");
        }
        let currently = config_servers(&app.data_dir)
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, e)| e.enabled)
            .unwrap_or_else(|| {
                // No config entry yet: bundled catalog entries are disabled
                // by default; declaration servers keep their file flag.
                if is_bundled(name) {
                    false
                } else {
                    read_mcp_declarations(&app.data_dir)
                        .iter()
                        .flat_map(|d| &d.servers)
                        .find(|s| s.name == name)
                        .map(|s| s.enabled)
                        .unwrap_or(false)
                }
            });
        set_enabled(app, name, !currently)
    }

    fn parse_new_server(body: &serde_json::Value) -> Result<ServerShape, String> {
        let name = body
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "name is required".to_string())?;
        if !safe_name(name) {
            return Err("unsafe server name".to_string());
        }
        let transport = body
            .get("transport")
            .and_then(|v| v.as_str())
            .unwrap_or("stdio");
        if !matches!(transport, "stdio" | "http" | "sse") {
            return Err("transport must be stdio, http, or sse".to_string());
        }
        let command = body
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let url = body
            .get("url")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let args: Vec<String> = body
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let requires_env: Vec<String> = body
            .get("requires_env")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let shape = ServerShape {
            transport: transport.to_string(),
            command,
            args,
            url,
            requires_env,
        };
        // Validate the shape WITHOUT a live handshake: a stdio command must
        // resolve on PATH (or be an existing path); an sse/http URL must
        // parse. No process is spawned and no connection is opened - deeper
        // verification is `pantheon doctor`'s job.
        if let Some(problem) = shape.problems(name).into_iter().next() {
            return Err(problem);
        }
        Ok(shape)
    }

    /// `POST /api/mcp/servers` - `{name, transport, command?, args?, url?,
    /// requires_env?, confirm: true}`.
    ///
    /// Custom servers are added **disabled** into `[mcp.servers.<name>]` in
    /// `config.toml` (the one enablement state). The shape is validated
    /// without a live handshake; a name the bundled catalog already owns is
    /// rejected (enable the catalog entry instead of shadowing it).
    pub fn add(app: &App, req: &Request) -> Response {
        let body = match body_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        if !body
            .get("confirm")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return bad_json("add requires confirm: true");
        }
        let name = body
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if !safe_name(name) {
            return bad_json("name is required");
        }
        if is_bundled(name) {
            return bad_json(&format!(
                "'{name}' is a bundled server: enable the catalog entry instead of adding a custom one"
            ));
        }
        if find_shape(&app.data_dir, name).is_some() {
            return bad_json(&format!("a server named '{name}' already exists"));
        }
        let shape = match parse_new_server(&body) {
            Ok(s) => s,
            Err(e) => return bad_json(&e),
        };
        let mut doc = match read_config_doc(&app.data_dir) {
            Ok(d) => d,
            Err(e) => return err_json(500, "MCP", &e),
        };
        let table = match server_table(&mut doc, name) {
            Ok(t) => t,
            Err(e) => return err_json(500, "MCP", &e),
        };
        materialize_shape(table, &shape);
        // Disabled by default: consent and enabling are separate explicit acts.
        table.insert("enabled".to_string(), toml::Value::Boolean(false));
        if let Err(e) = write_config_doc(&app.data_dir, &doc) {
            return err_json(500, "MCP", &e);
        }
        let mut body = list_json(app);
        body["added"] = serde_json::json!({"name": name, "enabled": false});
        json_ok(body)
    }

    /// Build the manager spec for dashboard approval: the config entry
    /// when one exists, else the legacy declaration (whose `requires_env`
    /// names become `env:` refs, exactly as the launcher's resolver does).
    /// `None` when the server is unknown to both, or its transport does
    /// not parse.
    fn build_approve_spec(
        data_dir: &Path,
        name: &str,
    ) -> Option<pantheon_mcp::manager::McpServerSpec> {
        use pantheon_mcp::manager::{McpAuth, McpServerSpec, McpTransport};
        if let Some((_, e)) = config_servers(data_dir)
            .into_iter()
            .find(|(n, _)| n == name)
        {
            return McpServerSpec::from_entry(name, &e);
        }
        for d in read_mcp_declarations(data_dir) {
            if let Some(s) = d.servers.iter().find(|s| s.name == name) {
                let transport = McpTransport::parse(s.transport.as_str())?;
                let mut env = HashMap::new();
                for var in &s.requires_env {
                    env.insert(var.clone(), format!("env:{var}"));
                }
                return Some(McpServerSpec {
                    name: name.to_string(),
                    transport,
                    command: s.command.clone(),
                    args: s.args.clone(),
                    env,
                    url: s.url.clone(),
                    enabled: s.enabled,
                    timeout: Duration::from_secs(30),
                    headers: HashMap::new(),
                    auth: McpAuth::default(),
                    scopes: Vec::new(),
                    client_id: None,
                });
            }
        }
        None
    }

    /// `POST /api/mcp/servers/:name/approve` - `{confirm: true}`.
    ///
    /// Record operator consent for a **custom** server in the unified
    /// approval store, binding the server's name to its content hash (the
    /// binary bytes, or the URL bytes for remote transports). The version
    /// the server reports on handshake is recorded for DISPLAY only: the
    /// launcher's gate is hash-bound, so a version string can never gate
    /// (or lapse) execution on its own - exactly like
    /// `pantheon mcp approve`. The handshake runs here (a process spawn
    /// for stdio, a connection for remote transports); a failed handshake
    /// records nothing and returns 502.
    ///
    /// Bundled servers are first-party and skip consent (409) - enabling is
    /// their only gate. Approval is consent, not enablement: an approved
    /// server still needs `enabled`.
    pub fn approve(app: &App, name: &str, req: &Request) -> Response {
        let body = match body_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        if !body
            .get("confirm")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return bad_json("approve requires confirm: true");
        }
        if !safe_name(name) {
            return bad_json("unsafe server name");
        }
        let (shape, origin) = match find_shape(&app.data_dir, name) {
            Some(found) => found,
            None => return err_json(404, "NOT_FOUND", "no server by that name"),
        };
        // Only a canonical bundled recipe skips consent (409). A bundled
        // name with a modified shape is a custom server: it goes through
        // approval here instead of being unapprovable via the dashboard
        // while the list claims approved:true.
        let canonical_bundled = origin == "bundled"
            || config_servers(&app.data_dir)
                .into_iter()
                .find(|(n, _)| n == name)
                .is_some_and(|(_, e)| is_canonical_bundled_entry(name, &e));
        if canonical_bundled {
            return err_json(
                409,
                "MCP_BUNDLED",
                "bundled servers are first-party: no consent record needed, enabling is the gate",
            );
        }
        if let Some(problem) = shape.problems(name).into_iter().next() {
            return bad_json(&format!("cannot approve an invalid server: {problem}"));
        }
        let hash = match content_hash(&shape) {
            Ok(h) => h,
            Err(e) => {
                return err_json(
                    400,
                    "MCP_UNHASHABLE",
                    &format!("cannot fingerprint this server: {e}"),
                )
            }
        };
        let _ = hash;
        let spec = match build_approve_spec(&app.data_dir, name) {
            Some(s) => s,
            None => return bad_json("cannot build a server spec for approval"),
        };
        // Handshake like `pantheon mcp approve`: connect, then record the
        // server's reported version (DISPLAY only) bound to its content
        // hash. The launcher's post-handshake gate is hash-bound, so a
        // version bump with identical content does not lapse consent;
        // only a content change does.
        let manager = pantheon_mcp::manager::McpManager::new(app.data_dir.clone());
        if let Ok(cfg) = crate::session_factory::load_config(&app.data_dir) {
            let broker = crate::session_factory::secrets_broker(cfg.as_ref());
            manager.set_secrets(broker.clone());
            let resolver_broker = broker;
            manager.set_env_resolver(std::sync::Arc::new(move |var: &str| {
                resolver_broker
                    .resolve(var)
                    .ok()
                    .flatten()
                    .map(|v| v.expose().to_owned())
                    .or_else(|| std::env::var(var).ok())
            }));
        }
        manager.configure(vec![spec]);
        let rec = match manager.approve_server(name) {
            Ok(rec) => rec,
            Err(e) => {
                manager.shutdown();
                return err_json(502, "MCP_APPROVE", &format!("handshake failed: {e}"));
            }
        };
        manager.shutdown();
        json_ok(serde_json::json!({
            "ok": true, "name": name, "approved": true,
            "version": rec.version,
            "note": "consent recorded; the server still needs to be enabled before it launches",
        }))
    }

    /// `DELETE /api/mcp/servers/:name` - remove a custom server. Config
    /// entries are deleted from `config.toml`; legacy declaration entries
    /// from their declaration file. Bundled catalog entries cannot be
    /// deleted (disable them instead).
    pub fn delete(app: &App, name: &str) -> Response {
        if !safe_name(name) {
            return bad_json("unsafe server name");
        }
        if is_bundled(name) {
            return err_json(
                409,
                "MCP_BUNDLED",
                "bundled servers cannot be deleted; disable the catalog entry instead",
            );
        }
        // Config entry first: it is the primary store.
        if let Ok(mut doc) = read_config_doc(&app.data_dir) {
            let removed = doc
                .as_table_mut()
                .and_then(|r| r.get_mut("mcp"))
                .and_then(|m| m.as_table_mut())
                .and_then(|m| m.get_mut("servers"))
                .and_then(|s| s.as_table_mut())
                .map(|t| t.remove(name).is_some())
                .unwrap_or(false);
            if removed {
                // Prune now-empty parents so the file does not accumulate
                // empty `[mcp.servers]` tables.
                if let Some(mcp) = doc
                    .as_table_mut()
                    .and_then(|r| r.get_mut("mcp"))
                    .and_then(|m| m.as_table_mut())
                {
                    let servers_empty = mcp
                        .get("servers")
                        .and_then(|s| s.as_table())
                        .map(|t| t.is_empty())
                        .unwrap_or(false);
                    if servers_empty {
                        mcp.remove("servers");
                    }
                    if mcp.is_empty() {
                        doc.as_table_mut().map(|r| r.remove("mcp"));
                    }
                }
                if let Err(e) = write_config_doc(&app.data_dir, &doc) {
                    return err_json(500, "MCP", &e);
                }
                return list(app);
            }
        }
        // Fall back to legacy declaration files.
        let decls = read_mcp_declarations(&app.data_dir);
        let mut touched: Option<String> = None;
        for d in &decls {
            if d.servers.iter().any(|s| s.name == name) {
                touched = Some(d.source.clone());
                break;
            }
        }
        match touched {
            Some(source) => rewrite_declaration(app, &source, |servers| {
                let before = servers.len();
                servers.retain(|s| s.name != name);
                if servers.len() == before {
                    return Err("server vanished".to_string());
                }
                Ok(())
            }),
            None => err_json(404, "NOT_FOUND", "no server by that name"),
        }
    }

    /// `POST /api/mcp/reload` - re-read the servers from disk.
    pub fn reload(app: &App) -> Response {
        list(app)
    }

    /// `POST /api/mcp/servers/:name/test` - a readiness check, NOT a
    /// handshake: for stdio, resolve the command on PATH (or as an
    /// absolute path); for http/sse, attempt a TCP connect to the URL's
    /// host:port with a 3s timeout. A crashing server still reports ok
    /// here because it is never spawned; use `pantheon mcp health` for a
    /// real handshake. The readiness blocker, if any, is reported
    /// alongside.
    pub fn test(app: &App, name: &str) -> Response {
        let (shape, origin) = match find_shape(&app.data_dir, name) {
            Some(found) => found,
            None => return err_json(404, "NOT_FOUND", "no server by that name"),
        };
        if let Some(blocker) = shape.problems(name).into_iter().next() {
            return json_ok(serde_json::json!({
                "ok": false, "name": name, "origin": origin,
                "detail": format!("not ready: {blocker}"),
            }));
        }
        let (ok, detail) = match shape.transport.as_str() {
            "stdio" => {
                let cmd = shape.command.as_deref().unwrap_or("");
                (
                    resolve_command(cmd).is_some(),
                    format!("command {cmd:?} found (readiness only, no handshake)"),
                )
            }
            _ => match shape.url.as_deref() {
                Some(url) => probe_url(url),
                None => (false, "no url".to_string()),
            },
        };
        json_ok(serde_json::json!({"ok": ok, "name": name, "origin": origin, "detail": detail}))
    }

    fn decl_path(data_dir: &Path, source: &str) -> Result<PathBuf, String> {
        if source.trim().is_empty()
            || source.contains('/')
            || source.contains('\\')
            || source.contains("..")
        {
            return Err(format!("unsafe declaration source {source:?}"));
        }
        let dir = mcp_dir(data_dir);
        let p = dir.join(format!("{source}.json"));
        if !p.starts_with(&dir) {
            return Err("invalid declaration path".to_string());
        }
        Ok(p)
    }

    fn atomic_write_json(path: &Path, v: &serde_json::Value) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Rewrite one legacy declaration file with `f` applied to its servers.
    /// Only used for deleting declaration-only servers; enablement writes
    /// go to `config.toml`.
    fn rewrite_declaration(
        app: &App,
        source: &str,
        f: impl FnOnce(&mut Vec<McpServer>) -> Result<(), String>,
    ) -> Response {
        let path = match decl_path(&app.data_dir, source) {
            Ok(p) => p,
            Err(e) => return bad_json(&e),
        };
        let mut servers: Vec<McpServer> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("servers").cloned())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        if let Err(e) = f(&mut servers) {
            return bad_json(&e);
        }
        let doc = serde_json::json!({
            "source": source,
            "note": "Managed by the Pantheon dashboard. Credentials are declared, not stored.",
            "servers": servers,
        });
        if let Err(e) = atomic_write_json(&path, &doc) {
            return err_json(500, "MCP", &format!("write: {e}"));
        }
        list(app)
    }

    fn resolve_command(cmd: &str) -> Option<PathBuf> {
        if cmd.is_empty() {
            return None;
        }
        let p = Path::new(cmd);
        if p.components().count() > 1 {
            // A path, not a bare name.
            return p.exists().then(|| p.to_path_buf());
        }
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(cmd))
                .find(|p| p.is_file())
        })
    }

    fn probe_url(url: &str) -> (bool, String) {
        // Minimal: scheme://host[:port][/...]. No regex crate, no URL crate.
        let after = url.split("://").nth(1).unwrap_or(url);
        let host_port = after.split('/').next().unwrap_or("");
        if host_port.is_empty() {
            return (false, "unparseable url".to_string());
        }
        // Bracketed IPv6 first: rsplit_once(':') on "[::1]" splits inside
        // the address, and a malformed port used to fall through to the
        // default port 80, probing the wrong service instead of erroring.
        let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
            match rest.split_once(']') {
                Some((h, tail)) => {
                    let port = match tail.strip_prefix(':') {
                        Some(p) => match p.parse::<u16>() {
                            Ok(port) => port,
                            Err(_) => {
                                return (false, format!("invalid port in url: {p}"));
                            }
                        },
                        None if tail.is_empty() => 80,
                        None => return (false, "unparseable url".to_string()),
                    };
                    (h.to_string(), port)
                }
                None => return (false, "unparseable url".to_string()),
            }
        } else {
            match host_port.rsplit_once(':') {
                Some((h, p)) => match p.parse::<u16>() {
                    Ok(port) => (h.to_string(), port),
                    Err(_) => return (false, format!("invalid port in url: {p}")),
                },
                None => (host_port.to_string(), 80),
            }
        };
        let host = host.as_str();
        let addrs: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
            Ok(a) => a.collect(),
            Err(e) => return (false, format!("DNS failed: {e}")),
        };
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
                Ok(_) => return (true, format!("connected to {addr}")),
                Err(_) => continue,
            }
        }
        (false, format!("no route to {host}:{port}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Scratch data dir without adding a dev-dependency: unique per
        /// test recipe, removed afterwards.
        fn scratch_dir(tag: &str) -> PathBuf {
            let p = std::env::temp_dir().join(format!(
                "pantheon-dash-mcp-test-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("scratch dir");
            p
        }

        fn read_server_table(data_dir: &Path, name: &str) -> toml::Value {
            let text = std::fs::read_to_string(config_path(data_dir)).expect("config written");
            let doc: toml::Value = toml::from_str(&text).expect("config parses");
            doc.get("mcp")
                .and_then(|m| m.get("servers"))
                .and_then(|s| s.get(name))
                .cloned()
                .expect("server table present")
        }

        /// #7: enabling the same bundled server through the dashboard write
        /// path and through the agent-tool write path must produce
        /// byte-identical `[mcp.servers.<name>]` tables - one canonical
        /// recipe, no first-writer-wins divergence. (Fails before the fix:
        /// the dashboard materializes the launcher copy - e.g. playwright
        /// without `-y`, notion unpinned - while the agent tool materializes
        /// the api copy.)
        #[test]
        fn bundled_enablement_paths_materialize_identically() {
            for recipe in pantheon_api::mcp_catalog::bundled_catalog() {
                let dash_dir = scratch_dir(&format!("dash-{}", recipe.name));
                let agent_dir = scratch_dir(&format!("agent-{}", recipe.name));
                write_bundled_enabled(dash_dir.as_path(), &recipe.name, true)
                    .expect("dashboard enable");
                pantheon_api::mcp_catalog::set_enabled(agent_dir.as_path(), &recipe.name, true)
                    .expect("agent enable");
                let dash_table = read_server_table(dash_dir.as_path(), &recipe.name);
                let agent_table = read_server_table(agent_dir.as_path(), &recipe.name);
                assert_eq!(
                    dash_table, agent_table,
                    "materialization diverged for {}",
                    recipe.name
                );
                let _ = std::fs::remove_dir_all(&dash_dir);
                let _ = std::fs::remove_dir_all(&agent_dir);
            }
        }

        /// Minimal newline-delimited JSON-RPC MCP server: answers
        /// `initialize` with a fixed reported version, ignores everything
        /// else. Enough for the approve handshake (which connects and
        /// reads `serverInfo.version`, nothing more).
        const FAKE_MCP_PY: &str = r#"
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    if msg.get("method") == "initialize":
        out = {"jsonrpc": "2.0", "id": msg.get("id"),
               "result": {"protocolVersion": "2025-06-18", "capabilities": {},
                          "serverInfo": {"name": "fake", "version": "9.9.9"}}}
        sys.stdout.write(json.dumps(out) + "\n")
        sys.stdout.flush()
    "#;

        fn scratch_app(data_dir: PathBuf) -> App {
            App {
                data_dir: data_dir.clone(),
                token: "test".to_string(),
                bind: "127.0.0.1".to_string(),
                bind_all: false,
                on_approval: None,
                send_locks: Default::default(),
                ui_action_locks: Default::default(),
                turn_children: Default::default(),
                swarm: crate::swarm::orchestrator_for(&data_dir),
                desktop: Default::default(),
            }
        }

        /// Cluster 1 (b): dashboard approve must handshake like CLI
        /// approve. Before the fix it recorded version "unverified"
        /// without connecting, and the launcher's post-handshake gate
        /// (exact reported version + hash) never matched - so a
        /// dashboard-approved server stayed unapproved with 0 tools.
        /// After the fix the record carries the handshaked version and
        /// passes the launcher's own `is_approved` check.
        #[test]
        fn dashboard_approve_handshakes_and_records_real_version() {
            let dir = scratch_dir("approve-handshake");
            let script = dir.join("fake_mcp.py");
            std::fs::write(&script, FAKE_MCP_PY).expect("write fake server");
            std::fs::write(
                dir.join("config.toml"),
                format!(
                    "[mcp.servers.fake]\nenabled = false\ntransport = \"stdio\"\ncommand = \"python3\"\nargs = [{}]\n",
                    serde_json::to_string(&script.to_string_lossy()).unwrap()
                ),
            )
            .expect("write config");
            let app = scratch_app(dir.clone());
            let req = Request {
                method: "POST".to_string(),
                path: "/api/mcp/servers/fake/approve".to_string(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: br#"{"confirm": true}"#.to_vec(),
            };
            let resp = approve(&app, "fake", &req);
            let (status, body) = match resp {
                Response::Buffered { status, body, .. } => (
                    status,
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default(),
                ),
                _ => panic!("expected a buffered response"),
            };
            assert_eq!(status, 200, "approve must handshake successfully: {body}");
            assert_eq!(body["approved"], serde_json::json!(true));
            assert_eq!(body["version"], serde_json::json!("9.9.9"));
            // The launcher's own gate must now pass: exact version + hash.
            let store = ApprovalStore::open(&mcp_dir(&dir));
            let rec = store.get("fake").expect("approval record written");
            assert_eq!(rec.version, "9.9.9");
            assert_ne!(rec.version, "unverified");
            assert!(!rec.dir_hash.is_empty());
            assert!(
                store.is_approved("fake", &rec.version, &rec.dir_hash),
                "record must pass the launcher's post-handshake gate"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Enabling a custom server through the dashboard write path must
        /// land in `config.toml` without deleting comments or unrelated
        /// sections (the preserving writer, not the old re-serialization).
        #[test]
        fn enable_preserves_comments_elsewhere() {
            let dir = scratch_dir("enable-comments");
            std::fs::write(
                config_path(&dir),
                "# Operator note\n[mcp.servers.myserver]\ncommand = \"/bin/true\"\nenabled = false\n\n[nightly]\nenabled = true\n",
            )
            .expect("seed config");
            write_enabled(dir.as_path(), "myserver", true).expect("enable");
            let text = std::fs::read_to_string(config_path(&dir)).expect("config written");
            assert!(
                text.contains("# Operator note"),
                "comment must survive: {text}"
            );
            assert!(
                text.contains("[nightly]"),
                "unrelated section must survive: {text}"
            );
            let table = read_server_table(dir.as_path(), "myserver");
            assert_eq!(table.get("enabled"), Some(&toml::Value::Boolean(true)));
            assert_eq!(
                table.get("command"),
                Some(&toml::Value::String("/bin/true".to_string()))
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A server name containing dots is ONE literal key, not a path:
        /// enabling it must not split it into nested tables.
        #[test]
        fn enable_dotted_server_name_stays_one_key() {
            let dir = scratch_dir("enable-dotted");
            std::fs::write(
                config_path(&dir),
                "[mcp.servers.\"my.server\"]\ncommand = \"/bin/true\"\nenabled = false\n",
            )
            .expect("seed config");
            write_enabled(dir.as_path(), "my.server", true).expect("enable");
            let table = read_server_table(dir.as_path(), "my.server");
            assert_eq!(table.get("enabled"), Some(&toml::Value::Boolean(true)));
            let text = std::fs::read_to_string(config_path(&dir)).expect("config written");
            assert!(
                !text.contains("[mcp.servers.my]"),
                "dotted name must not split into nested tables: {text}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Deleting one custom server must leave the other server, the
        /// comment, and unrelated sections intact.
        #[test]
        fn delete_preserves_comments_and_other_servers() {
            let dir = scratch_dir("delete-comments");
            std::fs::write(
                config_path(&dir),
                "# Operator note\n[mcp.servers.keep]\ncommand = \"/bin/true\"\nenabled = true\n\n[mcp.servers.drop]\ncommand = \"/bin/false\"\nenabled = false\n\n[nightly]\nenabled = true\n",
            )
            .expect("seed config");
            let app = scratch_app(dir.clone());
            let resp = delete(&app, "drop");
            let status = match resp {
                Response::Buffered { status, .. } => status,
                _ => panic!("expected a buffered response"),
            };
            assert_eq!(status, 200, "delete must succeed");
            let text = std::fs::read_to_string(config_path(&dir)).expect("config written");
            assert!(
                text.contains("# Operator note"),
                "comment must survive: {text}"
            );
            assert!(
                text.contains("[nightly]"),
                "unrelated section must survive: {text}"
            );
            assert!(
                !text.contains("drop"),
                "deleted server must be gone: {text}"
            );
            let table = read_server_table(dir.as_path(), "keep");
            assert_eq!(table.get("enabled"), Some(&toml::Value::Boolean(true)));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
