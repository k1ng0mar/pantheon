//! `.env` key manager for `<data_dir>/.env`.
//!
//! Every read and write goes through the secrets crate: a
//! [`pantheon_secrets::DotenvVault`] over the file behind a
//! [`pantheon_secrets::SecretsBroker`], so dotenv keys get the same name
//! validation and contracts as every other secret backend.
//!
//! Values are write-only from the UI: `GET` returns names with a fully
//! masked placeholder (`••••`) — never secret material, not even partial.
//! `PUT` takes a two-phase confirm (name-only preview first). Values are
//! never logged — the server logs nothing per request at all.

use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_api::dotenv::valid_key;
use pantheon_gateway::http::{Request, Response};
use pantheon_secrets::{DotenvVault, EnvVault, SecretsBroker};

/// Broker over exactly the `.env` file: no process-env fallback, so
/// `names`/`resolve` reflect the file and nothing else.
fn broker_for(app: &App) -> SecretsBroker {
    SecretsBroker::new()
        .with_vault(Box::new(DotenvVault::new(&app.data_dir)))
        .with_env(EnvVault::from_map(Vec::<(&str, &str)>::new()))
}

/// `GET /api/env`: key names with fully masked values, "used by"
/// cross-reference against the config's `api_key` blocks, and whether the
/// process environment currently shadows the file (exported vars beat the
/// file). Secret material never leaves the server.
pub fn list(app: &App) -> Response {
    let broker = broker_for(app);
    let used_by = super::config::secret_ref_names(&app.data_dir);
    let keys: Vec<serde_json::Value> = broker
        .names()
        .iter()
        .map(|k| {
            let used: Vec<&String> = used_by
                .iter()
                .filter(|(_, name)| name == k)
                .map(|(path, _)| path)
                .collect();
            serde_json::json!({
                "key": k,
                "redacted": "••••",
                "used_by": used,
                "shadowed_by_process_env": std::env::var_os(k).is_some(),
            })
        })
        .collect();
    json_ok(serde_json::json!({"keys": keys}))
}

/// `PUT /api/env`: `{upserts: {KEY: value}, deletes: [KEY], confirm?}`.
/// Without `confirm` returns the name-only preview; with it, applies
/// via one atomic batch write (validation failures abort before any
/// write).
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
    let broker = broker_for(app);
    // Name-only preview: which keys are added, changed, deleted.
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for (k, _) in &upserts {
        if deletes.iter().any(|d| d == k) {
            return bad_json(&format!("'{k}' is both upserted and deleted"));
        }
        match broker.resolve(k).ok().flatten().is_some() {
            true => changed.push(k.clone()),
            false => added.push(k.clone()),
        }
    }
    if !confirm {
        return json_ok(serde_json::json!({
            "preview": true,
            "added": added,
            "changed": changed,
            "deleted": deletes,
            "note": "key names only. Values are write-only. Resubmit with confirm=true to apply",
        }));
    }
    // One atomic commit for the whole PUT: the batch writer applies
    // every upsert and delete in a single read-modify-write
    // (temp-file + rename under the process-wide dotenv lock), so a
    // crash or a concurrent request never leaves a half-applied PUT.
    // This writes the same `<data_dir>/.env` the broker's DotenvVault
    // wraps; names and single-line values were validated above, and
    // the batch writer re-validates defensively.
    if let Err(e) = pantheon_api::dotenv::apply_dotenv_batch(
        &app.data_dir,
        &upserts
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>(),
        &deletes,
    ) {
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
    let broker = broker_for(app);
    let existed = broker.resolve(key).ok().flatten().is_some();
    if let Err(e) = broker.delete(key) {
        return err_json(500, "ENV", &format!("delete: {e}"));
    }
    if existed {
        json_ok(serde_json::json!({"ok": true}))
    } else {
        err_json(404, "NOT_FOUND", "no such key")
    }
}

/// Item 5: redaction tests — the `.env` key manager never lets secret
/// material leave the server. `GET /api/env` returns names with a fully
/// masked placeholder; `PUT` previews (and confirms) with key names
/// only.
#[cfg(test)]
mod env_redaction_tests {
    use super::*;
    use crate::swarm;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_app() -> (App, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dashboard-env-redact-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        let app = App {
            data_dir: dir.clone(),
            token: "test-token".to_string(),
            bind: "127.0.0.1".to_string(),
            bind_all: false,
            on_approval: None,
            send_locks: Default::default(),
            turn_children: Default::default(),
            swarm: swarm::orchestrator_for(&dir),
        };
        (app, dir)
    }

    fn body_of(resp: &Response) -> String {
        match resp {
            Response::Buffered { body, .. } => String::from_utf8(body.clone()).expect("json body"),
            _ => panic!("expected a buffered response"),
        }
    }

    fn put_req(body: &[u8]) -> Request {
        Request {
            method: "PUT".to_string(),
            path: "/api/env".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn list_masks_values_with_placeholder() {
        let (app, _dir) = test_app();
        pantheon_api::dotenv::upsert_dotenv_file(
            &app.data_dir,
            ".env",
            "TEST_LIST_SECRET",
            "s3cr3t-value",
        )
        .expect("seed .env");
        let body = body_of(&list(&app));
        assert!(
            body.contains("TEST_LIST_SECRET"),
            "the key name is listed: {body}"
        );
        assert!(body.contains("••••"), "values are masked: {body}");
        assert!(
            !body.contains("s3cr3t-value"),
            "secret material must not appear in list JSON: {body}"
        );
    }

    #[test]
    fn put_preview_is_name_only_and_writes_nothing() {
        let (app, _dir) = test_app();
        let body = body_of(&put(
            &app,
            &put_req(br#"{"upserts":{"TEST_PREVIEW_KEY":"s3cr3t-value"}}"#),
        ));
        assert!(
            body.contains("\"preview\":true"),
            "unconfirmed PUT previews: {body}"
        );
        assert!(
            body.contains("TEST_PREVIEW_KEY"),
            "preview names the key: {body}"
        );
        assert!(
            !body.contains("s3cr3t-value"),
            "preview must not echo the value: {body}"
        );
        let env_path = app.data_dir.join(".env");
        let written = std::fs::read_to_string(&env_path).unwrap_or_default();
        assert!(
            !written.contains("TEST_PREVIEW_KEY"),
            "preview must not write the file"
        );
    }

    #[test]
    fn put_confirm_applies_but_response_stays_name_only() {
        let (app, _dir) = test_app();
        let body = body_of(&put(
            &app,
            &put_req(br#"{"upserts":{"TEST_CONFIRM_KEY":"s3cr3t-value"},"confirm":true}"#),
        ));
        assert!(
            body.contains("\"ok\":true"),
            "confirmed PUT applies: {body}"
        );
        assert!(
            body.contains("TEST_CONFIRM_KEY") && !body.contains("s3cr3t-value"),
            "confirm response carries names only: {body}"
        );
        // The value reached the file (write-only path works)...
        let written = std::fs::read_to_string(app.data_dir.join(".env")).unwrap_or_default();
        assert!(written.contains("TEST_CONFIRM_KEY"));
        // ...but the list endpoint still masks it.
        let listed = body_of(&list(&app));
        assert!(
            !listed.contains("s3cr3t-value"),
            "list masks after write: {listed}"
        );
    }
}
