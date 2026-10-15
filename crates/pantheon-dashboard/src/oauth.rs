//! OAuth connection status: `GET /api/oauth/status`.
//!
//! Read-only presence metadata for the mobile app's OAuth screen. The
//! response contract is fixed - the mobile app is built against it, do
//! not change field names or shapes:
//!
//! ```json
//! {"providers": [
//!   {"id": "github", "connected": true,
//!    "scopes": ["read:user"], "expires_at": "2026-10-14T20:00:00.000Z"},
//!   {"id": "google", "connected": false, "scopes": [], "expires_at": null}
//! ]}
//! ```
//!
//! `expires_at` is RFC-3339 UTC with millis (same shape as the rest of the
//! dashboard API) or null.
//!
//! Never token material: [`pantheon_secrets::oauth::OAuthStore::status`]
//! returns presence, scopes, and expiry only. A store read failure for
//! one provider degrades that row to disconnected - status must never
//! fail the whole response.

use crate::{json_ok, App};
use pantheon_gateway::http::{Request, Response};
use std::time::{SystemTime, UNIX_EPOCH};

/// One provider's status row. Field names are the mobile contract.
fn provider_row(id: &str, broker: &pantheon_secrets::SecretsBroker) -> serde_json::Value {
    let store = pantheon_secrets::oauth::OAuthStore::new(broker);
    let (connected, scopes, expires_at) = match store.status(id) {
        Ok(s) => (
            s.connected,
            s.scopes,
            s.expires_at.and_then(system_time_rfc3339),
        ),
        Err(_) => (false, Vec::new(), None),
    };
    serde_json::json!({
        "id": id,
        "connected": connected,
        "scopes": scopes,
        "expires_at": expires_at,
    })
}

/// SystemTime as RFC-3339 UTC (`2026-10-14T20:00:00Z`), or `None` when
/// unrepresentable. Millis precision is dropped: whole seconds, matching
/// the token meta's stored granularity.
fn system_time_rfc3339(t: SystemTime) -> Option<String> {
    let ms = t
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()?;
    Some(crate::browser::ts_rfc3339(ms))
}

/// `GET /api/oauth/status` → `{providers: [...]}` in
/// [`pantheon_secrets::oauth::PROVIDERS`] order.
pub fn status(app: &App, _req: &Request) -> Response {
    let cfg = crate::session_factory::load_config(&app.data_dir).unwrap_or(None);
    let broker = crate::session_factory::secrets_broker(cfg.as_ref());
    let providers: Vec<serde_json::Value> = pantheon_secrets::oauth::PROVIDERS
        .iter()
        .map(|p| provider_row(p.id, &broker))
        .collect();
    json_ok(serde_json::json!({ "providers": providers }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_secrets::oauth::OAuthToken;
    use pantheon_secrets::{MemoryVault, SecretValue, SecretsBroker};

    fn memory_broker() -> SecretsBroker {
        SecretsBroker::new().with_vault(Box::new(MemoryVault::new()))
    }

    fn status_json(broker: &SecretsBroker) -> serde_json::Value {
        let providers: Vec<serde_json::Value> = pantheon_secrets::oauth::PROVIDERS
            .iter()
            .map(|p| provider_row(p.id, broker))
            .collect();
        serde_json::json!({ "providers": providers })
    }

    #[test]
    fn all_disconnected_shape() {
        let v = status_json(&memory_broker());
        let providers = v["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 4);
        let ids: Vec<&str> = providers
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["github", "google", "notion", "spotify"]);
        for p in providers {
            assert_eq!(p["connected"], false);
            assert_eq!(p["scopes"].as_array().unwrap().len(), 0);
            assert!(p["expires_at"].is_null());
        }
    }

    #[test]
    fn connected_shape_exposes_no_token_material() {
        let broker = memory_broker();
        let store = pantheon_secrets::oauth::OAuthStore::new(&broker);
        let secret_access = "tok_test_secret_abc123";
        let secret_refresh = "rt_test_secret_def456";
        let expires_at = UNIX_EPOCH + std::time::Duration::from_secs(1_791_000_000); // 2026-10-03T04:00:00Z
        store
            .save(
                "github",
                &OAuthToken {
                    access_token: SecretValue::new(secret_access),
                    refresh_token: Some(SecretValue::new(secret_refresh)),
                    expires_at: Some(expires_at),
                    scopes: vec!["read:user".to_string(), "repo".to_string()],
                    token_type: "Bearer".to_string(),
                },
            )
            .expect("save token");
        let v = status_json(&broker);
        let raw = serde_json::to_string(&v).unwrap();
        // The contract carries presence only: both secret values must be absent.
        assert!(
            !raw.contains(secret_access),
            "access token leaked into status"
        );
        assert!(
            !raw.contains(secret_refresh),
            "refresh token leaked into status"
        );
        assert!(!raw.contains("access_token"), "secret field name leaked");
        let github = v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "github")
            .unwrap();
        assert_eq!(github["connected"], true);
        assert_eq!(github["scopes"], serde_json::json!(["read:user", "repo"]));
        assert_eq!(
            github["expires_at"],
            serde_json::json!("2026-10-03T04:00:00.000Z")
        );
        // Untouched providers stay disconnected.
        let google = v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "google")
            .unwrap();
        assert_eq!(google["connected"], false);
        assert!(google["expires_at"].is_null());
    }
}
