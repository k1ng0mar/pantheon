//! Form-based config editor for `config.toml`.
//!
//! The field list is auto-discovered, not hand-written: the live
//! `config.toml` is parsed and flattened into dotted-path fields with
//! inferred types. Validation reuses the real schema primitives from
//! `pantheon_api::config_schema`: [`PolicyPreset`] for the `policy` enum
//! and [`SecretRef::validate`](pantheon_api::config_schema::SecretRef)
//! for `api_key` blocks (raw key values are rejected by design —
//! `config.toml` stores env-var *names* only).
//!
//! Writes are atomic (temp file + rename) and two-phase: without
//! `confirm` the endpoint returns a name-only preview; with it, the
//! change is applied and the applied diff is returned.

use crate::server::{Request, Response};
use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_api::config_schema::{PolicyPreset, SecretRef};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml::Value;

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.toml")
}

fn read_doc(data_dir: &Path) -> Result<(Value, String), Response> {
    let path = config_path(data_dir);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| err_json(404, "CONFIG", &format!("read {}: {e}", path.display())))?;
    let doc: Value = raw
        .parse()
        .map_err(|e| err_json(400, "CONFIG", &format!("config.toml does not parse: {e}")))?;
    Ok((doc, raw))
}

/// Atomic write: temp file in the same directory + rename, so a crash
/// mid-write never leaves half a config.
fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Integer(_) => "integer",
        Value::Float(_) => "float",
        Value::Boolean(_) => "bool",
        Value::Datetime(_) => "datetime",
        Value::Array(_) => "array",
        Value::Table(_) => "table",
    }
}

/// A `{source, name}` table is a secret reference.
fn is_secret_ref(v: &Value) -> bool {
    match v {
        Value::Table(t) => t.len() == 2 && t.contains_key("source") && t.contains_key("name"),
        _ => false,
    }
}

fn secret_ref_of(v: &Value) -> Option<SecretRef> {
    let t = v.as_table()?;
    Some(SecretRef {
        source: t.get("source")?.as_str()?.to_string(),
        name: t.get("name")?.as_str()?.to_string(),
    })
}

/// Flatten a TOML document into dotted paths. Secret-ref tables stay
/// whole (they validate as a unit); other tables recurse.
fn flatten(prefix: &str, v: &Value, out: &mut BTreeMap<String, Value>) {
    match v {
        Value::Table(t) if !is_secret_ref(v) => {
            for (k, child) in t {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&path, child, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), v.clone());
        }
    }
}

/// Navigate (and create) intermediate tables for a dotted path. Returns
/// the parent table and the leaf key.
fn parent_of<'a>(
    doc: &'a mut Value,
    path: &str,
) -> Result<(&'a mut toml::map::Map<String, Value>, String), String> {
    let mut parts: Vec<&str> = path.split('.').collect();
    let leaf = parts.pop().ok_or_else(|| "empty path".to_string())?;
    if leaf.is_empty() {
        return Err("empty path segment".to_string());
    }
    let mut cur = doc;
    for part in parts {
        if part.is_empty() {
            return Err("empty path segment".to_string());
        }
        cur = cur
            .as_table_mut()
            .ok_or_else(|| format!("'{path}' walks through a non-table"))?
            .entry(part.to_string())
            .or_insert_with(|| Value::Table(toml::map::Map::new()));
        if !cur.is_table() {
            return Err(format!("'{path}' walks through a non-table value"));
        }
    }
    let table = cur
        .as_table_mut()
        .ok_or_else(|| format!("'{path}' has no parent table"))?;
    Ok((table, leaf.to_string()))
}

fn json_to_toml(v: &serde_json::Value) -> Result<Value, String> {
    match v {
        serde_json::Value::Null => Err("null is not a valid config value".to_string()),
        serde_json::Value::Bool(b) => Ok(Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Float(f))
            } else {
                Err("number out of range".to_string())
            }
        }
        serde_json::Value::String(s) => Ok(Value::String(s.clone())),
        serde_json::Value::Array(a) => Ok(Value::Array(
            a.iter().map(json_to_toml).collect::<Result<Vec<_>, _>>()?,
        )),
        serde_json::Value::Object(o) => {
            let mut m = toml::map::Map::new();
            for (k, val) in o {
                m.insert(k.clone(), json_to_toml(val)?);
            }
            Ok(Value::Table(m))
        }
    }
}

/// Validate one proposed change against the schema flattened from the
/// current document.
fn validate_change(
    schema: &BTreeMap<String, Value>,
    path: &str,
    new: &Value,
) -> Result<(), String> {
    // The policy enum comes from the real schema type, not a hand list.
    // Any `policy` leaf — top-level or under a profile — must be a preset.
    if path == "policy" || path.ends_with(".policy") {
        let s = new
            .as_str()
            .ok_or_else(|| "policy must be a string".to_string())?;
        PolicyPreset::parse(s).ok_or_else(|| {
            format!("unknown policy '{s}'; expected reader, coder, or coder_memory")
        })?;
        return Ok(());
    }
    // Secret refs validate as a unit, wherever they sit.
    if is_secret_ref(new) {
        let r = secret_ref_of(new).ok_or_else(|| "api_key must be {source, name}".to_string())?;
        return r.validate();
    }
    if let Some(current) = schema.get(path) {
        if is_secret_ref(current) && !is_secret_ref(new) {
            return Err(format!(
                "'{path}' is an api_key reference; it must stay {{source = \"env\", name = \"...\"}}"
            ));
        }
        let (want, got) = (type_of(current), type_of(new));
        if want != got {
            return Err(format!("'{path}' must be {want}, got {got}"));
        }
        return Ok(());
    }
    // New path: allowed when the parent table exists (e.g. a new
    // `[agents.<name>]` section); the value still cannot be a raw secret
    // smuggled in as a plain string under an api_key-ish name.
    if path.ends_with("api_key") || path.ends_with("api_key.name") {
        return Err(format!(
            "'{path}' is new: add api_key blocks as {{source = \"env\", name = \"ENV_VAR\"}}"
        ));
    }
    Ok(())
}

/// `GET /api/config`: the document as JSON plus the raw TOML.
pub fn get(app: &App) -> Response {
    let (doc, raw) = match read_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let values = serde_json::to_value(&doc).unwrap_or(serde_json::Value::Null);
    json_ok(serde_json::json!({
        "path": config_path(&app.data_dir).display().to_string(),
        "values": values,
        "raw": raw,
    }))
}

/// `GET /api/config/schema`: auto-discovered fields.
pub fn schema(app: &App) -> Response {
    let (doc, _) = match read_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut flat = BTreeMap::new();
    flatten("", &doc, &mut flat);
    let fields: Vec<serde_json::Value> = flat
        .iter()
        .map(|(path, v)| {
            let mut f = serde_json::json!({
                "path": path,
                "type": if is_secret_ref(v) { "secret_ref" } else { type_of(v) },
                "value": serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
            });
            if path == "policy" || path.ends_with(".policy") {
                f["enum"] = serde_json::json!([
                    PolicyPreset::Reader.as_str(),
                    PolicyPreset::Coder.as_str(),
                    PolicyPreset::CoderMemory.as_str(),
                ]);
            }
            f
        })
        .collect();
    json_ok(serde_json::json!({"fields": fields}))
}

fn apply_changes(app: &App, changes: &BTreeMap<String, Value>, confirm: bool) -> Response {
    let (mut doc, _) = match read_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut flat = BTreeMap::new();
    flatten("", &doc, &mut flat);
    // Validate everything before touching anything.
    for (path, new) in changes {
        if let Err(e) = validate_change(&flat, path, new) {
            return bad_json(&e);
        }
    }
    let names: Vec<&String> = changes.keys().collect();
    if !confirm {
        return json_ok(serde_json::json!({
            "preview": true,
            "changes": names,
            "note": "names only. Resubmit with confirm=true to apply",
        }));
    }
    for (path, new) in changes {
        match parent_of(&mut doc, path) {
            Ok((table, leaf)) => {
                table.insert(leaf, new.clone());
            }
            Err(e) => return bad_json(&e),
        }
    }
    let text = toml::to_string_pretty(&doc).unwrap_or_default();
    if let Err(e) = atomic_write(&config_path(&app.data_dir), &text) {
        return err_json(500, "CONFIG", &format!("write: {e}"));
    }
    json_ok(serde_json::json!({"ok": true, "changed": names}))
}

/// `PUT /api/config`: `{changes: {path: value}, confirm?: bool}`.
pub fn put(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let confirm = body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let obj = match body.get("changes").and_then(|v| v.as_object()) {
        Some(o) => o,
        None => return bad_json("body must be {changes: {path: value}, confirm?}"),
    };
    if obj.is_empty() {
        return bad_json("no changes supplied");
    }
    let mut changes = BTreeMap::new();
    for (path, val) in obj {
        match json_to_toml(val) {
            Ok(t) => {
                changes.insert(path.clone(), t);
            }
            Err(e) => return bad_json(&format!("'{path}': {e}")),
        }
    }
    apply_changes(app, &changes, confirm)
}

/// `GET /api/config/export`: download the raw TOML.
pub fn export(app: &App) -> Response {
    let path = config_path(&app.data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => Response::download("config.toml", "text/plain; charset=utf-8", bytes),
        Err(e) => err_json(404, "CONFIG", &format!("read {}: {e}", path.display())),
    }
}

/// `POST /api/config/import`: `{toml: "...", confirm?: bool}`. The whole
/// document is validated leaf-by-leaf against the current schema before
/// anything is written; unknown tables are allowed (agent profiles are
/// dynamic) but typed fields must keep their types.
pub fn import(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let confirm = body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let text = match body.get("toml").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return bad_json("body must be {toml: \"...\", confirm?}"),
    };
    let new_doc: Value = match text.parse() {
        Ok(d) => d,
        Err(e) => return bad_json(&format!("TOML does not parse: {e}")),
    };
    let (old_doc, _) = match read_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut old_flat = BTreeMap::new();
    flatten("", &old_doc, &mut old_flat);
    let mut new_flat = BTreeMap::new();
    flatten("", &new_doc, &mut new_flat);
    for (path, v) in &new_flat {
        if let Err(e) = validate_change(&old_flat, path, v) {
            return bad_json(&format!("import rejected at '{path}': {e}"));
        }
    }
    // Name-only diff preview.
    let mut changed: Vec<&String> = new_flat
        .keys()
        .filter(|k| old_flat.get(*k) != new_flat.get(*k))
        .collect();
    changed.extend(old_flat.keys().filter(|k| !new_flat.contains_key(*k)));
    changed.sort();
    changed.dedup();
    if !confirm {
        return json_ok(serde_json::json!({
            "preview": true,
            "changes": changed,
            "note": "names only. Resubmit with confirm=true to apply",
        }));
    }
    let out = toml::to_string_pretty(&new_doc).unwrap_or_else(|_| text.to_string());
    if let Err(e) = atomic_write(&config_path(&app.data_dir), &out) {
        return err_json(500, "CONFIG", &format!("write: {e}"));
    }
    json_ok(serde_json::json!({"ok": true, "changed": changed}))
}

/// Every `(dotted_path, env_name)` secret reference in the config, for
/// the Keys view's "used by" column. Parse-only; never resolves values.
pub fn secret_ref_names(data_dir: &Path) -> Vec<(String, String)> {
    let Ok((doc, _)) = read_doc(data_dir) else {
        return Vec::new();
    };
    let mut flat = BTreeMap::new();
    flatten("", &doc, &mut flat);
    flat.iter()
        .filter_map(|(path, v)| {
            let r = secret_ref_of(v)?;
            Some((path.clone(), r.name))
        })
        .collect()
}
