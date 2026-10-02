//! Memory browser: agent-layer records with provenance.
//!
//! Reads through the real [`pantheon_memory`] backend selection
//! (`open_selected`), so the dashboard sees whatever backend the install
//! uses. The consolidation score is honestly `null`: the persisted record
//! carries no score field, and the dashboard will not invent one.

use crate::util::{now_ms, DEFAULT_AGENT_NAME};
use crate::{bad_json, body_json, created_json, err_json, json_ok, query_usize, App};
use pantheon_api::capability::Policy;
use pantheon_api::error::PantheonError;
use pantheon_api::provenance::TrustTier;
use pantheon_gateway::http::{Request, Response};
use pantheon_memory::{open_selected, write_via, LayerKind, MemoryBackend, Proposal, Provenance};

/// Cap on the serialized proposal bytes for a single `write_via` memory
/// write; the policy gate rejects larger proposals.
const MAX_MEMORY_WRITE_BYTES: usize = 4096;

fn namespace_of(req: &Request) -> String {
    req.query
        .get("namespace")
        .cloned()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("PANTHEON_MEMORY_NAMESPACE").ok())
        .unwrap_or_else(|| DEFAULT_AGENT_NAME.to_string())
}

/// `GET /api/memory?namespace=&q=&limit=`
pub fn browse(app: &App, req: &Request) -> Response {
    let ns = namespace_of(req);
    let limit = query_usize(&req.query, "limit", 200).min(1000);
    let q = req.query.get("q").map(|s| s.to_lowercase());
    let backend = match open_selected(&app.data_dir) {
        Ok(b) => b,
        Err(e) => return err_json(500, "MEMORY", &format!("open backend: {e}")),
    };
    let pairs = match backend.list_agent(&ns) {
        Ok(p) => p,
        Err(e) => return err_json(500, "MEMORY", &format!("list: {e}")),
    };
    let mut out = Vec::new();
    for (key, value) in pairs {
        if let Some(qq) = &q {
            if !key.to_lowercase().contains(qq) && !value.to_lowercase().contains(qq) {
                continue;
            }
        }
        // Provenance comes from the full record; a backend without `get`
        // still yields the key/value row.
        let (provenance, recorded_at_ms) = match backend.get(LayerKind::Agent, &ns, &key) {
            Ok(Some(rec)) => (
                serde_json::json!({
                    "source": rec.provenance.source,
                    "origin": rec.provenance.origin,
                    "trust": format!("{:?}", rec.provenance.trust).to_lowercase(),
                }),
                Some(rec.provenance.recorded_at_ms),
            ),
            _ => (serde_json::Value::Null, None),
        };
        out.push(serde_json::json!({
            "key": key,
            "value": value,
            "layer": "agent",
            "namespace": ns,
            "provenance": provenance,
            "recorded_at_ms": recorded_at_ms,
            // No persisted score exists on the record; null, honestly.
            "score": serde_json::Value::Null,
        }));
        if out.len() >= limit {
            break;
        }
    }
    json_ok(serde_json::json!({"namespace": ns, "records": out}))
}

/// Derive a stable, human-readable key from the text: the first words
/// slugified, plus a short hash so two similar notes never collide into
/// an upsert. `kind`, when given, namespaces the key.
fn derive_key(kind: Option<&str>, text: &str) -> String {
    let mut slug: String = text
        .split_whitespace()
        .take(8)
        .filter_map(|w| {
            let clean: String = w
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            if clean.is_empty() {
                None
            } else {
                Some(clean)
            }
        })
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        slug = "note".to_string();
    }
    // ASCII-only by construction, so byte truncation is char-safe.
    if slug.len() > 48 {
        slug.truncate(48);
    }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    now_ms().hash(&mut h);
    let key = format!("{slug}-{:06x}", h.finish() % 0xffffff);
    match kind {
        Some(k) => format!("{k}:{key}"),
        None => key,
    }
}

/// `POST /api/memory`: store a memory entry (the write path `browse` lacks).
///
/// Body: `{"text": "...", "kind": "..."}` (`kind` optional). Mirrors
/// `pantheon memory put`: agent layer, the request namespace (query
/// `?namespace=`, env, or the default), user-trust provenance stamped by
/// the API. The key is derived from the text - the CLI's explicit key has
/// no equivalent in this body shape. Returns 201 with the entry in browse
/// shape.
pub fn add(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let text = body
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if text.is_empty() {
        return bad_json("field \"text\" is required");
    }
    let kind = body
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let ns = namespace_of(req);
    let backend = match open_selected(&app.data_dir) {
        Ok(b) => b,
        Err(e) => return err_json(500, "MEMORY", &format!("open backend: {e}")),
    };
    let record = match write_via(
        backend.as_ref(),
        &Policy::coder_with_memory(),
        Proposal {
            layer: LayerKind::Agent,
            namespace: ns.clone(),
            key: derive_key(kind, text),
            value: text.to_string(),
            provenance: Provenance {
                source: "api".into(),
                origin: "user".into(),
                trust: TrustTier::User,
                recorded_at_ms: now_ms(),
            },
        },
        MAX_MEMORY_WRITE_BYTES,
    ) {
        Ok(r) => r,
        Err(e) => return err_json(500, "MEMORY", &format!("write: {e}")),
    };
    created_json(serde_json::json!({
        "key": record.key,
        "value": record.value,
        "layer": "agent",
        "namespace": record.namespace,
        "provenance": {
            "source": record.provenance.source,
            "origin": record.provenance.origin,
            "trust": format!("{:?}", record.provenance.trust).to_lowercase(),
        },
        "recorded_at_ms": record.provenance.recorded_at_ms,
        "score": serde_json::Value::Null,
    }))
}

/// Does `id` name an entry in the namespace? Checked through
/// `list_agent` - the same read path `browse` uses - because external
/// backends are query-oriented and may not implement `get`.
fn entry_exists(backend: &dyn MemoryBackend, ns: &str, id: &str) -> Result<bool, PantheonError> {
    Ok(backend.list_agent(ns)?.iter().any(|(k, _)| k == id))
}

fn entry_or_404(backend: &dyn MemoryBackend, ns: &str, id: &str) -> Option<Response> {
    match entry_exists(backend, ns, id) {
        Ok(true) => None,
        Ok(false) => Some(err_json(
            404,
            "NOT_FOUND",
            &format!("no memory entry '{id}' in namespace '{ns}'"),
        )),
        Err(e) => Some(err_json(500, "MEMORY", &format!("list: {e}"))),
    }
}

/// `DELETE /api/memory/:id`: remove one agent-layer entry from the
/// request namespace (`?namespace=`, env, or the default). 404 when the
/// id names no entry.
pub fn remove(app: &App, req: &Request, id: &str) -> Response {
    let ns = namespace_of(req);
    let backend = match open_selected(&app.data_dir) {
        Ok(b) => b,
        Err(e) => return err_json(500, "MEMORY", &format!("open backend: {e}")),
    };
    if let Some(r) = entry_or_404(backend.as_ref(), &ns, id) {
        return r;
    }
    match backend.forget(LayerKind::Agent, &ns, id) {
        Ok(_) => json_ok(serde_json::json!({"ok": true, "key": id, "namespace": ns})),
        Err(e) => err_json(500, "MEMORY", &format!("forget: {e}")),
    }
}

/// `PUT /api/memory/:id`: correct/replace an entry's text in place (same
/// key, same layer and namespace). Body: `{"text": "..."}` - 400 on
/// empty text, 404 on unknown id. The replacement is stamped as a user
/// edit via the API (the same provenance shape `add` uses); the native
/// store upserts on (layer, namespace, key), and the User trust tier
/// wins the upsert guard, so a correction always lands.
pub fn replace(app: &App, req: &Request, id: &str) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let text = body
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if text.is_empty() {
        return bad_json("field \"text\" is required");
    }
    let ns = namespace_of(req);
    let backend = match open_selected(&app.data_dir) {
        Ok(b) => b,
        Err(e) => return err_json(500, "MEMORY", &format!("open backend: {e}")),
    };
    if let Some(r) = entry_or_404(backend.as_ref(), &ns, id) {
        return r;
    }
    let record = match write_via(
        backend.as_ref(),
        &Policy::coder_with_memory(),
        Proposal {
            layer: LayerKind::Agent,
            namespace: ns.clone(),
            key: id.to_string(),
            value: text.to_string(),
            provenance: Provenance {
                source: "api".into(),
                origin: "user".into(),
                trust: TrustTier::User,
                recorded_at_ms: now_ms(),
            },
        },
        MAX_MEMORY_WRITE_BYTES,
    ) {
        Ok(r) => r,
        Err(e) => return err_json(500, "MEMORY", &format!("write: {e}")),
    };
    json_ok(serde_json::json!({
        "key": record.key,
        "value": record.value,
        "layer": "agent",
        "namespace": record.namespace,
        "provenance": {
            "source": record.provenance.source,
            "origin": record.provenance.origin,
            "trust": format!("{:?}", record.provenance.trust).to_lowercase(),
        },
        "recorded_at_ms": record.provenance.recorded_at_ms,
        "score": serde_json::Value::Null,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_app() -> (App, std::path::PathBuf) {
        let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dashboard-memory-test-{}-{}",
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
            turn_children: Default::default(),
            swarm: swarm::orchestrator_for(&dir),
        };
        (app, dir)
    }

    fn json_req(method: &str, body: &str) -> Request {
        let mut query = HashMap::new();
        query.insert("namespace".to_string(), "test-ns".to_string());
        Request {
            method: method.to_string(),
            path: "/api/memory".to_string(),
            query,
            headers: HashMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn status_body(resp: Response) -> (u16, serde_json::Value) {
        match resp {
            Response::Buffered { status, body, .. } => (
                status,
                serde_json::from_slice(&body).expect("response body is JSON"),
            ),
            _ => panic!("expected a buffered response"),
        }
    }

    /// Add one entry through the real handler; returns its derived key.
    fn add_entry(app: &App, text: &str) -> String {
        let (status, body) = status_body(add(
            app,
            &json_req("POST", &format!(r#"{{"text":{text:?}}}"#, text = text)),
        ));
        assert_eq!(status, 201, "{body}");
        body["key"].as_str().expect("key in response").to_string()
    }

    fn listed_keys(app: &App) -> Vec<String> {
        let (status, body) = status_body(browse(app, &json_req("GET", "")));
        assert_eq!(status, 200, "{body}");
        body["records"]
            .as_array()
            .expect("records array")
            .iter()
            .map(|r| r["key"].as_str().expect("key").to_string())
            .collect()
    }

    #[test]
    fn replace_updates_entry_text() {
        let (app, dir) = test_app();
        let key = add_entry(&app, "the sky is blue");
        let (status, body) = status_body(replace(
            &app,
            &json_req("PUT", r#"{"text":"the sky is actually azure"}"#),
            &key,
        ));
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["key"].as_str(), Some(key.as_str()));
        assert_eq!(body["value"].as_str(), Some("the sky is actually azure"));
        // Same key, new text visible through browse.
        assert!(listed_keys(&app).contains(&key));
        let (status, body) = status_body(browse(&app, &json_req("GET", "")));
        assert_eq!(status, 200);
        let rec = body["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"].as_str() == Some(key.as_str()))
            .expect("entry still listed");
        assert_eq!(rec["value"].as_str(), Some("the sky is actually azure"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_rejects_unknown_id_and_empty_text() {
        let (app, dir) = test_app();
        let (status, body) = status_body(replace(
            &app,
            &json_req("PUT", r#"{"text":"whatever"}"#),
            "no-such-key",
        ));
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"].as_str(), Some("NOT_FOUND"));
        let key = add_entry(&app, "keep me");
        let (status, body) =
            status_body(replace(&app, &json_req("PUT", r#"{"text":"   "}"#), &key));
        assert_eq!(status, 400, "{body}");
        // The failed correction left the original text alone.
        assert!(listed_keys(&app).contains(&key));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_deletes_entry_and_404s_afterwards() {
        let (app, dir) = test_app();
        let key = add_entry(&app, "temporary note");
        assert!(listed_keys(&app).contains(&key));
        let (status, body) = status_body(remove(&app, &json_req("DELETE", ""), &key));
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["ok"].as_bool(), Some(true));
        assert!(!listed_keys(&app).contains(&key));
        // Second delete is a 404, not a silent no-op.
        let (status, body) = status_body(remove(&app, &json_req("DELETE", ""), &key));
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"].as_str(), Some("NOT_FOUND"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_404s_on_unknown_id() {
        let (app, dir) = test_app();
        let (status, body) = status_body(remove(&app, &json_req("DELETE", ""), "no-such-key"));
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"].as_str(), Some("NOT_FOUND"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
