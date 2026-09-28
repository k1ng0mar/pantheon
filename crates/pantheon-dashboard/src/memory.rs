//! Memory browser: agent-layer records with provenance.
//!
//! Reads through the real [`pantheon_memory`] backend selection
//! (`open_selected`), so the dashboard sees whatever backend the install
//! uses. The consolidation score is honestly `null`: the persisted record
//! carries no score field, and the dashboard will not invent one.

use crate::server::{Request, Response};
use crate::{err_json, json_ok, query_usize, App};
use pantheon_memory::{open_selected, LayerKind};

fn namespace_of(req: &Request) -> String {
    req.query
        .get("namespace")
        .cloned()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("PANTHEON_MEMORY_NAMESPACE").ok())
        .unwrap_or_else(|| "nyx".to_string())
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
