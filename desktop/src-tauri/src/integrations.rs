//! Integration inventory surface for the desktop.
//!
//! Pantheon manages MCP servers and skills through its CLI. It does not expose
//! the old app-server inventory protocol, so there is nothing to enumerate
//! here yet; the panel reports that honestly instead of spawning a dead verb.
use crate::providers::executable;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub servers: Vec<Server>,
    pub skills: Vec<Skill>,
    pub apps: Vec<App>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    pub name: String,
    pub description: String,
    pub status: String,
    pub auth_status: String,
    pub tools: Vec<Tool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct App {
    pub name: String,
    pub description: String,
    pub status: String,
}

/// No thread, turn, tool execution, OAuth, configuration writes, or login requests.
pub async fn discover(_cwd: PathBuf) -> Result<Catalog, String> {
    executable("pantheon").ok_or("Install Pantheon to inspect its configured integrations")?;
    Ok(Catalog {
        warnings: vec![
            "Pantheon does not expose automatic integration discovery. Configure servers with \
             `pantheon tools mcp` and skills with `pantheon tools skills`."
                .to_owned(),
        ],
        ..Catalog::default()
    })
}
