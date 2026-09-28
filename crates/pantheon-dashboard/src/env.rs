//! `.env` key manager for `<data_dir>/.env`.
//!
//! Values are write-only from the UI: `GET` returns redacted previews
//! (`sk-••••1234` — first two chars plus last four; `••••` when short),
//! `PUT` takes a two-phase confirm (name-only preview first), and writes
//! go through the atomic batch writer in `pantheon_api::dotenv`. Values
//! are never logged — the server logs nothing per request at all.

use crate::server::{Request, Response};
use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_api::dotenv::{
    apply_dotenv_batch, delete_dotenv_key, parse_dotenv, read_dotenv_value, valid_key,
};
use std::collections::BTreeMap;

/// Redacted display: `sk-••••1234`. Short values collapse to `••••`.
fn redact_value(v: &str) -> String {
    if v.len() <= 4 {
        "••••".to_string()
    } else {
        let (head, tail) = v.split_at(2);
        format!("{head}••••{}", &tail[tail.len().saturating_sub(4)..])
    }
}

fn dotenv_text(app: &App) -> String {
    std::fs::read_to_string(app.data_dir.join(".env")).unwrap_or_default()
}

/// `GET /api/env`: keys with redacted values, "used by" cross-reference
/// against the config's `api_key` blocks, and whether the process
/// environment currently shadows the file (exported vars beat the file).
pub fn list(app: &App) -> Response {
    let pairs = parse_dotenv(&dotenv_text(app));
    // Last-wins, like the loader.
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in pairs {
        merged.insert(k, v);
    }
    let used_by = super::config::secret_ref_names(&app.data_dir);
    let keys: Vec<serde_json::Value> = merged
        .iter()
        .map(|(k, v)| {
            let used: Vec<&String> = used_by
                .iter()
                .filter(|(_, name)| name == k)
                .map(|(path, _)| path)
                .collect();
            serde_json::json!({
                "key": k,
                "redacted": redact_value(v),
                "used_by": used,
                "shadowed_by_process_env": std::env::var_os(k).is_some(),
            })
        })
        .collect();
    json_ok(serde_json::json!({"keys": keys}))
}

/// `PUT /api/env`: `{upserts: {KEY: value}, deletes: [KEY], confirm?}`.
/// Without `confirm` returns the name-only preview; with it, applies
/// atomically (single temp-file + rename for the whole batch).
pub fn put(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let confirm = body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut upserts: Vec<(String, String)> = Vec::new();
    if let Some(obj) = body.get("upserts").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            let val = match v.as_str() {
                Some(s) => s,
                None => return bad_json(&format!("value for '{k}' must be a string")),
            };
            if !valid_key(k) {
                return bad_json(&format!("invalid key name '{k}'"));
            }
            if val.contains('\n') || val.contains('\r') {
                return bad_json(&format!("value for '{k}' must be single-line"));
            }
            upserts.push((k.clone(), val.to_string()));
        }
    }
    let mut deletes: Vec<String> = Vec::new();
    if let Some(arr) = body.get("deletes").and_then(|v| v.as_array()) {
        for v in arr {
            let k = match v.as_str() {
                Some(s) => s,
                None => return bad_json("deletes must be an array of key names"),
            };
            if !valid_key(k) {
                return bad_json(&format!("invalid key name '{k}'"));
            }
            deletes.push(k.to_string());
        }
    }
    if upserts.is_empty() && deletes.is_empty() {
        return bad_json("nothing to do: supply upserts and/or deletes");
    }
    // Name-only preview: which keys are added, changed, deleted.
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for (k, _) in &upserts {
        if deletes.iter().any(|d| d == k) {
            return bad_json(&format!("'{k}' is both upserted and deleted"));
        }
        match read_dotenv_value(&app.data_dir, k) {
            Some(_) => changed.push(k.clone()),
            None => added.push(k.clone()),
        }
    }
    if !confirm {
        return json_ok(serde_json::json!({
            "preview": true,
            "added": added,
            "changed": changed,
            "deleted": deletes,
            "note": "key names only — values are write-only. resubmit with confirm=true to apply",
        }));
    }
    if let Err(e) = apply_dotenv_batch(&app.data_dir, &upserts, &deletes) {
        return err_json(500, "ENV", &format!("write .env: {e}"));
    }
    json_ok(serde_json::json!({
        "ok": true,
        "added": added,
        "changed": changed,
        "deleted": deletes,
    }))
}

/// `DELETE /api/env/:key`
pub fn delete(app: &App, key: &str) -> Response {
    if !valid_key(key) {
        return bad_json(&format!("invalid key name '{key}'"));
    }
    match delete_dotenv_key(&app.data_dir, key) {
        Ok(true) => json_ok(serde_json::json!({"ok": true})),
        Ok(false) => err_json(404, "NOT_FOUND", "no such key"),
        Err(e) => err_json(500, "ENV", &format!("delete: {e}")),
    }
}

#[cfg(test)]
mod invariant_tests {
    use super::*;

    #[test]
    fn redaction_shows_last_four_only() {
        assert_eq!(redact_value("sk-abcdef1234"), "sk••••1234");
        assert_eq!(redact_value("abc"), "••••");
        assert_eq!(redact_value(""), "••••");
    }
}
