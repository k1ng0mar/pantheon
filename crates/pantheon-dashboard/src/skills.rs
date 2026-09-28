//! Skills: list, enable/disable, import, delete.
//!
//! Discovery is the real `pantheon_exec::skills` scan (bundled seeding
//! included). The enabled/disabled flag is the shared
//! `<data_dir>/skills/disabled.json` registry — the same file the
//! session loader reads, so a dashboard toggle genuinely removes the
//! skill from the model's toolset. Delete is limited to pantheon-scope
//! skills (under `<data_dir>/skills`); foreign scopes (hermes, claude,
//! …) can be disabled but never deleted by the dashboard.

use crate::server::{Request, Response};
use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_exec::skills::{
    disabled_skill_names, discover_skills_ext, import_skill_from_url, import_skills_from_repo,
    set_skill_disabled,
};
use std::path::PathBuf;

fn project_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// `GET /api/skills?scope=pantheon|external`
pub fn list(app: &App, req: &Request) -> Response {
    let disabled = disabled_skill_names(&app.data_dir);
    let pantheon_root = app.data_dir.join("skills");
    let scope = req.query.get("scope").map(String::as_str).unwrap_or("");
    let skills = discover_skills_ext(&app.data_dir, &project_root(), &[]);
    let out: Vec<serde_json::Value> = skills
        .iter()
        .filter(|s| {
            let pantheon_scope = s.path.starts_with(&pantheon_root);
            match scope {
                "pantheon" => pantheon_scope,
                "external" => !pantheon_scope,
                _ => true,
            }
        })
        .map(|s| {
            let enabled = !disabled.iter().any(|d| d == &s.meta.name);
            let pantheon_scope = s.path.starts_with(&pantheon_root);
            serde_json::json!({
                "name": s.meta.name,
                "description": s.meta.description,
                "origin": s.meta.origin,
                "path": s.path.display().to_string(),
                "enabled": enabled,
                "scope": if pantheon_scope { "pantheon" } else { "external" },
            })
        })
        .collect();
    json_ok(serde_json::json!({"skills": out}))
}

/// `POST /api/skills/:name/toggle` — flips the shared disabled flag.
pub fn toggle(app: &App, name: &str) -> Response {
    let enabled = !disabled_skill_names(&app.data_dir)
        .iter()
        .any(|d| d == name);
    set_enabled(app, name, !enabled)
}

/// `POST /api/skills/:name/enable` or `.../disable`.
pub fn set_enabled(app: &App, name: &str, enabled: bool) -> Response {
    if let Err(e) = set_skill_disabled(&app.data_dir, name, !enabled) {
        return err_json(500, "SKILLS", &e.to_string());
    }
    json_ok(serde_json::json!({"ok": true, "name": name, "enabled": enabled}))
}

/// `DELETE /api/skills/:name` — pantheon-scope only.
pub fn delete(app: &App, name: &str) -> Response {
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.trim().is_empty() {
        return bad_json("unsafe skill name");
    }
    let dir = app.data_dir.join("skills").join(name);
    let root = app.data_dir.join("skills");
    if !dir.starts_with(&root) || !dir.join("SKILL.md").exists() {
        return err_json(404, "NOT_FOUND", "no pantheon-scope skill by that name");
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        return err_json(500, "SKILLS", &format!("delete: {e}"));
    }
    // Also clear any disabled flag so a re-import starts enabled.
    let _ = set_skill_disabled(&app.data_dir, name, false);
    json_ok(serde_json::json!({"ok": true}))
}

/// `POST /api/skills/import`:
/// `{url, confirm: true}` or `{repo, subpath?, confirm: true}`.
/// The UI confirms before sending; the endpoint still requires
/// `confirm: true` before touching the filesystem.
pub fn import(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return bad_json("import requires confirm: true");
    }
    if let Some(url) = body.get("url").and_then(|v| v.as_str()) {
        if url.trim().is_empty() {
            return bad_json("url is empty");
        }
        return match import_skill_from_url(&app.data_dir, url) {
            Ok((dest, skill)) => json_ok(serde_json::json!({
                "ok": true,
                "imported": [{"name": skill.meta.name, "path": dest.display().to_string()}],
            })),
            Err(e) => err_json(502, "SKILL_IMPORT", &e.to_string()),
        };
    }
    if let Some(repo) = body.get("repo").and_then(|v| v.as_str()) {
        if repo.trim().is_empty() {
            return bad_json("repo is empty");
        }
        let subpath = body
            .get("subpath")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        return match import_skills_from_repo(&app.data_dir, repo, subpath) {
            Ok(list) => json_ok(serde_json::json!({
                "ok": true,
                "imported": list.iter().map(|(n, p)| serde_json::json!({
                    "name": n, "path": p.display().to_string(),
                })).collect::<Vec<_>>(),
            })),
            Err(e) => err_json(502, "SKILL_IMPORT", &e.to_string()),
        };
    }
    bad_json("supply {url} or {repo}")
}
