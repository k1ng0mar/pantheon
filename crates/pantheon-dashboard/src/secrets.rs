//! Secure secret forms for the dashboard (`/api/secrets`).
//!
//! The dashboard half of Pantheon's Secure Vault equivalent. Values are
//! write-only: they arrive in a password field's JSON body, are stored
//! through the secrets broker, and are never returned, logged, or echoed
//! in an error. Listings carry names, scopes, update times and config
//! references only.
//!
//! Auth is the dashboard token, enforced by the gateway before dispatch
//! for every `/api/*` request, so these endpoints are reachable only
//! from an authenticated dashboard session — never from a delegated
//! agent or a plugin, which have no dashboard token and no route here.
//!
//! When the broker is unavailable (no durable vault), every write fails
//! closed with a 500 and nothing is written to config, a log, or `.env`.

use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_gateway::http::{Request, Response};
use pantheon_secrets::{
    BotTokenVerifier, ProviderModelsVerifier, SecretCaller, SecretScope, SecretValue,
    SecretsBroker, SecureFormStore, VerifyOutcome,
};

fn broker_for(app: &App) -> Result<SecretsBroker, Response> {
    SecretsBroker::durable(&app.data_dir).map_err(|e| {
        err_json(
            500,
            "SECRETS_UNAVAILABLE",
            &format!("secrets broker unavailable: {e}"),
        )
    })
}

fn verification_json(outcome: &VerifyOutcome) -> serde_json::Value {
    match outcome {
        VerifyOutcome::Verified => serde_json::json!({"status": "verified"}),
        VerifyOutcome::Failed(reason) => {
            serde_json::json!({"status": "failed", "reason": reason})
        }
        VerifyOutcome::Unavailable(reason) => {
            serde_json::json!({"status": "unavailable", "reason": reason})
        }
    }
}

fn parse_scope(body: &serde_json::Value) -> Result<SecretScope, Response> {
    match body.get("scope").and_then(|v| v.as_str()) {
        None => Ok(SecretScope::Custom),
        Some(s) => SecretScope::parse(s).ok_or_else(|| {
            bad_json(&format!(
                "unknown scope '{s}': use provider, mcp, browser_vault, bot_token or custom"
            ))
        }),
    }
}

fn parse_value(body: &serde_json::Value) -> Result<String, Response> {
    let value = body
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| bad_json("value must be a string"))?;
    if value.is_empty() {
        return Err(bad_json("value must not be empty"));
    }
    if value.contains('\n') || value.contains('\r') {
        return Err(bad_json("value must be single-line"));
    }
    Ok(value.to_string())
}

fn verifier_for(
    scope: SecretScope,
    base_url: Option<&str>,
) -> Option<Box<dyn pantheon_secrets::Verifier>> {
    match scope {
        SecretScope::Provider => base_url.map(|u| {
            Box::new(ProviderModelsVerifier {
                base_url: u.to_string(),
            }) as Box<dyn pantheon_secrets::Verifier>
        }),
        SecretScope::BotToken => Some(Box::new(BotTokenVerifier)),
        _ => None,
    }
}

/// `GET /api/secrets`: names, scopes, update times, config references.
/// Values are never included, not even masked prefixes.
pub fn list(app: &App) -> Response {
    let broker = match broker_for(app) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let store = SecureFormStore::new(&broker, &app.data_dir);
    let config_text = std::fs::read_to_string(app.data_dir.join("config.toml")).unwrap_or_default();
    let refs = SecureFormStore::references(&config_text);
    let (_, warnings) = broker.names_with_warnings();
    let secrets: Vec<serde_json::Value> = store
        .list()
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "scope": e.scope.as_str(),
                "updated_at": e.updated_at,
                "present": e.present,
                "used_by": refs.get(&e.name).cloned().unwrap_or_default(),
            })
        })
        .collect();
    json_ok(serde_json::json!({"secrets": secrets, "warnings": warnings}))
}

/// `POST /api/secrets`: `{name, scope?, value, base_url?}`. Saves via
/// the broker, verifies, and reports the outcome. The value is never
/// returned or echoed.
pub fn save(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => return bad_json("name must be a non-empty string"),
    };
    let scope = match parse_scope(&body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let value = match parse_value(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let base_url = body.get("base_url").and_then(|v| v.as_str());
    let broker = match broker_for(app) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let store = SecureFormStore::new(&broker, &app.data_dir);
    let verifier = verifier_for(scope, base_url);
    let outcome = store.save(
        SecretCaller::UserSession,
        &name,
        scope,
        SecretValue::new(value),
        verifier.as_deref(),
    );
    match outcome {
        Ok(saved) => json_ok(serde_json::json!({
            "saved": true,
            "name": name,
            "scope": scope.as_str(),
            "verification": verification_json(&saved.verification),
        })),
        Err(e) => err_json(
            500,
            "SECRET_SAVE_FAILED",
            &format!("secret could not be saved: {e}"),
        ),
    }
}

/// `POST /api/secrets/:name/rotate`: `{value, base_url?}`. Requires an
/// existing secret; config keeps naming it, so no config edit is needed.
pub fn rotate(app: &App, req: &Request, name: &str) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let value = match parse_value(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let base_url = body.get("base_url").and_then(|v| v.as_str());
    let broker = match broker_for(app) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let store = SecureFormStore::new(&broker, &app.data_dir);
    let scope = store
        .list()
        .into_iter()
        .find(|e| e.name == name)
        .map(|e| e.scope)
        .unwrap_or(SecretScope::Custom);
    let verifier = verifier_for(scope, base_url);
    let outcome = store.rotate(
        SecretCaller::UserSession,
        name,
        SecretValue::new(value),
        verifier.as_deref(),
    );
    match outcome {
        Ok(saved) => json_ok(serde_json::json!({
            "saved": true,
            "name": name,
            "scope": scope.as_str(),
            "verification": verification_json(&saved.verification),
        })),
        Err(e) => err_json(
            500,
            "SECRET_ROTATE_FAILED",
            &format!("secret could not be rotated: {e}"),
        ),
    }
}

/// `DELETE /api/secrets/:name`.
pub fn delete(app: &App, name: &str) -> Response {
    let broker = match broker_for(app) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let store = SecureFormStore::new(&broker, &app.data_dir);
    match store.delete(SecretCaller::UserSession, name) {
        Ok(()) => json_ok(serde_json::json!({"deleted": true, "name": name})),
        Err(e) => err_json(
            500,
            "SECRET_DELETE_FAILED",
            &format!("secret could not be deleted: {e}"),
        ),
    }
}

/// `POST /api/secrets/:name/verify`: `{base_url?}`. Verifies the stored
/// value without returning it. Absent secrets 404.
pub fn verify(app: &App, req: &Request, name: &str) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let base_url = body.get("base_url").and_then(|v| v.as_str());
    let broker = match broker_for(app) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(value) = broker.inject(name).ok().flatten() else {
        return err_json(404, "SECRET_NOT_FOUND", "no secret with that name");
    };
    let store = SecureFormStore::new(&broker, &app.data_dir);
    let scope = store
        .list()
        .into_iter()
        .find(|e| e.name == name)
        .map(|e| e.scope)
        .unwrap_or(SecretScope::Custom);
    let verifier = verifier_for(scope, base_url);
    let outcome = match verifier {
        Some(v) => v.verify(&value),
        None => match pantheon_secrets::default_verifier(scope) {
            Some(v) => v.verify(&value),
            None => {
                VerifyOutcome::Unavailable("no verification check for this secret type".to_string())
            }
        },
    };
    json_ok(serde_json::json!({
        "name": name,
        "verification": verification_json(&outcome),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_app() -> (App, PathBuf) {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dashboard-secrets-test-{}-{}",
            std::process::id(),
            n
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
            ui_action_locks: Default::default(),
            turn_children: Default::default(),
            swarm: crate::swarm::orchestrator_for(&dir),
            desktop: Default::default(),
        };
        (app, dir)
    }

    fn json_req(method: &str, path: &str, body: &str) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn status_of(resp: &Response) -> u16 {
        match resp {
            Response::Buffered { status, .. } => *status,
            _ => panic!("expected a buffered response"),
        }
    }

    fn body_of(resp: &Response) -> String {
        match resp {
            Response::Buffered { body, .. } => String::from_utf8_lossy(body).into_owned(),
            _ => panic!("expected a buffered response"),
        }
    }

    #[test]
    fn save_list_delete_round_trip_never_returns_value() {
        let (app, dir) = test_app();
        let resp = crate::dispatch(
            &app,
            &json_req(
                "POST",
                "/api/secrets",
                r#"{"name":"DASH_TEST_KEY","scope":"custom","value":"super-secret-123"}"#,
            ),
        );
        assert_eq!(status_of(&resp), 200, "{}", body_of(&resp));
        assert!(!body_of(&resp).contains("super-secret-123"));
        let resp = crate::dispatch(&app, &json_req("GET", "/api/secrets", ""));
        assert_eq!(status_of(&resp), 200);
        let body = body_of(&resp);
        assert!(body.contains("DASH_TEST_KEY"));
        assert!(!body.contains("super-secret-123"));
        let resp = crate::dispatch(&app, &json_req("DELETE", "/api/secrets/DASH_TEST_KEY", ""));
        assert_eq!(status_of(&resp), 200);
        let resp = crate::dispatch(&app, &json_req("GET", "/api/secrets", ""));
        assert!(!body_of(&resp).contains("DASH_TEST_KEY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_requires_existing_and_verify_reports_local_shape() {
        let (app, dir) = test_app();
        let resp = crate::dispatch(
            &app,
            &json_req("POST", "/api/secrets/MISSING/rotate", r#"{"value":"x"}"#),
        );
        assert_ne!(status_of(&resp), 200);
        let _ = crate::dispatch(
            &app,
            &json_req(
                "POST",
                "/api/secrets",
                r#"{"name":"BROWSER_VAULT","scope":"browser_vault","value":"long-enough-vault-key-123"}"#,
            ),
        );
        let resp = crate::dispatch(
            &app,
            &json_req("POST", "/api/secrets/BROWSER_VAULT/verify", ""),
        );
        assert_eq!(status_of(&resp), 200, "{}", body_of(&resp));
        assert!(body_of(&resp).contains("verified"));
        assert!(!body_of(&resp).contains("long-enough-vault-key-123"));
        let resp = crate::dispatch(
            &app,
            &json_req(
                "POST",
                "/api/secrets/BROWSER_VAULT/rotate",
                r#"{"value":"another-long-enough-key-456"}"#,
            ),
        );
        assert_eq!(status_of(&resp), 200, "{}", body_of(&resp));
        assert!(!body_of(&resp).contains("another-long-enough-key-456"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_rejects_empty_and_multiline_values() {
        let (app, dir) = test_app();
        for body in [
            r#"{"name":"X","value":""}"#,
            r#"{"name":"X","value":"line1\nline2"}"#,
            r#"{"name":"X","scope":"nope","value":"v"}"#,
        ] {
            let resp = crate::dispatch(&app, &json_req("POST", "/api/secrets", body));
            assert_eq!(status_of(&resp), 400, "{body} -> {}", body_of(&resp));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
