//! OAuth connector tools: broker-side API calls for connected apps.
//!
//! The model never sees token bytes: each tool takes only the request
//! shape (method, path, optional JSON body) and the secrets broker
//! performs the authenticated request, returning the response body.
//! Unconnected apps fail closed with the `pantheon oauth login <app>`
//! remediation.
//!
//! All four tools carry `Capability::NetworkOutbound` under the current
//! per-server capability model. Finer per-tool gating (e.g. parking
//! write methods for approval) belongs to the per-tool capability map on
//! the unmerged `chloe/agent-ui` branch and is intentionally not
//! duplicated here.

use crate::tools::{tool_err, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_secrets::SecretsBroker;
use std::path::{Path, PathBuf};

/// Options to configure OAuth connector tools.
#[derive(Clone, Debug)]
pub struct OAuthToolOptions {
    pub data_dir: PathBuf,
}

fn parse_oauth_args<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, PantheonError> {
    serde_json::from_str(raw).map_err(|e| {
        tool_err(
            "TOOL_BAD_ARGS",
            Layer::Execution,
            false,
            format!("failed to parse arguments: {e}"),
            "check the method, path/url, and body",
        )
    })
}

/// Broker-side request: open the durable broker, run the authed call,
/// return the body. The token never leaves the broker.
fn run_oauth(
    data_dir: &Path,
    app: &str,
    method: &str,
    url: &str,
    body: Option<&str>,
) -> Result<String, PantheonError> {
    let broker = SecretsBroker::durable(data_dir).map_err(|e| {
        tool_err(
            "OAUTH_STORE",
            Layer::Execution,
            false,
            format!("cannot open secret store: {e}"),
            "check the data dir permissions",
        )
    })?;
    broker.oauth_request(app, method, url, body).map_err(|e| {
        tool_err(
            "OAUTH_REQUEST",
            Layer::Execution,
            false,
            e.to_string(),
            "run `pantheon oauth login <app>` if the app is not connected",
        )
    })
}

/// Register one path-scoped tool: `method` + `path` under a fixed base.
fn register_path_tool(
    reg: &mut ToolRegistry,
    opts: &OAuthToolOptions,
    name: &str,
    app: &str,
    base: &str,
    description: &str,
) {
    let data_dir = opts.data_dir.clone();
    let app = app.to_string();
    let base = base.to_string();
    reg.register(
        ToolSchema {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "description": "HTTP method: GET, POST, PUT, PATCH or DELETE."
                    },
                    "path": {
                        "type": "string",
                        "description": format!("API path under {base}, e.g. '/user/repos'. Must start with '/'.")
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional JSON request body."
                    }
                },
                "required": ["method", "path"]
            }),
        },
        Capability::NetworkOutbound,
        move |args| {
            #[derive(serde::Deserialize)]
            struct In {
                method: String,
                path: String,
                body: Option<String>,
            }
            let In { method, path, body } = parse_oauth_args(args)?;
            if !path.starts_with('/') {
                return Err(tool_err(
                    "OAUTH_BAD_PATH",
                    Layer::Execution,
                    false,
                    "path must start with '/'".into(),
                    "use a path like '/user/repos'",
                ));
            }
            run_oauth(
                &data_dir,
                &app,
                &method,
                &format!("{base}{path}"),
                body.as_deref(),
            )
        },
    );
}

/// Register the connector tools. Always registered: each tool fails
/// closed when its app is not connected, and all carry
/// `Capability::NetworkOutbound` so the existing capability gate applies.
pub fn register_oauth_tools(reg: &mut ToolRegistry, opts: OAuthToolOptions) {
    register_path_tool(
        reg,
        &opts,
        "github_api",
        "github",
        "https://api.github.com",
        "Call the GitHub REST API with the connected account, broker-side. The OAuth token never leaves the broker; only the response body is returned. Fails closed when GitHub is not connected.",
    );
    // Google's API surface spans many hosts (gmail, calendar, drive):
    // the tool takes a full URL, constrained broker-side to
    // https://www.googleapis.com/*.
    {
        let data_dir = opts.data_dir.clone();
        reg.register(
            ToolSchema {
                name: "google_api".into(),
                description: "Call a Google API with the connected account, broker-side. The OAuth token never leaves the broker; only the response body is returned. Fails closed when Google is not connected.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "method": {
                            "type": "string",
                            "description": "HTTP method: GET, POST, PUT, PATCH or DELETE."
                        },
                        "url": {
                            "type": "string",
                            "description": "Full https://www.googleapis.com/... URL, e.g. 'https://www.googleapis.com/gmail/v1/users/me/messages'."
                        },
                        "body": {
                            "type": "string",
                            "description": "Optional JSON request body."
                        }
                    },
                    "required": ["method", "url"]
                }),
            },
            Capability::NetworkOutbound,
            move |args| {
                #[derive(serde::Deserialize)]
                struct In {
                    method: String,
                    url: String,
                    body: Option<String>,
                }
                let In { method, url, body } = parse_oauth_args(args)?;
                run_oauth(&data_dir, "google", &method, &url, body.as_deref())
            },
        );
    }
    register_path_tool(
        reg,
        &opts,
        "notion_api",
        "notion",
        "https://api.notion.com/v1",
        "Call the Notion API with the connected integration, broker-side. The OAuth token never leaves the broker and the Notion-Version header is pinned broker-side; only the response body is returned. Fails closed when Notion is not connected.",
    );
    register_path_tool(
        reg,
        &opts,
        "spotify_api",
        "spotify",
        "https://api.spotify.com/v1",
        "Call the Spotify Web API with the connected account, broker-side. The OAuth token never leaves the broker; only the response body is returned. Fails closed when Spotify is not connected.",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_register_with_network_outbound() {
        let mut reg = ToolRegistry::new();
        register_oauth_tools(
            &mut reg,
            OAuthToolOptions {
                data_dir: PathBuf::from("/tmp/oauth-tools-test"),
            },
        );
        for name in ["github_api", "google_api", "notion_api", "spotify_api"] {
            let tool = reg.get(name).unwrap_or_else(|| panic!("{name} registered"));
            assert_eq!(tool.capability, Capability::NetworkOutbound);
        }
    }

    #[test]
    fn path_tool_rejects_relative_path_without_network() {
        let mut reg = ToolRegistry::new();
        register_oauth_tools(
            &mut reg,
            OAuthToolOptions {
                data_dir: PathBuf::from("/tmp/oauth-tools-test"),
            },
        );
        // Relative path is rejected before any secret store is touched.
        let err = reg
            .execute("github_api", r#"{"method":"GET","path":"user/repos"}"#)
            .unwrap_err();
        assert!(err.to_string().contains("OAUTH_BAD_PATH"));
    }

    #[test]
    fn unconnected_app_fails_closed_with_remediation() {
        let mut reg = ToolRegistry::new();
        register_oauth_tools(
            &mut reg,
            OAuthToolOptions {
                // Durable broker opens (file vault), but no tokens stored.
                data_dir: PathBuf::from("/tmp/oauth-tools-test-unconnected"),
            },
        );
        let err = reg
            .execute("spotify_api", r#"{"method":"GET","path":"/me"}"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("pantheon oauth login spotify"),
            "unexpected: {err}"
        );
    }
}
