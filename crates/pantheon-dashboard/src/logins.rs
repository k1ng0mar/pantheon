//! Website-login credential manager for `<data_dir>/logins.json` +
//! `<data_dir>/logins.env`.
//!
//! Mirrors the `.env` key manager's contract ([`super::env`]): values are
//! write-only from the UI, `GET` returns masked placeholders (`••••`)
//! never secret material, not even partial - and writes take a two-phase
//! confirm (preview first, `confirm: true` to apply). Passwords live in
//! `logins.env` (never in `.env`, so they don't surface in the env key
//! list); site/username metadata lives in `logins.json`. The server logs
//! nothing per request at all.

use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_gateway::http::{Request, Response};
use pantheon_secrets::LoginStore;

fn store_for(app: &App) -> LoginStore {
    LoginStore::open(&app.data_dir)
}

/// `GET /api/logins`: every login masked - `{id, site, username,
/// password: "••••"}`. Passwords never leave the server.
pub fn list(app: &App) -> Response {
    let logins = match store_for(app).list() {
        Ok(l) => l,
        Err(e) => return err_json(500, "LOGINS", &format!("read logins: {e}")),
    };
    let logins: Vec<serde_json::Value> = logins.iter().map(|c| c.masked()).collect();
    json_ok(serde_json::json!({ "logins": logins }))
}

fn req_str(body: &serde_json::Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `POST /api/logins`: `{site, username, password, confirm?}`. Without
/// `confirm` returns the name-only preview (the id that would be
/// created); with it, creates. The password is write-only: it appears in
/// no response, ever.
pub fn create(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let site = req_str(&body, "site");
    let username = req_str(&body, "username");
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let (Some(site), Some(username), Some(password)) = (site, username, password) else {
        return bad_json("site, username, and password are required");
    };
    if password.contains('\n') || password.contains('\r') {
        return bad_json("password must be single-line");
    }
    let confirm = body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let store = store_for(app);
    if !confirm {
        return json_ok(serde_json::json!({
            "preview": true,
            "site": site,
            "username": username,
            "note": "password is write-only and shown here as nothing at all. Resubmit with confirm=true to create",
        }));
    }
    match store.create(&site, &username, &password) {
        Ok(cred) => json_ok(serde_json::json!({
            "ok": true,
            "login": cred.masked(),
        })),
        Err(e) => err_json(500, "LOGINS", &format!("create login: {e}")),
    }
}

/// `PUT /api/logins/:id`: `{username?, password?, confirm?}`. At least
/// one of username/password. Two-phase like create: without `confirm`,
/// previews which fields would change.
pub fn update(app: &App, id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let username = req_str(&body, "username");
    let site = req_str(&body, "site");
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    if username.is_none() && site.is_none() && password.is_none() {
        return bad_json("nothing to update: supply site, username and/or password");
    }
    if let Some(p) = &password {
        if p.contains('\n') || p.contains('\r') {
            return bad_json("password must be single-line");
        }
    }
    let confirm = body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let store = store_for(app);
    match store.get(id) {
        Ok(None) => return err_json(404, "NOT_FOUND", "no such login"),
        Err(e) => return err_json(500, "LOGINS", &format!("read logins: {e}")),
        Ok(Some(_)) => {}
    }
    if !confirm {
        return json_ok(serde_json::json!({
            "preview": true,
            "id": id,
            "change_site": site.is_some(),
            "change_username": username.is_some(),
            "change_password": password.is_some(),
            "note": "password is write-only. Resubmit with confirm=true to apply",
        }));
    }
    match store.update(
        id,
        site.as_deref(),
        username.as_deref(),
        password.as_deref(),
    ) {
        Ok(cred) => json_ok(serde_json::json!({
            "ok": true,
            "login": cred.masked(),
        })),
        Err(e) => err_json(500, "LOGINS", &format!("update login: {e}")),
    }
}

/// `DELETE /api/logins/:id`: removes the metadata row and its vault
/// password. 404 when unknown.
pub fn delete(app: &App, id: &str) -> Response {
    match store_for(app).delete(id) {
        Ok(true) => json_ok(serde_json::json!({ "ok": true, "deleted": id })),
        Ok(false) => err_json(404, "NOT_FOUND", "no such login"),
        Err(e) => err_json(500, "LOGINS", &format!("delete login: {e}")),
    }
}
