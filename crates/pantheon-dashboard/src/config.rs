//! Form-based config editor for `config.toml`.
//!
//! The field list is auto-discovered, not hand-written: the live
//! `config.toml` is parsed and flattened into dotted-path fields with
//! inferred types. Validation reuses the real schema primitives from
//! `pantheon_api::config_schema`: [`PolicyPreset`] for the `policy` enum
//! and [`SecretRef::validate`](pantheon_api::config_schema::SecretRef)
//! for `api_key` blocks (raw key values are rejected by design
//! `config.toml` stores env-var *names* only).
//!
//! Writes are atomic (temp file + rename) and two-phase: without
//! `confirm` the endpoint returns a name-only preview; with it, the
//! change is applied and the applied diff is returned.

use crate::util::{atomic_write, parent_of};
use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_api::config_schema::{PolicyPreset, SecretRef};
use pantheon_gateway::http::{Request, Response};
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

/// Dotted paths that carry secrets. The secret-bearing slots in
/// `config.toml` are the `api_key` tables (provider/API keys, validated
/// through [`SecretRef`](pantheon_api::config_schema::SecretRef)) and the
/// TUI's `api_key_secret` env-name fields. A `{source, name}` table
/// anywhere else is an ordinary table, not a secret reference.
fn is_secret_path(path: &str) -> bool {
    path == "api_key"
        || path.ends_with(".api_key")
        || path == "api_key_secret"
        || path.ends_with(".api_key_secret")
}

/// A `{source, name}` table is a secret reference - but only where the
/// schema expects a secret (see [`is_secret_path`]). Anywhere else (a
/// backup block, a plugin manifest table, ...) it is flattened like a
/// normal table, so a legitimate plain table can never be swallowed by
/// the secret machinery. Malformed refs at secret paths (wrong keys,
/// non-string values, unknown `source`) still fail validation downstream
/// via [`SecretRef::validate`](pantheon_api::config_schema::SecretRef).
fn is_secret_ref(path: &str, v: &Value) -> bool {
    if !is_secret_path(path) {
        return false;
    }
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
        Value::Table(t) if !is_secret_ref(prefix, v) => {
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

/// Lenient coercion for values that arrive as strings but target a
/// non-string field: `"true"`/`"false"` (any case, surrounding
/// whitespace tolerated) for bool fields, ISO-8601 for datetime fields.
/// The dashboard's bool `<select>` posts strings, and hand-driven API
/// callers do too - rejecting them with a type error made every bool
/// unsavable (TOP-10 #4). Real JSON bools pass through untouched; a
/// string that does not parse is left alone so `validate_change`
/// reports the type mismatch as before.
fn coerce_lax(schema: &BTreeMap<String, Value>, path: &str, new: &mut Value) {
    let want = schema.get(path).map(type_of);
    if let (Some(w), Value::String(s)) = (want, &*new) {
        match w {
            "bool" => match s.trim().to_ascii_lowercase().as_str() {
                "true" => *new = Value::Boolean(true),
                "false" => *new = Value::Boolean(false),
                _ => {}
            },
            "datetime" => {
                if let Ok(dt) = s.trim().parse::<toml_datetime::Datetime>() {
                    *new = Value::Datetime(dt);
                }
            }
            _ => {}
        }
    }
}

/// Read one dotted path out of a TOML document.
fn get_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in path.split('.') {
        cur = cur.as_table()?.get(part)?;
    }
    Some(cur)
}

/// Does the typed [`pantheon_api::config::Config`] document know this
/// dotted path? Probes by building a minimal TOML doc holding just the
/// new key, parsing it as `Config`, and serializing back: keys the
/// document type does not declare are silently dropped by the
/// round-trip - exactly the "written then ignored" defect this gates
/// (D-2). Dynamic namespaces (`agents.<name>`, `plugins.<name>`,
/// `custom_providers.<name>`, `mcp.servers.<name>`,
/// `gateway.channels.<name>` - the `HashMap<String, _>` fields of the
/// document type) round-trip fine, so new entries there stay allowed.
/// A parse failure means the key is known but the value has the wrong
/// shape - reported as-is, which doubles as type validation for new
/// keys. Read-only use of the pantheon-api types; pantheon-api itself
/// is untouched.
fn key_known_to_config(path: &str, new: &Value) -> Result<bool, String> {
    let mut probe = Value::Table(toml::map::Map::new());
    match parent_of(&mut probe, path) {
        Ok((table, leaf)) => {
            table.insert(leaf, new.clone());
        }
        Err(e) => return Err(format!("'{path}': {e}")),
    }
    let text = toml::to_string(&probe).map_err(|e| format!("'{path}': {e}"))?;
    let typed: pantheon_api::config::Config =
        toml::from_str(&text).map_err(|e| format!("'{path}' rejected: {e}"))?;
    let back = toml::Value::try_from(&typed).map_err(|e| format!("'{path}': {e}"))?;
    Ok(get_path(&back, path).is_some())
}

/// Validate one proposed change against the schema flattened from the
/// current document.
fn validate_change(
    schema: &BTreeMap<String, Value>,
    path: &str,
    new: &Value,
) -> Result<(), String> {
    // The policy enum comes from the real schema type, not a hand list.
    // Any `policy` leaf - top-level or under a profile - must be a preset.
    if path == "policy" || path.ends_with(".policy") {
        let s = new
            .as_str()
            .ok_or_else(|| "policy must be a string".to_string())?;
        PolicyPreset::parse(s).ok_or_else(|| {
            format!("unknown policy '{s}'; expected reader, coder, or coder_memory")
        })?;
        return Ok(());
    }
    // Secret-bearing paths validate as a unit: the value must be a
    // well-formed {source, name} reference - a raw secret can never be
    // smuggled in as a plain string, and an unknown `source` is rejected
    // by SecretRef::validate. Shape-only, by design: the typed document
    // types several `*_secret` slots as plain strings, so the round-trip
    // probe below cannot judge them - the ref shape is the check.
    if is_secret_path(path) {
        let r = secret_ref_of(new)
            .ok_or_else(|| format!("'{path}' must be {{source = \"env\", name = \"ENV_VAR\"}}"))?;
        return r.validate();
    }
    if let Some(current) = schema.get(path) {
        let (want, got) = (type_of(current), type_of(new));
        if want != got {
            return Err(format!("'{path}' must be {want}, got {got}"));
        }
        return Ok(());
    }
    // New path: the value still cannot be a raw secret smuggled in as a
    // plain string under an api_key-ish name.
    if path.ends_with("api_key") || path.ends_with("api_key.name") {
        return Err(format!(
            "'{path}' is new: add api_key blocks as {{source = \"env\", name = \"ENV_VAR\"}}"
        ));
    }
    // New path: explicit writes reject unknown keys (D-2). The probe
    // consults the canonical document type, so sparse on-disk docs stay
    // writable (a missing-but-known section like `[nightly]` is fine)
    // while `budget.bogus_key` 400s naming the key instead of being
    // written and silently ignored. (Loads only warn - see the
    // pantheon-api load-time unknown-key warnings; consistent policy:
    // explicit writes reject, loads warn.)
    match key_known_to_config(path, new) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("unknown config key '{path}'")),
        Err(e) => Err(e),
    }
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
                "type": if is_secret_ref(path, v) { "secret_ref" } else { type_of(v) },
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

/// Top-level config sections whose values are read once at process
/// startup, so a change only takes effect after a restart. Everything
/// else applies live to new turns/sessions. Deliberately small and
/// documented: `[stt]`/`[tts]`/`[voice]` back long-lived voice
/// pipelines that never re-read config, and `[gateway]` backs the
/// gateway process itself (channels, voice replies).
fn restart_scoped(top: &str) -> bool {
    matches!(top, "stt" | "tts" | "voice" | "gateway")
}

/// Sorted, deduplicated top-level sections among `changed` that need a
/// restart to take effect (see [`restart_scoped`]).
fn restart_required_for<'a>(changed: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = changed
        .map(|p| p.split('.').next().unwrap_or("").to_string())
        .filter(|t| restart_scoped(t))
        .collect();
    out.sort();
    out.dedup();
    out
}

pub(crate) fn apply_changes(
    app: &App,
    changes: &BTreeMap<String, Value>,
    confirm: bool,
) -> Response {
    let (mut doc, _) = match read_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut flat = BTreeMap::new();
    flatten("", &doc, &mut flat);
    // Lenient coercion before validation (bool "true"/"false" strings,
    // ISO-8601 datetime strings) - see `coerce_lax`.
    let mut changes: BTreeMap<String, Value> = changes.clone();
    for (path, new) in changes.iter_mut() {
        coerce_lax(&flat, path, new);
    }
    // Validate everything before touching anything.
    for (path, new) in &changes {
        if let Err(e) = validate_change(&flat, path, new) {
            return bad_json(&e);
        }
    }
    let names: Vec<String> = changes.keys().cloned().collect();
    let restart_required = restart_required_for(names.iter());
    if !confirm {
        return json_ok(serde_json::json!({
            "preview": true,
            "changes": names,
            "restart_required": restart_required,
            "note": "names only. Resubmit with confirm=true to apply",
        }));
    }
    for (path, new) in changes {
        match parent_of(&mut doc, &path) {
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
    json_ok(serde_json::json!({"ok": true, "changed": names, "restart_required": restart_required}))
}

/// Remove whole tables (or leaves) at dotted `paths` through the same
/// read-modify-atomic-write path [`apply_changes`] (and so `PUT
/// /api/config`) uses: the doc is re-read, each path's leaf is removed
/// from its parent table, now-empty parents are pruned so the file does
/// not accumulate empty tables, and the result is atomically written.
/// A path that does not exist is a 404; nothing is written unless every
/// path was removed.
pub(crate) fn remove_paths(app: &App, paths: &[&str]) -> Result<Vec<String>, Response> {
    let (mut doc, _) = match read_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return Err(r),
    };
    let mut removed = Vec::with_capacity(paths.len());
    for path in paths {
        let (table, leaf) = match parent_of(&mut doc, path) {
            Ok(t) => t,
            Err(e) => return Err(bad_json(&e)),
        };
        if table.remove(&leaf).is_none() {
            return Err(err_json(
                404,
                "CONFIG",
                &format!("no such config path '{path}'"),
            ));
        }
        prune_empty_parents(&mut doc, path);
        removed.push(path.to_string());
    }
    let text = toml::to_string_pretty(&doc).unwrap_or_default();
    if let Err(e) = atomic_write(&config_path(&app.data_dir), &text) {
        return Err(err_json(500, "CONFIG", &format!("write: {e}")));
    }
    Ok(removed)
}

/// After a removal, drop parent tables that became empty (mirrors the
/// pruning `DELETE /api/mcp/servers/:name` does for `[mcp.servers]`).
fn prune_empty_parents(doc: &mut Value, path: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut prefix: Vec<&str> = parts[..parts.len().saturating_sub(1)].to_vec();
    while let Some(last) = prefix.pop() {
        let parent = prefix.iter().fold(doc.as_table_mut(), |acc, p| {
            acc.and_then(|t| t.get_mut(*p))
                .and_then(|v| v.as_table_mut())
        });
        let Some(parent) = parent else { break };
        let empty = parent
            .get(last)
            .and_then(|v| v.as_table())
            .map(|t| t.is_empty())
            .unwrap_or(false);
        if !empty {
            break;
        }
        parent.remove(last);
    }
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
/// anything is written; unknown keys are rejected (same explicit-write
/// policy as `PUT /api/config` - dynamic namespaces like `[agents.<name>]`
/// stay writable). On top of the leaf checks, the incoming TOML is parsed
/// through the shared [`pantheon_api::config::Config`] document - a shape
/// the document type rejects is rejected here - and
/// [`Config::validate`](pantheon_api::config::Config::validate) problems
/// are returned as warnings.
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
    // The import must describe a real `config.toml`, not just well-formed
    // TOML: parse it through the shared config document. A shape the
    // document type rejects is rejected here, with the document's error.
    let typed: pantheon_api::config::Config = match toml::from_str(text) {
        Ok(c) => c,
        Err(e) => {
            return bad_json(&format!(
                "import rejected: not a valid config document: {e}"
            ));
        }
    };
    // Doctor-level problems (empty provider, unset env var, unknown agent
    // table...) travel as warnings, not rejections: the leaf checks above
    // are the import gate, this is the shared document's second opinion.
    let doc_warnings = typed.validate();
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
    let restart_required = restart_required_for(changed.iter().copied());
    if !confirm {
        return json_ok(serde_json::json!({
            "preview": true,
            "changes": changed,
            "warnings": doc_warnings,
            "restart_required": restart_required,
            "note": "names only. Resubmit with confirm=true to apply",
        }));
    }
    let out = toml::to_string_pretty(&new_doc).unwrap_or_else(|_| text.to_string());
    if let Err(e) = atomic_write(&config_path(&app.data_dir), &out) {
        return err_json(500, "CONFIG", &format!("write: {e}"));
    }
    json_ok(
        serde_json::json!({"ok": true, "changed": changed, "warnings": doc_warnings, "restart_required": restart_required}),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_pair(path: &str, v: Value) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert(path.to_string(), v);
        m
    }

    /// D-1: bool schema leaves accept "true"/"false" strings (any case);
    /// anything else is left for the strict type checker to reject, and
    /// real booleans are never touched.
    #[test]
    fn coerce_lax_bool_strings() {
        let schema = schema_pair("nightly.enabled", Value::Boolean(true));
        let mut v = Value::String("true".into());
        coerce_lax(&schema, "nightly.enabled", &mut v);
        assert_eq!(v, Value::Boolean(true));
        let mut v = Value::String(" FALSE ".into());
        coerce_lax(&schema, "nightly.enabled", &mut v);
        assert_eq!(v, Value::Boolean(false));
        let mut v = Value::String("yes".into());
        coerce_lax(&schema, "nightly.enabled", &mut v);
        assert_eq!(v, Value::String("yes".into()));
        let mut v = Value::Boolean(false);
        coerce_lax(&schema, "nightly.enabled", &mut v);
        assert_eq!(v, Value::Boolean(false));
    }

    /// D-1: ISO-8601 strings coerce for datetime schema leaves;
    /// unparseable strings pass through to the type-mismatch error.
    #[test]
    fn coerce_lax_datetime_strings() {
        let schema = schema_pair(
            "scheduled.at",
            Value::Datetime("2026-01-01T00:00:00Z".parse().unwrap()),
        );
        let mut v = Value::String("2026-10-01T12:00:00Z".into());
        coerce_lax(&schema, "scheduled.at", &mut v);
        assert!(matches!(v, Value::Datetime(_)), "got {v:?}");
        let mut v = Value::String("not a date".into());
        coerce_lax(&schema, "scheduled.at", &mut v);
        assert_eq!(v, Value::String("not a date".into()));
    }

    /// D-2: the unknown-key gate accepts known keys (including sparse
    /// docs missing whole sections, and dynamic namespaces) and rejects
    /// keys the typed Config document drops on round-trip.
    #[test]
    fn probe_rejects_unknown_keys() {
        assert_eq!(
            key_known_to_config("budget.bogus_key", &Value::Integer(5)),
            Ok(false)
        );
        assert_eq!(
            key_known_to_config("nightly.enabled", &Value::Boolean(true)),
            Ok(true)
        );
        // Dynamic namespaces stay writable.
        assert_eq!(
            key_known_to_config("plugins.myplug.enabled", &Value::Boolean(true)),
            Ok(true)
        );
        // A known key under a section the sparse probe doc does not
        // otherwise mention is still accepted.
        assert_eq!(
            key_known_to_config("stt.backend", &Value::String("groq".into())),
            Ok(true)
        );
    }

    /// A key that is known but whose value has the wrong shape reports an
    /// error (which doubles as type validation for brand-new keys).
    #[test]
    fn probe_reports_wrong_shape_as_error() {
        let r = key_known_to_config("budget.max_turns", &Value::String("lots".into()));
        assert!(r.is_err(), "wrong-shaped value must Err, got {r:?}");
    }

    /// A-3: restart scoping covers the four restart sections and leaves
    /// everything else live; output is sorted and deduplicated.
    #[test]
    fn restart_required_scopes() {
        let paths = [
            "stt.enabled".to_string(),
            "budget.max_turns".to_string(),
            "gateway.port".to_string(),
            "tts.provider".to_string(),
            "stt.provider".to_string(),
        ];
        let rr = restart_required_for(paths.iter());
        assert_eq!(
            rr,
            vec!["gateway".to_string(), "stt".to_string(), "tts".to_string()]
        );
        let empty: Vec<String> = vec![];
        assert!(restart_required_for(empty.iter()).is_empty());
    }
}
