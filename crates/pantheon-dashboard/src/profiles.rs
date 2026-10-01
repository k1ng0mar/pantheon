//! Agent profile persona-file manager.
//!
//! The profile agent is the one with a personality: its `soul_file`
//! (SOUL.md), `user_file` (USER.md), and `agents_file` (AGENTS.md) are
//! injected into the main session's system prompt every turn. These
//! endpoints let the dashboard and the mobile app read and edit those
//! files without shell access.
//!
//! - `GET /api/profiles/:name/files` →
//!   `{soul: {path, content}, user: {path, content}, agents: {path, content}}`.
//!   A file that is not declared or cannot be read reports
//!   `{path: null, content: ""}`.
//! - `PUT /api/profiles/:name/files` ←
//!   `{file: "soul"|"user"|"agents", content: String}`. Writes the content
//!   to the profile's declared path. When the profile declares no path for
//!   that file, the file is created at
//!   `<data_dir>/profiles/<name>/{SOUL,USER,AGENTS}.md` and the profile
//!   field is set through the same validated config write path
//!   `PUT /api/config` uses.
//!
//! Security: the request never chooses a path. Writes go only to (a) the
//! profile's declared file paths (operator-configured, used as-is) or (b)
//! the `<data_dir>/profiles/<name>/` directory. The `:name` segment is
//! rejected when it could escape that directory (`/`, `\`, `..`).
//! Content is capped at 256 KiB.

use crate::util::atomic_write;
use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_gateway::http::{Request, Response};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Persona-file content cap: 256 KiB. These are prompt-injected every turn;
/// anything larger is a mistake, not a persona.
const MAX_FILE_BYTES: usize = 256 * 1024;

/// Map the `file` discriminator to (config field, default file name).
fn file_slot(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "soul" => Some(("soul_file", "SOUL.md")),
        "user" => Some(("user_file", "USER.md")),
        "agents" => Some(("agents_file", "AGENTS.md")),
        _ => None,
    }
}

/// Read `config.toml` as a TOML value. Kept local so this module does not
/// depend on config.rs internals.
fn read_config_doc(data_dir: &Path) -> Result<toml::Value, Response> {
    let path = data_dir.join("config.toml");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| err_json(404, "PROFILES", &format!("read {}: {e}", path.display())))?;
    raw.parse::<toml::Value>()
        .map_err(|e| err_json(400, "PROFILES", &format!("config.toml does not parse: {e}")))
}

/// The `[agents.<name>]` table, or a 404 when the profile is not declared.
fn profile_table<'a>(doc: &'a toml::Value, name: &str) -> Result<&'a toml::Value, Response> {
    doc.get("agents").and_then(|v| v.get(name)).ok_or_else(|| {
        err_json(
            404,
            "PROFILES",
            &format!("no [agents.{name}] profile declared"),
        )
    })
}

/// Declared path for one persona-file field; blank reads as undeclared.
fn declared_path(table: &toml::Value, field: &str) -> Option<String> {
    table
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Reject profile names that could escape `<data_dir>/profiles/<name>/`.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains("..")
}

/// `GET /api/profiles/:name/files`.
pub fn get_files(app: &App, name: &str) -> Response {
    if !valid_name(name) {
        return bad_json("invalid profile name");
    }
    let doc = match read_config_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let table = match profile_table(&doc, name) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let mut out = serde_json::Map::new();
    for (kind, (field, _)) in [
        ("soul", file_slot("soul").unwrap()),
        ("user", file_slot("user").unwrap()),
        ("agents", file_slot("agents").unwrap()),
    ] {
        let (path, content) = match declared_path(table, field) {
            Some(p) => match std::fs::read_to_string(&p) {
                Ok(c) => (Some(p), c),
                Err(_) => (None, String::new()),
            },
            None => (None, String::new()),
        };
        out.insert(
            kind.to_string(),
            serde_json::json!({ "path": path, "content": content }),
        );
    }
    json_ok(serde_json::Value::Object(out))
}

/// Resolve the write target: the declared path when the profile has one,
/// otherwise `<data_dir>/profiles/<name>/<default>`. Returns the target
/// and whether the config field needs setting.
fn write_target(
    data_dir: &Path,
    name: &str,
    table: &toml::Value,
    field: &str,
    default_name: &str,
) -> Result<(PathBuf, bool), Response> {
    if let Some(p) = declared_path(table, field) {
        return Ok((PathBuf::from(p), false));
    }
    let dir = data_dir.join("profiles").join(name);
    std::fs::create_dir_all(&dir)
        .map_err(|e| err_json(500, "PROFILES", &format!("create {}: {e}", dir.display())))?;
    Ok((dir.join(default_name), true))
}

/// `PUT /api/profiles/:name/files` ← `{file, content}`.
pub fn put_files(app: &App, req: &Request, name: &str) -> Response {
    if !valid_name(name) {
        return bad_json("invalid profile name");
    }
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let kind = body.get("file").and_then(|v| v.as_str()).unwrap_or("");
    let (field, default_name) = match file_slot(kind) {
        Some(f) => f,
        None => return bad_json("file must be one of: soul, user, agents"),
    };
    let content = match body.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return bad_json("content is required"),
    };
    if content.len() > MAX_FILE_BYTES {
        return err_json(413, "PROFILES", "content exceeds the 256 KiB cap");
    }
    let doc = match read_config_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let table = match profile_table(&doc, name) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let (target, needs_config) = match write_target(&app.data_dir, name, table, field, default_name)
    {
        Ok(t) => t,
        Err(r) => return r,
    };
    if let Err(e) = atomic_write(&target, content) {
        return err_json(500, "PROFILES", &format!("write: {e}"));
    }
    if needs_config {
        // Point the profile at the new file through the validated config
        // write path (same validation `PUT /api/config` applies).
        let mut changes = BTreeMap::new();
        changes.insert(
            format!("agents.{name}.{field}"),
            toml::Value::String(target.display().to_string()),
        );
        return crate::config::apply_changes(app, &changes, true);
    }
    json_ok(serde_json::json!({
        "ok": true,
        "path": target.display().to_string(),
    }))
}

/* ------------------------------------------------------------------ */
/* Profile DELETE (added separately from the persona-file manager       */
/* above): remove a whole `[agents.<name>]` profile.                    */
/* ------------------------------------------------------------------ */

/// `DELETE /api/profiles/:name` → removes the `[agents.<name>]` table
/// through the same config write path `PUT /api/config` uses
/// ([`crate::config::remove_paths`]).
///
/// - 404 when no `[agents.<name>]` profile is declared.
/// - 409 when `<name>` is the active profile (`agent = "<name>"` in the
///   config): the active profile cannot be deleted.
/// - The profile's persona files under `<data_dir>/profiles/<name>/` are
///   removed too — and only those. Declared paths pointing anywhere
///   outside that directory are left untouched.
pub fn delete_profile(app: &App, name: &str) -> Response {
    if !valid_name(name) {
        return bad_json("invalid profile name");
    }
    let doc = match read_config_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if let Err(r) = profile_table(&doc, name) {
        return r;
    }
    let active = doc.get("agent").and_then(|v| v.as_str()).unwrap_or("");
    if active == name {
        return err_json(
            409,
            "PROFILES",
            &format!(
                "cannot delete the active profile '{name}'; \
                 set `agent` to another profile first"
            ),
        );
    }
    let path = format!("agents.{name}");
    if let Err(r) = crate::config::remove_paths(app, &[&path]) {
        return r;
    }
    // Persona files live only under `<data_dir>/profiles/<name>/`. The
    // name was validated above (no `/`, `\`, or `..`), so this join
    // cannot escape that directory; the starts_with guard is defense in
    // depth.
    let base = app.data_dir.join("profiles");
    let dir = base.join(name);
    if dir.starts_with(&base) {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return err_json(500, "PROFILES", &format!("remove {}: {e}", dir.display())),
        }
    }
    json_ok(serde_json::json!({"ok": true, "deleted": name}))
}
