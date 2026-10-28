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
    let doc: Value = toml::from_str(&raw)
        .map_err(|e| err_json(400, "CONFIG", &format!("config.toml does not parse: {e}")))?;
    Ok((doc, raw))
}

/// Convert a parsed `toml::Value` into a `toml_edit` value, for
/// format-preserving writes. Tables become inline tables: a leaf table
/// (e.g. an `api_key` `{source, name}` ref) keeps its value, just not
/// its section layout - the comments elsewhere in the file survive,
/// which is what the old re-serialize path destroyed.
fn edit_value(v: &Value) -> Result<toml_edit::Value, String> {
    match v {
        Value::String(s) => Ok(toml_edit::Value::from(s.as_str())),
        Value::Integer(i) => Ok(toml_edit::Value::from(*i)),
        Value::Float(f) => Ok(toml_edit::Value::from(*f)),
        Value::Boolean(b) => Ok(toml_edit::Value::from(*b)),
        Value::Datetime(dt) => dt
            .to_string()
            .parse::<toml_edit::Datetime>()
            .map(|parsed| toml_edit::Value::from(parsed))
            .map_err(|e| format!("datetime does not round-trip: {e}")),
        Value::Array(items) => {
            let mut a = toml_edit::Array::new();
            for item in items {
                a.push(edit_value(item)?);
            }
            Ok(toml_edit::Value::Array(a))
        }
        Value::Table(t) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, child) in t {
                inline.insert(k, edit_value(child)?);
            }
            Ok(toml_edit::Value::InlineTable(inline))
        }
    }
}

/// Set one dotted path inside a `toml_edit` table, creating intermediate
/// tables as needed. A non-table on the path is an error (the caller
/// already validated against the parsed document, so this only fires
/// when the format-preserving view disagrees with it).
fn set_path_like(
    table: &mut dyn toml_edit::TableLike,
    parts: &[&str],
    mut value: toml_edit::Item,
) -> Result<(), String> {
    if parts.len() == 1 {
        // Replace in place when the key already exists: `insert`
        // replaces the key (and its decor), which drops a comment
        // sitting directly above the line - most visibly a comment
        // above a dotted key (`# note\nnightly.enabled = false`),
        // which toml_edit re-encodes without the outer key's decor.
        // A fresh key still goes through `insert`. Replacing the
        // item wholesale still drops the old VALUE's decor, which is
        // where a trailing comment lives (`enabled = false # note`):
        // copy it onto the replacement for scalar-to-scalar edits,
        // the same rule `set_in_table` uses.
        if let Some(item) = table.get_mut(parts[0]) {
            if let (toml_edit::Item::Value(old), toml_edit::Item::Value(new)) =
                (&*item, &mut value)
            {
                let decor = old.decor().clone();
                *new.decor_mut() = decor;
            }
            *item = value;
        } else {
            table.insert(parts[0], value);
        }
        return Ok(());
    }
    let child = table
        .entry(parts[0])
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let next = child
        .as_table_like_mut()
        .ok_or_else(|| format!("'{}' is not a table", parts[0]))?;
    set_path_like(next, &parts[1..], value)
}

/// Apply dotted-path changes to the raw TOML text in place, preserving
/// comments, blank lines, and key order everywhere else. This is what
/// `PUT /api/config` writes; the old `toml::to_string_pretty` rewrite
/// silently deleted every comment in the file on each save.
pub(crate) fn apply_edits_preserving(
    raw: &str,
    changes: &BTreeMap<String, Value>,
) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| format!("config.toml does not parse for edit: {e}"))?;
    for (path, new) in changes {
        let parts: Vec<&str> = path.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(format!("'{path}' is not a valid dotted path"));
        }
        let value = toml_edit::Item::Value(edit_value(new)?);
        let root = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
        set_path_like(root, &parts, value)?;
    }
    let text = doc.to_string();
    // Verify the edit actually landed: the output must parse, and
    // every changed path must read back as the new value. toml_edit
    // keeps dotted keys (`nightly.enabled = true`) in a form the
    // section-walking edit cannot see, so editing under such a key
    // would otherwise append a second section and write a
    // duplicate-key file. A failed verification is an Err, and every
    // caller falls back to the (comment-losing but valid)
    // re-serialization instead of writing a corrupt document.
    if !verify_edits(&text, changes) {
        return Err("format-preserving edit did not land; refusing to write it".to_string());
    }
    Ok(text)
}

/// The output of a preserving edit must parse, with every changed
/// path reading back as the new value (see [`apply_edits_preserving`]
/// for why this check exists).
fn verify_edits(text: &str, changes: &BTreeMap<String, Value>) -> bool {
    let Ok(doc) = toml::from_str::<Value>(text) else {
        return false;
    };
    changes
        .iter()
        .all(|(path, new)| get_path(&doc, path) == Some(new))
}

/// Remove one dotted path from a `toml_edit` table. Returns false when
/// the path was absent (the caller already 404'd that case against the
/// parsed document, so false here only fires on a view disagreement).
fn remove_path_like(table: &mut dyn toml_edit::TableLike, parts: &[&str]) -> Result<bool, String> {
    if parts.len() == 1 {
        return Ok(table.remove(parts[0]).is_some());
    }
    let Some(child) = table.get_mut(parts[0]) else {
        return Ok(false);
    };
    let Some(next) = child.as_table_like_mut() else {
        return Err(format!("'{}' is not a table", parts[0]));
    };
    remove_path_like(next, &parts[1..])
}

/// Find the table at a dotted prefix inside the document, if it exists
/// and is a table.
fn table_at_mut<'d>(
    doc: &'d mut toml_edit::DocumentMut,
    parts: &[&str],
) -> Option<&'d mut dyn toml_edit::TableLike> {
    let mut cur: &mut dyn toml_edit::TableLike =
        doc.as_table_mut() as &mut dyn toml_edit::TableLike;
    for part in parts {
        let child = cur.get_mut(part)?;
        cur = child.as_table_like_mut()?;
    }
    Some(cur)
}

/// Remove dotted paths from the raw TOML text in place, pruning
/// tables left empty, and preserving comments elsewhere - the
/// format-preserving counterpart of [`remove_paths`]'s rewrite.
pub(crate) fn remove_preserving(raw: &str, paths: &[&str]) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| format!("config.toml does not parse for edit: {e}"))?;
    for path in paths {
        let parts: Vec<&str> = path.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(format!("'{path}' is not a valid dotted path"));
        }
        let root = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
        remove_path_like(root, &parts)?;
        // Prune parents left empty, longest prefix first.
        for len in (1..parts.len()).rev() {
            let parent_parts = &parts[..len - 1];
            let empty = match table_at_mut(&mut doc, &parts[..len]) {
                Some(t) => t.is_empty(),
                None => false,
            };
            if !empty {
                break;
            }
            let leaf = parts[len - 1];
            if parent_parts.is_empty() {
                doc.as_table_mut().remove(leaf);
            } else if let Some(parent) = table_at_mut(&mut doc, parent_parts) {
                parent.remove(leaf);
            }
        }
    }
    let text = doc.to_string();
    // Same landing check as the edit path: the output must parse, with
    // every removed path gone. Otherwise the caller falls back to the
    // re-serialization instead of writing a corrupt document.
    let Ok(out) = toml::from_str::<Value>(&text) else {
        return Err("format-preserving removal did not land; refusing to write it".to_string());
    };
    if paths.iter().any(|p| get_path(&out, p).is_some()) {
        return Err("format-preserving removal did not land; refusing to write it".to_string());
    }
    Ok(text)
}

/// Segment-wise change between two parsed documents, for the
/// format-preserving rewrite of a whole document (used by MCP config
/// writes, which compute their new document as a `toml::Value` and
/// then need it landed on the raw text without losing comments).
/// Segments are literal keys - never re-joined into dotted strings -
/// because a server name may itself contain dots.
enum DocChange {
    Set(Vec<String>, Value),
    Remove(Vec<String>),
}

fn diff_docs(prefix: &mut Vec<String>, old: &Value, new: &Value, out: &mut Vec<DocChange>) {
    match (old, new) {
        (Value::Table(o), Value::Table(n)) => {
            for (k, nv) in n {
                match o.get(k) {
                    Some(ov) => {
                        prefix.push(k.clone());
                        diff_docs(prefix, ov, nv, out);
                        prefix.pop();
                    }
                    None => {
                        prefix.push(k.clone());
                        collect_sets(prefix, nv, out);
                        prefix.pop();
                    }
                }
            }
            for k in o.keys() {
                if !n.contains_key(k) {
                    prefix.push(k.clone());
                    out.push(DocChange::Remove(prefix.clone()));
                    prefix.pop();
                }
            }
        }
        _ if old != new => out.push(DocChange::Set(prefix.clone(), new.clone())),
        _ => {}
    }
}

/// A whole new subtree becomes one `Set` per leaf, so intermediate
/// tables are created as sections by [`set_path_like`] instead of
/// landing as one inline table. An empty new table has no leaves, so
/// it gets an explicit (empty, inline) set.
fn collect_sets(prefix: &mut Vec<String>, v: &Value, out: &mut Vec<DocChange>) {
    match v {
        Value::Table(t) => {
            for (k, child) in t {
                prefix.push(k.clone());
                collect_sets(prefix, child, out);
                prefix.pop();
            }
            if t.is_empty() {
                out.push(DocChange::Set(prefix.clone(), Value::Table(Default::default())));
            }
        }
        _ => out.push(DocChange::Set(prefix.clone(), v.clone())),
    }
}

/// Read a parsed document by literal segments.
fn get_segments<'v>(mut cur: &'v Value, parts: &[String]) -> Option<&'v Value> {
    for part in parts {
        cur = cur.as_table()?.get(part)?;
    }
    Some(cur)
}

/// Land a whole new parsed document onto the raw TOML text, editing
/// only what changed: comments, blank lines, and key order elsewhere
/// survive, and keys are literal segments (so a server name containing
/// dots is one key, not a path). Returns `Err` on any shape the
/// segment writer cannot land, so the caller can fall back to the
/// valid re-serialization instead of writing a corrupt document.
pub(crate) fn write_doc_preserving(raw: &str, old: &Value, new: &Value) -> Result<String, String> {
    let mut changes = Vec::new();
    diff_docs(&mut Vec::new(), old, new, &mut changes);
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| format!("config.toml does not parse for edit: {e}"))?;
    for change in &changes {
        match change {
            DocChange::Set(parts, value) => {
                let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
                let root = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
                set_path_like(root, &refs, toml_edit::Item::Value(edit_value(value)?))?;
            }
            DocChange::Remove(parts) => {
                let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
                let root = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
                remove_path_like(root, &refs)?;
                // Prune parents left empty, longest prefix first.
                for len in (1..refs.len()).rev() {
                    let empty = match table_at_mut(&mut doc, &refs[..len]) {
                        Some(t) => t.is_empty(),
                        None => false,
                    };
                    if !empty {
                        break;
                    }
                    let leaf = refs[len - 1];
                    if len == 1 {
                        doc.as_table_mut().remove(leaf);
                    } else if let Some(parent) = table_at_mut(&mut doc, &refs[..len - 1]) {
                        parent.remove(leaf);
                    }
                }
            }
        }
    }
    let text = doc.to_string();
    let Ok(out) = toml::from_str::<Value>(&text) else {
        return Err("format-preserving document write did not land; refusing to write it".to_string());
    };
    for change in &changes {
        match change {
            DocChange::Set(parts, value) => {
                if get_segments(&out, parts) != Some(value) {
                    return Err("format-preserving document write did not land; refusing to write it"
                        .to_string());
                }
            }
            DocChange::Remove(parts) => {
                if get_segments(&out, parts).is_some() {
                    return Err("format-preserving document write did not land; refusing to write it"
                        .to_string());
                }
            }
        }
    }
    Ok(text)
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

/// Is a dotted JSON path secret-bearing? Leaf names that carry secret
/// material, plus MCP server `env` tables (literal env values for a
/// spawned server, which are secrets in practice, not references).
fn is_redact_path(path: &str) -> bool {
    let leaf = path.rsplit('.').next().unwrap_or(path);
    leaf == "token_secret"
        || leaf == "api_key"
        || leaf == "api_key_secret"
        || leaf.contains("secret")
        || leaf.contains("token")
        || (path.starts_with("mcp.servers.") && path.contains(".env."))
}

/// Redact secret-bearing leaves in a JSON view of the config, in place.
fn redact_values(path: &str, v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                let child_path = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if is_redact_path(&child_path) && !child.is_object() && !child.is_array() {
                    *child = serde_json::Value::String("[redacted]".to_string());
                } else {
                    redact_values(&child_path, child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_values(path, item);
            }
        }
        _ => {}
    }
}

/// `GET /api/config`: the document as JSON plus the raw TOML.
///
/// Inside the token boundary that is acceptable on loopback, but a
/// token holder on a non-loopback bind is a much wider blast radius:
/// when bound non-loopback, secret leaves are redacted and the raw
/// TOML (which cannot be reliably redacted as free text) is omitted.
pub fn get(app: &App) -> Response {
    let (doc, raw) = match read_doc(&app.data_dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut values = serde_json::to_value(&doc).unwrap_or(serde_json::Value::Null);
    if app.bind_all {
        redact_values("", &mut values);
        return json_ok(serde_json::json!({
            "path": config_path(&app.data_dir).display().to_string(),
            "values": values,
            "raw": serde_json::Value::Null,
            "raw_omitted": "bound to a non-loopback address; raw TOML omitted because it may contain secrets (read the file locally instead)",
        }));
    }
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
/// pipelines that never re-read config, `[gateway]` backs the
/// gateway process itself (channels, voice replies), `[server]` backs
/// the listener itself (host/port), and `[swarm]` caps are baked into
/// the orchestrator at startup - all four used to report "live" while
/// needing a restart/rebuild to take effect.
fn restart_scoped(top: &str) -> bool {
    matches!(top, "stt" | "tts" | "voice" | "gateway" | "server" | "swarm")
}

/// Does a [`Config::validate`] problem name exactly one of the changed
/// paths? Same prefix rule as the CLI's `config set` gate: a problem
/// about an untouched key (e.g. a missing `[model]` section) must not
/// veto an unrelated change, but a problem about the changed key does.
fn problem_names_changed(problem: &str, changed: &[String]) -> bool {
    changed.iter().any(|key| {
        problem == key
            || problem.starts_with(&format!("{key} "))
            || problem.starts_with(&format!("{key}:"))
            || problem.starts_with(&format!("{key}."))
    })
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

/// Format-preserving edits (toml_edit, already in the tree via the
/// TUI's `config set`): the dashboard's PUT used to re-serialize the
/// parsed document, silently destroying every user comment and all
/// hand formatting. These helpers apply a change to the raw document
/// instead, so a save touches only the changed keys:
/// - scalar leaves are replaced in place, keeping the line's decor
///   (leading whitespace, trailing comment) intact;
/// - table leaves merge key-by-key instead of replacing the section,
///   so comments on untouched keys survive;
/// - new keys/tables append at the end; nothing else moves.
fn toml_edit_value(v: &Value) -> Result<toml_edit::Value, String> {
    use toml_edit::Formatted;
    Ok(match v {
        Value::String(s) => toml_edit::Value::String(Formatted::new(s.clone())),
        Value::Integer(i) => toml_edit::Value::Integer(Formatted::new(*i)),
        Value::Float(f) => toml_edit::Value::Float(Formatted::new(*f)),
        Value::Boolean(b) => toml_edit::Value::Boolean(Formatted::new(*b)),
        Value::Datetime(dt) => {
            let parsed: toml_edit::Datetime = dt
                .to_string()
                .parse()
                .map_err(|e| format!("datetime does not round-trip: {e}"))?;
            toml_edit::Value::Datetime(Formatted::new(parsed))
        }
        Value::Array(items) => {
            let mut a = toml_edit::Array::new();
            for item in items {
                a.push(toml_edit_value(item)?);
            }
            toml_edit::Value::Array(a)
        }
        Value::Table(t) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, val) in t {
                inline.insert(k.as_str(), toml_edit_value(val)?);
            }
            toml_edit::Value::InlineTable(inline)
        }
    })
}

fn toml_edit_item(v: &Value) -> Result<toml_edit::Item, String> {
    match v {
        Value::Table(t) => {
            let mut table = toml_edit::Table::new();
            for (k, val) in t {
                table.insert(k.as_str(), toml_edit_item(val)?);
            }
            Ok(toml_edit::Item::Table(table))
        }
        _ => Ok(toml_edit::Item::Value(toml_edit_value(v)?)),
    }
}

/// Comment trivia attached to the `key` entry of `table`: leading
/// comments (key leaf decor for values, table decor for tables) plus
/// a trailing same-line comment rewritten as its own line. A prefix
/// of pure whitespace carries no comment and is left behind.
fn entry_comment_trivia(table: &toml_edit::Table, key: &str) -> Option<String> {
    let (k, item) = table.get_key_value(key)?;
    let mut out = String::new();
    let leading = match item {
        toml_edit::Item::Table(t) => t.decor().prefix().and_then(|r| r.as_str()),
        toml_edit::Item::Value(_) => k.leaf_decor().prefix().and_then(|r| r.as_str()),
        _ => None,
    };
    if let Some(prefix) = leading {
        if prefix.contains('#') {
            out.push_str(prefix);
        }
    }
    let suffix = match item {
        toml_edit::Item::Table(t) => t.decor().suffix().and_then(|r| r.as_str()),
        toml_edit::Item::Value(v) => v.decor().suffix().and_then(|r| r.as_str()),
        _ => None,
    };
    if let Some(suffix) = suffix {
        if let Some(idx) = suffix.find('#') {
            out.push_str(&suffix[idx..]);
            out.push('\n');
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Prepend trivia to the `key` entry's leading decor.
fn prepend_entry_trivia(table: &mut toml_edit::Table, key: &str, trivia: &str) {
    let Some((mut k, item)) = table.get_key_value_mut(key) else {
        return;
    };
    let decor = match item {
        toml_edit::Item::Table(t) => t.decor_mut(),
        toml_edit::Item::Value(_) => k.leaf_decor_mut(),
        _ => return,
    };
    let old = decor
        .prefix()
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    decor.set_prefix(format!("{trivia}{old}"));
}

/// Remove `keys` from `table`, moving each removed item's comments to
/// the next surviving key in document order instead of deleting them
/// with it. A comment above the last key has no next sibling and is
/// lost; that is a lone comment on a deleted last line.
fn remove_keys_preserving_comments(table: &mut toml_edit::Table, keys: &[String]) {
    let order: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
    let mut trivia: Vec<(String, String)> = Vec::new();
    for key in keys {
        if let Some(t) = entry_comment_trivia(table, key) {
            trivia.push((key.clone(), t));
        }
        table.remove(key);
    }
    for (removed, t) in trivia {
        let next = order
            .iter()
            .position(|k| k == &removed)
            .and_then(|pos| order.iter().skip(pos + 1).find(|k| table.get(*k).is_some()));
        if let Some(next) = next {
            prepend_entry_trivia(table, next, &t);
        }
    }
}

/// Set one leaf inside an existing table, preserving decor on scalar
/// replacement and merging table leaves key-by-key.
fn set_in_table(table: &mut toml_edit::Table, key: &str, new: &Value) -> Result<(), String> {
    match (table.get_mut(key), new) {
        (Some(toml_edit::Item::Value(old)), Value::Table(_)) => {
            // A scalar becomes a table: no decor to preserve.
            *old = toml_edit_value(new)?;
            Ok(())
        }
        (Some(toml_edit::Item::Value(old)), _) => {
            let decor = old.decor().clone();
            let mut replacement = toml_edit_value(new)?;
            *replacement.decor_mut() = decor;
            *old = replacement;
            Ok(())
        }
        (Some(toml_edit::Item::Table(old)), Value::Table(new_table)) => {
            let stale: Vec<String> = old
                .iter()
                .map(|(k, _)| k.to_string())
                .filter(|k| !new_table.contains_key(k))
                .collect();
            remove_keys_preserving_comments(old, &stale);
            for (k, val) in new_table {
                set_in_table(old, k, val)?;
            }
            Ok(())
        }
        _ => {
            table.insert(key, toml_edit_item(new)?);
            Ok(())
        }
    }
}

/// Apply one dotted-path change to a format-preserving document.
fn set_dotted_edit(
    doc: &mut toml_edit::DocumentMut,
    path: &str,
    new: &Value,
) -> Result<(), String> {
    let mut parts: Vec<&str> = path.split('.').collect();
    let leaf = parts.pop().ok_or_else(|| "empty path".to_string())?;
    if leaf.is_empty() {
        return Err("empty path segment".to_string());
    }
    let mut table = doc.as_table_mut();
    for part in parts {
        if part.is_empty() {
            return Err("empty path segment".to_string());
        }
        let entry = table
            .entry(part)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        match entry {
            toml_edit::Item::Table(t) => table = t,
            _ => return Err(format!("'{path}' walks through a non-table value")),
        }
    }
    set_in_table(table, leaf, new)
}

/// Remove one dotted path from a format-preserving document, pruning
/// parent tables left empty (mirrors [`remove_paths`]). Returns false
/// when the path does not exist.
fn remove_dotted_edit(doc: &mut toml_edit::DocumentMut, path: &str) -> Result<bool, String> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err("empty path segment".to_string());
    }
    fn parent_table<'a>(
        doc: &'a mut toml_edit::DocumentMut,
        prefix: &[&str],
    ) -> Result<Option<&'a mut toml_edit::Table>, String> {
        let mut table = doc.as_table_mut();
        for part in prefix {
            match table.get_mut(*part) {
                Some(toml_edit::Item::Table(t)) => table = t,
                Some(_) => return Err(format!("'{part}' is not a table")),
                None => return Ok(None),
            }
        }
        Ok(Some(table))
    }
    let (leaf, parents) = parts.split_last().ok_or_else(|| "empty path".to_string())?;
    let Some(parent) = parent_table(doc, parents)? else {
        return Ok(false);
    };
    if parent.get(leaf).is_none() {
        return Ok(false);
    }
    remove_keys_preserving_comments(parent, &[(*leaf).to_string()]);
    // Prune empty parents, deepest first.
    for depth in (1..=parents.len()).rev() {
        let prefix = &parents[..depth - 1];
        let name = parents[depth - 1];
        let Some(grand) = parent_table(doc, prefix)? else {
            break;
        };
        let empty = grand
            .get(name)
            .and_then(|i| i.as_table())
            .map(|t| t.iter().count() == 0)
            .unwrap_or(false);
        if !empty {
            break;
        }
        remove_keys_preserving_comments(grand, &[(*name).to_string()]);
    }
    Ok(true)
}

pub(crate) fn apply_changes(
    app: &App,
    changes: &BTreeMap<String, Value>,
    confirm: bool,
) -> Response {
    let (mut doc, raw) = match read_doc(&app.data_dir) {
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
    for (path, new) in &changes {
        match parent_of(&mut doc, path) {
            Ok((table, leaf)) => {
                table.insert(leaf, new.clone());
            }
            Err(e) => return bad_json(&e),
        }
    }
    // `validate_change` above checks shape and schema membership, but
    // never ran `Config::validate()`: a semantically bad value (bad
    // reasoning level, unparseable permission_mode, zero aux timeout)
    // was written and only surfaced later in doctor. Reject here when
    // validation implicates a changed key; nothing is written then.
    match <pantheon_api::config::Config as serde::Deserialize>::deserialize(doc.clone()) {
        Ok(typed) => {
            let problems: Vec<String> = typed
                .validate()
                .into_iter()
                .filter(|p| problem_names_changed(p, &names))
                .collect();
            if !problems.is_empty() {
                return bad_json(&format!(
                    "config change rejected by validation: {}",
                    problems.join("; ")
                ));
            }
        }
        Err(e) => return bad_json(&format!("config change does not form a valid document: {e}")),
    }
    // Format-preserving write: the changes go into the raw text via
    // `toml_edit`, so comments, blank lines, and key order elsewhere
    // survive the save. Only a document `toml_edit` cannot parse (the
    // parsed view above accepted it, so this is pathological) falls
    // back to the re-serialized form, and says so in the response
    // instead of silently eating comments.
    let (text, comments_preserved) = match apply_edits_preserving(&raw, &changes) {
        Ok(t) => (t, true),
        Err(_) => (toml::to_string_pretty(&doc).unwrap_or_default(), false),
    };
    if let Err(e) = atomic_write(&config_path(&app.data_dir), &text) {
        return err_json(500, "CONFIG", &format!("write: {e}"));
    }
    let mut body = serde_json::json!({"ok": true, "changed": names, "restart_required": restart_required, "comments_preserved": comments_preserved});
    if !comments_preserved {
        body["note"] = serde_json::json!("config.toml was rewritten from its parsed form: comments are not preserved for this document (use `pantheon config set` for format-preserving edits)");
    }
    json_ok(body)
}

/// Remove whole tables (or leaves) at dotted `paths` through the same
/// read-modify-atomic-write path [`apply_changes`] (and so `PUT
/// /api/config`) uses: the doc is re-read, each path's leaf is removed
/// from its parent table, now-empty parents are pruned so the file does
/// not accumulate empty tables, and the result is atomically written.
/// A path that does not exist is a 404; nothing is written unless every
/// path was removed.
pub(crate) fn remove_paths(app: &App, paths: &[&str]) -> Result<Vec<String>, Response> {
    let (mut doc, raw) = read_doc(&app.data_dir)?;
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
    // Format-preserving removal, same policy as `apply_changes`: only a
    // document `toml_edit` cannot handle falls back to re-serialization.
    let text = remove_preserving(&raw, paths)
        .unwrap_or_else(|_| toml::to_string_pretty(&doc).unwrap_or_default());
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
///
/// Refused when bound non-loopback: raw TOML cannot be reliably
/// redacted as free text (literal MCP env secrets, api keys), and the
/// token is the only boundary there. Read the file locally instead.
pub fn export(app: &App) -> Response {
    if app.bind_all {
        return err_json(
            403,
            "CONFIG",
            "config export is disabled when bound to a non-loopback address: raw TOML may contain secrets; read config.toml locally instead",
        );
    }
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
    let new_doc: Value = match toml::from_str(text) {
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
    // Doctor-level problems (empty provider, bad reasoning level,
    // unknown agent table...) used to travel as warnings while the
    // import was still written - a broken document landed with a
    // shrug. On confirm they are rejections; the preview still reports
    // them as warnings so the caller can fix before committing. (Import
    // replaces the whole document, so unlike PUT's per-key gate, the
    // full validation result applies here.)
    let doc_warnings = typed.validate();
    if confirm && !doc_warnings.is_empty() {
        return bad_json(&format!(
            "import rejected by validation: {}",
            doc_warnings.join("; ")
        ));
    }
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
    // Write the submitted text verbatim, not a re-serialized document:
    // it parsed and validated above, and re-serializing would strip the
    // author's comments. (PUT merges changes into the existing document
    // through the same format-preserving edit, so it keeps comments too.)
    let out = text.to_string();
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
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_app() -> (crate::App, std::path::PathBuf) {
        let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dashboard-config-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        let app = crate::App {
            data_dir: dir.clone(),
            token: "test-token".to_string(),
            bind: "127.0.0.1".to_string(),
            bind_all: false,
            on_approval: None,
            send_locks: Default::default(),
            ui_action_locks: Default::default(),
            turn_children: Default::default(),
            swarm: crate::swarm::orchestrator_for(&dir),
            desktop: Default::default(),
        };
        (app, dir)
    }

    fn put_req(body: &str) -> pantheon_gateway::http::Request {
        pantheon_gateway::http::Request {
            method: "PUT".to_string(),
            path: "/api/config".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn resp_json(resp: pantheon_gateway::http::Response) -> (u16, serde_json::Value) {
        match resp {
            pantheon_gateway::http::Response::Buffered { status, body, .. } => {
                (status, serde_json::from_slice(&body).unwrap_or_default())
            }
            _ => panic!("expected a buffered response"),
        }
    }

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

    /// A-3: restart scoping covers the restart sections and leaves
    /// everything else live; output is sorted and deduplicated.
    /// `[server]` and `[swarm]` used to report live while needing a
    /// restart/rebuild.
    #[test]
    fn restart_required_scopes() {
        let paths = [
            "stt.enabled".to_string(),
            "budget.max_turns".to_string(),
            "gateway.port".to_string(),
            "tts.provider".to_string(),
            "stt.provider".to_string(),
            "server.port".to_string(),
            "swarm.max_agents".to_string(),
        ];
        let rr = restart_required_for(paths.iter());
        assert_eq!(
            rr,
            vec![
                "gateway".to_string(),
                "server".to_string(),
                "stt".to_string(),
                "swarm".to_string(),
                "tts".to_string()
            ]
        );
        let empty: Vec<String> = vec![];
        assert!(restart_required_for(empty.iter()).is_empty());
    }

    /// Sweep (dashboard/API): PUT /api/config must apply the change
    /// AND preserve the file's comments. Before the fix the write was
    /// a `toml::to_string_pretty` re-serialization that deleted every
    /// comment on each save (disclosed, not fixed). This also covers
    /// the corrected defaults end to end through the generic editor:
    /// web search provider TinyFish and browser backend GSD land in
    /// the file and survive a re-read.
    #[test]
    fn put_preserves_comments_and_applies_search_and_browser() {
        let (app, dir) = test_app();
        std::fs::write(
            dir.join("config.toml"),
            "# Operator note: do not delete me\n[nightly]\nenabled = false\n\n# Search provider note\n[websearch]\nprovider = \"tavily\"\n",
        )
        .expect("seed config");
        let (status, body) = resp_json(put(
            &app,
            &put_req(
                r#"{"changes": {"websearch.provider": "tinyfish", "browser.backend": "gsd"}, "confirm": true}"#,
            ),
        ));
        assert_eq!(status, 200, "PUT must succeed: {body}");
        assert_eq!(body["comments_preserved"], serde_json::json!(true));
        let raw = std::fs::read_to_string(dir.join("config.toml")).expect("read back config");
        assert!(
            raw.contains("# Operator note: do not delete me"),
            "top comment must survive: {raw}"
        );
        assert!(
            raw.contains("# Search provider note"),
            "section comment must survive: {raw}"
        );
        let doc: Value = toml::from_str(&raw).expect("config still parses");
        assert_eq!(
            doc.get("websearch").and_then(|t| t.get("provider")),
            Some(&Value::String("tinyfish".to_string()))
        );
        assert_eq!(
            doc.get("browser").and_then(|t| t.get("backend")),
            Some(&Value::String("gsd".to_string()))
        );
        // The change is visible to the next read, not just the file.
        let (status, body) = resp_json(get(&app));
        assert_eq!(status, 200, "GET after PUT: {body}");
        assert_eq!(body["values"]["websearch"]["provider"], "tinyfish");
        assert_eq!(body["values"]["browser"]["backend"], "gsd");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scalar change on a commented file keeps the comment too (the
    /// nightly toggle shares the preserving writer via `apply_changes`
    /// callers and the nightly path itself).
    #[test]
    fn put_preserves_comments_on_scalar_change() {
        let (app, dir) = test_app();
        std::fs::write(
            dir.join("config.toml"),
            "# Keep me\n[nightly]\nenabled = false\n",
        )
        .expect("seed config");
        let (status, body) = resp_json(put(
            &app,
            &put_req(r#"{"changes": {"nightly.enabled": true}, "confirm": true}"#),
        ));
        assert_eq!(status, 200, "PUT must succeed: {body}");
        let raw = std::fs::read_to_string(dir.join("config.toml")).expect("read back config");
        assert!(raw.contains("# Keep me"), "comment must survive: {raw}");
        let doc: Value = toml::from_str(&raw).expect("config still parses");
        assert_eq!(
            doc.get("nightly").and_then(|t| t.get("enabled")),
            Some(&Value::Boolean(true))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dotted-key document (`nightly.enabled = false` at top level)
    /// edits in place too: toml_edit stores dotted keys as dotted
    /// inline tables, which the `TableLike` walk already handles, so
    /// the dotted form (and the comment) survive instead of gaining a
    /// duplicate `[nightly]` section. The verification guard in
    /// `apply_edits_preserving` is the backstop for any shape the
    /// walker cannot land: it refuses the write and the caller falls
    /// back to a valid re-serialization rather than a corrupt file.
    #[test]
    fn put_on_dotted_key_doc_preserves_dotted_form() {
        let (app, dir) = test_app();
        std::fs::write(
            dir.join("config.toml"),
            "# Keep me\nnightly.enabled = false\n",
        )
        .expect("seed config");
        let (status, body) = resp_json(put(
            &app,
            &put_req(r#"{"changes": {"nightly.enabled": true}, "confirm": true}"#),
        ));
        assert_eq!(status, 200, "PUT must succeed: {body}");
        assert_eq!(body["comments_preserved"], serde_json::json!(true));
        let raw = std::fs::read_to_string(dir.join("config.toml")).expect("read back config");
        assert!(raw.contains("# Keep me"), "comment must survive: {raw}");
        assert!(
            raw.contains("nightly.enabled = true"),
            "dotted form must survive, not gain a [nightly] section: {raw}"
        );
        assert!(
            !raw.contains("[nightly]"),
            "no duplicate section may appear: {raw}"
        );
        let doc: Value = toml::from_str(&raw).expect("output must still parse");
        assert_eq!(
            doc.get("nightly").and_then(|t| t.get("enabled")),
            Some(&Value::Boolean(true))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Removal through the preserving writer keeps unrelated comments
    /// and sections intact. (A comment attached to the removed
    /// section's own header goes with that section - it documented
    /// the removed thing - so the surviving comment lives on the
    /// surviving section here.)
    #[test]
    fn remove_preserves_comments_elsewhere() {
        let (app, dir) = test_app();
        std::fs::write(
            dir.join("config.toml"),
            "[websearch]\nprovider = \"tinyfish\"\n\n# Keep me\n[nightly]\nenabled = true\n",
        )
        .expect("seed config");
        let removed = match remove_paths(&app, &["websearch.provider"]) {
            Ok(v) => v,
            Err(_) => panic!("remove succeeds"),
        };
        assert_eq!(removed, vec!["websearch.provider".to_string()]);
        let raw = std::fs::read_to_string(dir.join("config.toml")).expect("read back config");
        assert!(raw.contains("# Keep me"), "comment must survive: {raw}");
        assert!(
            !raw.contains("tinyfish"),
            "removed value must be gone: {raw}"
        );
        let doc: Value = toml::from_str(&raw).expect("config still parses");
        assert_eq!(
            doc.get("nightly").and_then(|t| t.get("enabled")),
            Some(&Value::Boolean(true))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redact_values_hides_secret_leaves() {
        let mut v = serde_json::json!({
            "model": {"provider": "openai"},
            "gateway": {"channels": {"bot": {"token_secret": "shh"}}},
            "mcp": {"servers": {"srv": {"env": {"KEY": "literal-secret"}}}},
        });
        redact_values("", &mut v);
        assert_eq!(v["model"]["provider"], "openai");
        assert_eq!(v["gateway"]["channels"]["bot"]["token_secret"], "[redacted]");
        assert_eq!(v["mcp"]["servers"]["srv"]["env"]["KEY"], "[redacted]");
    }

    /// PUT's format-preserving edit: comments, blank lines, and trailing
    /// comments survive; only the changed value moves.
    #[test]
    fn set_dotted_edit_preserves_comments() {
        let raw = "# Top comment: operator notes live here\n[nightly]\n# Friday rule: do not flip without asking\nenabled = false # trailing comment\n\n[budget]\nmax_turns = 10 # keep this limit\n";
        let mut doc: toml_edit::DocumentMut = raw.parse().unwrap();
        set_dotted_edit(&mut doc, "nightly.enabled", &Value::Boolean(true)).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# Top comment"), "{out}");
        assert!(out.contains("# Friday rule"), "{out}");
        assert!(out.contains("# trailing comment"), "{out}");
        assert!(out.contains("# keep this limit"), "{out}");
        assert!(out.contains("enabled = true"), "{out}");
        assert!(out.contains("max_turns = 10"), "{out}");
    }

    /// The writer PUT actually uses (`apply_edits_preserving`, via
    /// `set_path_like`) must keep a trailing comment on the changed
    /// key, not just the comments above it: replacing the value
    /// wholesale used to drop the value's decor, which is where the
    /// trailing comment lives.
    #[test]
    fn apply_edits_preserving_keeps_trailing_comment() {
        let raw = "[nightly]\nenabled = false # trailing comment\n";
        let mut changes = BTreeMap::new();
        changes.insert("nightly.enabled".to_string(), Value::Boolean(true));
        let out = apply_edits_preserving(raw, &changes).unwrap();
        assert!(out.contains("enabled = true # trailing comment"), "{out}");
    }

    /// A new key appends without disturbing existing lines.
    #[test]
    fn set_dotted_edit_appends_new_key() {
        let raw = "[nightly]\nenabled = false\n";
        let mut doc: toml_edit::DocumentMut = raw.parse().unwrap();
        set_dotted_edit(&mut doc, "nightly.dry_run", &Value::Boolean(true)).unwrap();
        let out = doc.to_string();
        assert!(out.contains("enabled = false"), "{out}");
        assert!(out.contains("dry_run = true"), "{out}");
    }

    /// A whole-table change merges key-by-key: untouched keys (and
    /// their comments) survive, stale keys go.
    #[test]
    fn set_dotted_edit_merges_table_leaf() {
        let raw = "[server]\n# keep me\nhost = \"a\"\nport = 1 # old port\nstale = true\n";
        let mut doc: toml_edit::DocumentMut = raw.parse().unwrap();
        let mut t = toml::map::Map::new();
        t.insert("port".to_string(), Value::Integer(2));
        set_dotted_edit(&mut doc, "server", &Value::Table(t)).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# keep me"), "{out}");
        assert!(out.contains("port = 2"), "{out}");
        assert!(!out.contains("stale"), "{out}");
    }

    /// Removal prunes tables left empty and keeps surrounding comments.
    #[test]
    fn remove_dotted_edit_prunes_and_preserves() {
        let raw = "# header\n[only]\nkey = 1\n\n[other]\nkey = 2 # stays\n";
        let mut doc: toml_edit::DocumentMut = raw.parse().unwrap();
        assert!(remove_dotted_edit(&mut doc, "only.key").unwrap());
        assert!(!remove_dotted_edit(&mut doc, "only.key").unwrap());
        let out = doc.to_string();
        assert!(out.contains("# header"), "{out}");
        assert!(out.contains("# stays"), "{out}");
        assert!(!out.contains("[only]"), "{out}");
    }

    /// End to end: a confirmed PUT through [`apply_changes`] rewrites
    /// only the changed value; comments and formatting survive.
    #[test]
    fn put_preserves_comments_end_to_end() {
        let dir = std::env::temp_dir().join(format!("pantheon-dec-web-put-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let raw = "# Top comment: operator notes live here\n[nightly]\n# Friday rule: do not flip without asking\nenabled = false # trailing comment\n\n[budget]\nmax_turns = 10 # keep this limit\n";
        std::fs::write(dir.join("config.toml"), raw).unwrap();
        let app = crate::App {
            data_dir: dir.clone(),
            token: "test-token".to_string(),
            bind: "127.0.0.1".to_string(),
            bind_all: false,
            on_approval: None,
            send_locks: Default::default(),
            ui_action_locks: Default::default(),
            turn_children: Default::default(),
            swarm: crate::swarm::orchestrator_for(&dir),
            desktop: Default::default(),
        };
        let mut changes = BTreeMap::new();
        changes.insert("nightly.enabled".to_string(), Value::Boolean(true));
        let resp = apply_changes(&app, &changes, true);
        let (status, body) = match &resp {
            pantheon_gateway::http::Response::Buffered { status, body, .. } => {
                (*status, String::from_utf8_lossy(body).to_string())
            }
            _ => panic!("expected a buffered response"),
        };
        assert_eq!(status, 200, "{body}");
        let out = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(out.contains("# Top comment"), "{out}");
        assert!(out.contains("# Friday rule"), "{out}");
        assert!(out.contains("# trailing comment"), "{out}");
        assert!(out.contains("# keep this limit"), "{out}");
        assert!(out.contains("enabled = true"), "{out}");
        assert!(out.contains("max_turns = 10"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
