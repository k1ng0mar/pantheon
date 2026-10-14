//! `pantheon oauth`: connect hosted apps via local OAuth2.
//!
//! Tokens live in the local durable vaults (OS keychain, else the
//! encrypted file vault): there is no server side. Client IDs come from
//! `PANTHEON_OAUTH_<APP>_CLIENT_ID`; every flow is the RFC 8252
//! public-client loopback flow with PKCE S256, so no client secret is
//! needed (Notion additionally honors `PANTHEON_OAUTH_NOTION_CLIENT_SECRET`
//! for confidential integrations).
//!
//! Nothing here prints token material: status shows presence, scopes and
//! expiry only.

use pantheon_secrets::{oauth, SecretsBroker};
use std::time::{Duration, SystemTime};

/// `pantheon oauth <login|status|logout> [app]`. `args[0]` is argv[0],
/// `args[1]` the verb, `args[2]` the subcommand - same shape as
/// `cmd_gateway`.
pub fn cmd_oauth(args: &[String]) {
    match args.get(2).map(String::as_str) {
        Some("login") => cmd_login(args.get(3).map(String::as_str)),
        Some("status") => cmd_status(),
        Some("logout") => cmd_logout(args.get(3).map(String::as_str)),
        _ => {
            eprintln!("usage: pantheon oauth <login|status|logout> [app]");
            eprintln!("  apps: github, google, notion, spotify");
            std::process::exit(2);
        }
    }
}

fn broker() -> SecretsBroker {
    SecretsBroker::durable(&crate::terminal::data_dir()).unwrap_or_else(|e| {
        eprintln!("oauth: cannot open secret store: {e}");
        std::process::exit(1);
    })
}

fn resolve_app(app: Option<&str>) -> oauth::OAuthProvider {
    let id = app.unwrap_or("").to_lowercase();
    oauth::provider(&id).unwrap_or_else(|| {
        eprintln!("unknown oauth app '{id}'; choose github, google, notion, or spotify");
        std::process::exit(2);
    })
}

/// Per-app client registration hints: where the OAuth client comes from
/// and which redirect URI to allowlist (any 127.0.0.1 port works for the
/// loopback flow; the exact URI is printed at login time).
fn registration_hint(prov: oauth::OAuthProvider) -> &'static str {
    match prov.id {
        "github" => "GitHub Settings > Developer settings > OAuth Apps (Authorization callback URL: http://127.0.0.1)",
        "google" => "Google Cloud Console > APIs & Services > Credentials > OAuth client ID, application type Desktop",
        "notion" => "Notion Developers > your integration > Distribution: public, add http://127.0.0.1 as a redirect URI",
        "spotify" => "Spotify Developer Dashboard > your app > Settings > add http://127.0.0.1 as a redirect URI",
        _ => "your provider's developer console",
    }
}

fn cmd_login(app: Option<&str>) {
    let prov = resolve_app(app);
    let client_id = std::env::var(prov.client_id_env)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            eprintln!(
                "missing {}: create an OAuth client first.",
                prov.client_id_env
            );
            eprintln!("  {}", registration_hint(prov));
            eprintln!(
                "then run: {}=YOUR_CLIENT_ID pantheon oauth login {}",
                prov.client_id_env, prov.id
            );
            std::process::exit(2);
        });
    let broker = broker();
    match oauth::login_flow(prov, &client_id) {
        Ok(token) => {
            let store = oauth::OAuthStore::new(&broker);
            if let Err(e) = store.save(prov.id, &token) {
                eprintln!("oauth: failed to store token: {e}");
                std::process::exit(1);
            }
            let scopes = if token.scopes.is_empty() {
                "(none required)".to_string()
            } else {
                token.scopes.join(" ")
            };
            println!("{} connected (scopes: {scopes})", prov.display);
        }
        Err(e) => {
            eprintln!("oauth: login failed: {e}");
            std::process::exit(1);
        }
    }
}

fn describe_expiry(expires_at: Option<SystemTime>) -> String {
    match expires_at {
        None => "never".to_string(),
        Some(t) => match t.duration_since(SystemTime::now()) {
            Ok(d) => format!("in {}", human_duration(d)),
            Err(_) => "expired".to_string(),
        },
    }
}

fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 5400 {
        format!("{}m", secs / 60)
    } else if secs < 172_800 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn cmd_status() {
    let broker = broker();
    let store = oauth::OAuthStore::new(&broker);
    for prov in oauth::PROVIDERS {
        match store.status(prov.id) {
            Ok(s) if s.connected => {
                let scopes = if s.scopes.is_empty() {
                    "(none required)".to_string()
                } else {
                    s.scopes.join(" ")
                };
                println!(
                    "{}: connected (scopes: {scopes}; expires {})",
                    prov.id,
                    describe_expiry(s.expires_at)
                );
            }
            Ok(_) => println!("{}: not connected", prov.id),
            Err(e) => {
                eprintln!("{}: status error: {e}", prov.id);
                std::process::exit(1);
            }
        }
    }
}

fn cmd_logout(app: Option<&str>) {
    let prov = resolve_app(app);
    let broker = broker();
    let store = oauth::OAuthStore::new(&broker);
    match store.delete(prov.id) {
        Ok(()) => println!("{}: disconnected", prov.id),
        Err(e) => {
            eprintln!("oauth: logout failed: {e}");
            std::process::exit(1);
        }
    }
}
