//! Carrying the three surfaces a "just copy the files" migration misses:
//! MCP servers, credentials, and session transcripts.
//!
//! Each one has the same shape of problem — the source representation cannot
//! be dropped into the data dir, but *abandoning* it loses real state. So
//! each gets a bridge: a declaration Pantheon can act on, written in
//! Pantheon's own terms.
//!
//! - **MCP** (`mcp_servers` in a Hermes `config.yaml`, an `mcp.json`, a
//!   `.mcp.json`): parsed into a Pantheon-shaped server list. Transport,
//!   command, args, and url carry over. Headers, tokens, and env values do
//!   not — they become a declared requirement instead.
//! - **Credentials** (`.env`, `auth.json`, provider blocks): read as *names*.
//!   The bridge emits a manifest saying which env var feeds which Pantheon
//!   provider, so the operator can fill them through `pantheon model` rather
//!   than inheriting a foreign vault. With `carry_credentials: true` the
//!   values are written into Pantheon's own encrypted vault, which is the
//!   only way a value moves — never a raw copy of the source `.env`.
//! - **Sessions** (transcript dirs): copied into a quarantine path with a
//!   manifest, so they can be indexed into `session_search` deliberately
//!   rather than silently shadowing ledger-owned transcripts.

use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn cerr(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Runtime,
        false,
        cause,
        remediation,
        "see `pantheon migrate <source> --json` for the full plan",
    )
}

// ===========================================================================
// MCP
// ===========================================================================

/// One MCP server, in Pantheon's terms.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    /// The name Pantheon will register it under.
    pub name: String,
    /// `stdio` or `http`. Anything else is refused at the bridge.
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Env var *names* the server needs. Values are never carried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_env: Vec<String>,
    /// True when the source declared a credential we refused to copy.
    #[serde(default)]
    pub needs_credentials: bool,
}

/// A `mcp.json`-shaped document, as Claude Code / omp / others write it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpJson {
    #[serde(default, rename = "mcpServers", alias = "mcp_servers")]
    pub mcp_servers: BTreeMap<String, McpJsonEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpJsonEntry {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl McpJsonEntry {
    /// Does this entry embed a credential inline?
    pub fn has_credential(&self) -> bool {
        !self.headers.is_empty() || self.env.values().any(|v| looks_secret(v))
    }

    /// Env var names the entry declares, minus the ones that are plainly not
    /// credentials (PATH, HOME, a base URL).
    pub fn credential_env_names(&self) -> Vec<String> {
        self.env
            .iter()
            .filter(|(_, v)| looks_secret(v))
            .map(|(k, _)| k.clone())
            .collect()
    }
}

/// Cheap "this looks like a credential" test, used so a manifest is not read
/// deeply enough to have to trust its contents.
pub fn looks_secret(v: &str) -> bool {
    let v = v.trim();
    if v.len() < 8 {
        return false;
    }
    if v.starts_with("${") || v.starts_with('$') {
        return false; // already an indirection
    }
    if v.starts_with("http://") || v.starts_with("https://") {
        return false;
    }
    let lower = v.to_ascii_lowercase();
    lower.contains("bearer")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("apikey")
        || (v.chars().filter(|c| c.is_ascii_digit()).count() >= 4
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
}

/// Parse an `mcp.json` document into Pantheon servers.
pub fn parse_mcp_json(body: &str) -> Result<Vec<McpServer>, PantheonError> {
    let doc: McpJson = serde_json::from_str(body).map_err(|e| {
        cerr(
            "MIGRATE_MCP_PARSE",
            e.to_string(),
            "check the file is valid JSON",
        )
    })?;
    let mut out = Vec::new();
    for (name, e) in doc.mcp_servers {
        // Infer from the shape when `type` is absent: a url means http.
        let transport = match e.kind.as_deref() {
            None => if e.url.is_some() { "http" } else { "stdio" }.to_string(),
            Some("stdio") | Some("http") | Some("sse") => e.kind.clone().unwrap(),
            Some(other) => {
                return Err(cerr(
                    "MIGRATE_MCP_TRANSPORT",
                    format!("{name}: unsupported transport {other:?}"),
                    "only stdio, http, and sse bridge to Pantheon",
                ))
            }
        };
        let requires_env = e.credential_env_names();
        let needs_credentials = e.has_credential();
        out.push(McpServer {
            name,
            transport,
            command: e.command,
            args: e.args,
            url: e.url,
            requires_env,
            needs_credentials,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Which key a bare `- ` list item currently belongs to, and which block
/// scalar entries are nested in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    /// Top level of a server: `command:`, `url:`, ...
    Server,
    /// Inside `args:`.
    Args,
    /// Inside `headers:`. Every entry is a credential.
    Headers,
    /// Inside `env:`. Only a secret-looking value counts.
    Env,
    /// Any other nested block.
    Other,
}

/// Parse the `mcp_servers:` block out of a Hermes `config.yaml` without a
/// full YAML parse. Hermes writes a flat `name: {command, args, env, headers}`
/// shape, so a line scan is enough and avoids deserialising a struct where a
/// stray field could capture a value.
///
/// A small state machine rather than a line matcher, because a bare `- item`
/// only means an argument when it directly follows `args:`. Under `env:` or
/// `headers:` it is a different list entirely, and folding those into `args`
/// would hand the runtime a command line that was never in the source.
pub fn parse_hermes_mcp(config_yaml: &str) -> Vec<McpServer> {
    let lines: Vec<&str> = config_yaml.lines().collect();
    let Some(start) = lines
        .iter()
        .position(|l| l.trim_start().starts_with("mcp_servers:"))
    else {
        return Vec::new();
    };
    let base = lines[start].len() - lines[start].trim_start().len();

    let mut servers: Vec<McpServer> = Vec::new();
    let mut cur: Option<McpServer> = None;
    let mut command: Option<String> = None;
    let mut url: Option<String> = None;
    let mut args: Vec<String> = Vec::new();
    // Which block the parser is currently inside.
    let mut block = Block::Server;

    macro_rules! flush {
        () => {
            if let Some(mut s) = cur.take() {
                s.command = command.take();
                s.url = url.take();
                s.args = std::mem::take(&mut args);
                if s.url.is_some() && s.transport == "stdio" {
                    s.transport = "http".into();
                }
                servers.push(s);
            }
        };
    }

    for line in &lines[start + 1..] {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent <= base {
            break; // block ended
        }

        // A new server at the first level under `mcp_servers:`.
        if indent == base + 2 && t.ends_with(':') && !t.starts_with('-') {
            flush!();
            let name = t.trim_end_matches(':').trim().trim_matches('"').to_string();
            cur = Some(McpServer {
                name,
                transport: "stdio".into(),
                command: None,
                args: Vec::new(),
                url: None,
                requires_env: Vec::new(),
                needs_credentials: false,
            });
            continue;
        }

        let Some(s) = cur.as_mut() else { continue };

        // A bare list item belongs to whatever block opened it. Only `args`
        // contributes to the command line; an `env:` or `headers:` list is not
        // the command line, and folding it in there would hand the runtime an
        // argv that was never in the source.
        if let Some(item) = t.strip_prefix("- ") {
            if block == Block::Args {
                args.push(unquote(item));
            }
            continue;
        }

        // `key:` with no value opens a block; `key: value` sets a scalar.
        if let Some(key) = t.strip_suffix(':') {
            block = match key.trim() {
                "args" => Block::Args,
                "headers" => Block::Headers,
                "env" => Block::Env,
                _ => Block::Other,
            };
            continue;
        }

        let Some((key, value)) = t.split_once(':') else {
            block = Block::Server;
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let block = std::mem::replace(&mut block, Block::Server);

        match key {
            "command" => command = Some(unquote(value)),
            "url" => url = Some(unquote(value)),
            "args" if value.starts_with('[') => args = parse_flow_seq(value),
            _ => {
                // Inside a headers/env block, or any scalar that looks like a
                // bearer. Record the env var *name* when the source used an
                // indirection; a literal token is flagged but never carried.
                let credential = match block {
                    Block::Headers => true,
                    Block::Env => looks_secret(value),
                    _ => value.to_ascii_lowercase().contains("bearer") && looks_secret(value),
                };
                if credential {
                    s.needs_credentials = true;
                    if let Some(var) = indirection_name(value) {
                        push_unique(&mut s.requires_env, var);
                    } else if let Some(var) = bearer_env_name(value) {
                        push_unique(&mut s.requires_env, var);
                    }
                }
            }
        }
    }
    flush!();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    servers
}

/// The variable name inside a `${VAR}` or `$VAR` reference, if the value is
/// purely an indirection. A value with surrounding literal text is not one.
fn indirection_name(value: &str) -> Option<String> {
    let v = value.trim();
    if let Some(inner) = v.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
        let n = inner.trim();
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Some(n.to_string());
        }
    }
    None
}

/// The `${VAR}` name inside a `Bearer ${VAR}` style header value, where the
/// literal and the reference are mixed.
fn bearer_env_name(value: &str) -> Option<String> {
    let start = value.find("${")?;
    let rest = &value[start + 2..];
    let end = rest.find('}')?;
    let n = rest[..end].trim();
    if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(n.to_string())
    } else {
        None
    }
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_string()
}

fn parse_flow_seq(s: &str) -> Vec<String> {
    s.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(unquote)
        .filter(|x| !x.is_empty())
        .collect()
}

// ===========================================================================
// Credentials
// ===========================================================================

/// One credential the source declares and Pantheon must be told to supply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialMapping {
    /// The env var name in the source.
    pub env_var: String,
    /// Where it should be supplied from, when we can tell.
    pub target: CredentialTarget,
    /// Set when the source had a value for it. The value is never here.
    pub had_value: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialTarget {
    /// A catalogued Pantheon provider; supply via `pantheon model`.
    Provider,
    /// An MCP server header; supply when registering the server.
    McpAuth,
    /// A channel bot token (telegram, discord, ...).
    Channel,
    /// We could not classify it.
    Unclassified,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialManifest {
    pub source: String,
    /// Names only. Values live in the vault, never in this file.
    pub mappings: Vec<CredentialMapping>,
    /// Written only when `carry_credentials` was set.
    pub vault_written: Vec<String>,
}

impl CredentialManifest {
    pub fn unclassified(&self) -> usize {
        self.mappings
            .iter()
            .filter(|m| m.target == CredentialTarget::Unclassified)
            .count()
    }
}

/// Classify an env var name into a Pantheon credential target.
///
/// Name-based on purpose: reading values to classify them would defeat the
/// point of not copying them.
pub fn classify_credential(name: &str) -> CredentialTarget {
    let n = name.to_ascii_uppercase();
    // MCP first: `MCP_COMPOSIO_API_KEY` ends in KEY and would otherwise be
    // filed as a model provider, which is exactly the wrong destination.
    if n.contains("MCP") {
        return CredentialTarget::McpAuth;
    }
    let last = n.rsplit('_').next().unwrap_or("");
    if matches!(
        last,
        "TOKEN" | "SECRET" | "KEY" | "PASSWORD" | "CREDENTIALS"
    ) {
        // Bot tokens belong to a channel; everything else looks like a
        // provider or service credential.
        if n.contains("TELEGRAM")
            || n.contains("DISCORD")
            || n.contains("SLACK")
            || n.contains("WHATSAPP")
            || n.contains("SIGNAL")
            || n.contains("MATRIX")
            || n.contains("IRC")
            || n.contains("FEISHU")
            || n.contains("WECOM")
            || n.contains("TEAMS")
            || n.contains("LINE_")
            || n.contains("NTFY")
        {
            return CredentialTarget::Channel;
        }
        return CredentialTarget::Provider;
    }
    CredentialTarget::Unclassified
}

/// One parsed `.env` entry: the name, and whether it carried a real value.
/// The value itself is deliberately not returned — reading it is the
/// caller's business, and it goes straight into a vault if anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEntry {
    pub name: String,
    pub had_value: bool,
    /// The raw value, `None` unless the caller explicitly asked for values.
    /// Populated only by [`parse_env_values`].
    pub value: Option<String>,
}

/// The value part of a dotenv line, with the rules
/// `pantheon-cli::dotenv::parse_dotenv` uses: trim, strip an inline ` #`
/// comment when the value is unquoted, then strip one layer of matching quotes.
///
/// One implementation, used by both parsers in this crate, and pinned against
/// the CLI's by a cross-crate test. The two drifted once: the CLI stripped the
/// comment and this did not, so a key carried by `pantheon migrate` could land
/// in `<data_dir>/.env` with the comment still attached.
pub(crate) fn dotenv_value(raw: &str) -> String {
    let v = raw.trim();
    let v = if v.starts_with('"') || v.starts_with('\'') {
        v
    } else if let Some(hash) = v.find(" #") {
        v[..hash].trim_end()
    } else {
        v
    };
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        return v[1..v.len() - 1].to_string();
    }
    v.to_string()
}

/// Parse a `.env`-shaped file into names + whether each had a value.
pub fn parse_env_names(body: &str) -> Vec<EnvEntry> {
    parse_env(body, false)
}

/// Parse a `.env`-shaped file **including values**. Only the credential-bridge
/// path calls this, and it writes the result straight into an encrypted vault
/// without echoing it. Nothing here prints, logs, or returns a value through
/// a plan, a report, or an error.
pub fn parse_env_values(body: &str) -> Vec<EnvEntry> {
    parse_env(body, true)
}

fn parse_env(body: &str, with_values: bool) -> Vec<EnvEntry> {
    let mut out = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let t = t.strip_prefix("export ").unwrap_or(t);
        let Some((k, v)) = t.split_once('=') else {
            continue;
        };
        let k = k.trim();
        // A shell identifier, not just an alphanumeric run: a leading digit
        // means this was not a variable assignment.
        let mut chars = k.chars();
        let valid_start = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
        if !valid_start || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = dotenv_value(v);
        let v = value.as_str();
        // Placeholders and indirections are "not really set".
        let real = !v.is_empty()
            && !v.starts_with("your_")
            && !v.starts_with("changeme")
            && !v.starts_with('<')
            && v != "null"
            && v != "TODO";
        out.push(EnvEntry {
            name: k.to_string(),
            had_value: real,
            value: if with_values && real {
                Some(v.to_string())
            } else {
                None
            },
        });
    }
    out
}

/// Build the credential manifest for a source's declared names.
pub fn credential_manifest(source: &str, names: &[String]) -> CredentialManifest {
    CredentialManifest {
        source: source.to_string(),
        mappings: names
            .iter()
            .map(|n| CredentialMapping {
                env_var: n.clone(),
                target: classify_credential(n),
                had_value: false,
            })
            .collect(),
        vault_written: Vec::new(),
    }
}

// ===========================================================================
// Sessions
// ===========================================================================

/// A batch of imported transcript files waiting to be indexed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionImport {
    pub source: String,
    pub files: Vec<PathBuf>,
    /// Written next to the transcripts.
    pub manifest: PathBuf,
    /// Transcript formats we recognised.
    pub formats: Vec<String>,
}

/// Recognise a transcript file by shape and count its records, without
/// parsing the content into anything that could carry a secret onward.
pub fn transcript_format(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".jsonl") {
        return Some("jsonl");
    }
    if name.ends_with(".json") {
        return Some("json");
    }
    None
}

/// Count records in a JSONL transcript without retaining them.
pub fn count_jsonl_records(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|b| b.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

// ===========================================================================
// Writers — the bridge artefacts
// ===========================================================================

fn werr(code: &str, cause: String) -> PantheonError {
    cerr(
        code,
        cause,
        "check the data dir is writable and the source file still exists",
    )
}

/// Write a Pantheon-shaped MCP declaration. Transport, command, args, and url
/// carry over; credentials become declared requirements.
pub fn write_mcp_declaration(
    targets_data_dir: &Path,
    source: &str,
    servers: &[McpServer],
) -> Result<PathBuf, PantheonError> {
    if servers.is_empty() {
        return Ok(PathBuf::new());
    }
    let dir = targets_data_dir.join("mcp");
    std::fs::create_dir_all(&dir).map_err(|e| werr("MIGRATE_MCP_MKDIR", e.to_string()))?;
    let path = dir.join(format!("{}.json", source));
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "source": source,
        "note": "Generated by `pantheon migrate apply`. Transport/command/args carry over; \
                 credentials are declared, not copied. Register with `pantheon mcp`.",
        "servers": servers,
    }))
    .map_err(|e| werr("MIGRATE_MCP_ENCODE", e.to_string()))?;
    std::fs::write(&path, body).map_err(|e| werr("MIGRATE_MCP_WRITE", e.to_string()))?;
    Ok(path)
}

/// Write the credential manifest. **Names and targets only** — the file
/// deliberately has no field a value could occupy.
pub fn write_credential_manifest(
    targets_data_dir: &Path,
    source: &str,
    manifest: &CredentialManifest,
) -> Result<PathBuf, PantheonError> {
    let dir = targets_data_dir.join("credentials");
    std::fs::create_dir_all(&dir).map_err(|e| werr("MIGRATE_CRED_MKDIR", e.to_string()))?;
    let path = dir.join(format!("{}.json", source));
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "source": source,
        "note": "Names only. No value in this file, by construction. Values live in \
                 <data_dir>/.env, which is pantheon's own key store. Manage them \
                 with `pantheon model` or by hand.",
        "mappings": manifest.mappings,
    }))
    .map_err(|e| werr("MIGRATE_CRED_ENCODE", e.to_string()))?;
    std::fs::write(&path, body).map_err(|e| werr("MIGRATE_CRED_WRITE", e.to_string()))?;
    Ok(path)
}

// ===========================================================================
// .env  ->  <data_dir>/.env
// ===========================================================================

/// Where a credential value landed, per key. **Names only** — this struct has
/// no field a value could occupy, deliberately.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvMergeReport {
    /// `<data_dir>/.env`, the file that was written.
    pub path: String,
    /// Written into the store by this migration.
    pub added: Vec<String>,
    /// Already present in `<data_dir>/.env`; left untouched. A migration must
    /// never clobber a key the operator already set in Pantheon.
    pub already_present: Vec<String>,
    /// Declared but empty or a placeholder in the source; nothing to carry.
    pub no_value: Vec<String>,
    /// Not a credential we recognise, so not carried.
    pub unclassified: Vec<String>,
}

/// The dotenv path Pantheon already uses, so an imported key is visible to
/// `load_dotenv` and `pantheon model` with no extra wiring.
pub fn pantheon_env_path(data_dir: &Path) -> PathBuf {
    data_dir.join(".env")
}

/// Read `KEY=value` pairs out of a dotenv file, last-wins per key. Mirrors
/// `pantheon-cli`'s `parse_dotenv` so a key set twice behaves the same here.
pub fn read_dotenv(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let t = t.strip_prefix("export ").unwrap_or(t);
        let Some((k, v)) = t.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        let value = dotenv_value(v);
        let v = value.as_str();
        match out.iter_mut().find(|(ek, _)| ek == k) {
            Some(slot) => slot.1 = v.to_string(),
            None => out.push((k.to_string(), v.to_string())),
        }
    }
    out
}

/// The key a dotenv line sets, ignoring comments and blanks.
fn dotenv_line_key(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let t = t.strip_prefix("export ").unwrap_or(t);
    t.split_once('=').map(|(k, _)| k.trim().to_string())
}

/// Insert or replace one `KEY=value`, preserving every other line and its
/// order. A hand-edited file with duplicate keys collapses to one, matching
/// `pantheon-cli::dotenv::upsert_dotenv`, which this mirrors.
fn upsert_dotenv(path: &Path, key: &str, value: &str) -> std::io::Result<()> {
    let rendered = format!("{key}={value}");
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.is_empty() {
        std::fs::write(path, format!("{rendered}\n"))?;
        return Ok(());
    }
    let mut replaced = false;
    let mut lines: Vec<String> = Vec::new();
    for raw in existing.lines() {
        if dotenv_line_key(raw).as_deref() == Some(key) {
            if !replaced {
                lines.push(rendered.clone());
                replaced = true;
            }
        } else {
            lines.push(raw.to_string());
        }
    }
    if !replaced {
        lines.push(rendered);
    }
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(path, text)
}

/// Owner-only, matching `restrict_permissions` in `pantheon-cli::dotenv`.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// Carry a source `.env` into Pantheon's own key store.
///
/// Rules, in order of importance:
/// - Only keys that classify as credentials are carried. A source `PATH` or
///   `HOME` does not belong in a key store.
/// - **An existing key is never clobbered.** It is reported as
///   `already_present` and the operator decides. A migration that overwrites a
///   key the operator set in Pantheon is a data-loss bug.
/// - A value is written to `<data_dir>/.env` and nowhere else. It is never
///   returned in this report, never logged, and never printed.
pub fn merge_env_into(
    data_dir: &Path,
    source: &str,
    source_env: &Path,
) -> Result<EnvMergeReport, PantheonError> {
    let body = std::fs::read_to_string(source_env).map_err(|e| {
        cerr(
            "MIGRATE_ENV_READ",
            format!("{}: {e}", source_env.display()),
            "check the source env file is readable",
        )
    })?;
    // Values are needed here and nowhere else; they go straight to the store.
    let entries = parse_env_values(&body);

    std::fs::create_dir_all(data_dir).map_err(|e| werr("MIGRATE_ENV_MKDIR", e.to_string()))?;
    let dest = pantheon_env_path(data_dir);
    let mut report = EnvMergeReport {
        path: dest.to_string_lossy().to_string(),
        ..Default::default()
    };

    let existing_keys: Vec<String> = read_dotenv(&dest).into_iter().map(|(k, _)| k).collect();

    for e in entries {
        if classify_credential(&e.name) == CredentialTarget::Unclassified {
            report.unclassified.push(e.name);
            continue;
        }
        let Some(value) = e.value else {
            report.no_value.push(e.name);
            continue;
        };
        if existing_keys.contains(&e.name) {
            report.already_present.push(e.name);
            continue;
        }
        upsert_dotenv(&dest, &e.name, &value)
            .map_err(|e| werr("MIGRATE_ENV_WRITE", format!("{}: {e}", dest.display())))?;
        report.added.push(e.name);
    }

    if !report.added.is_empty() {
        restrict_permissions(&dest);
    }

    // The names-only manifest rides alongside, so the import is auditable
    // without ever opening the value store.
    let manifest = credential_manifest(source, &report.added);
    write_credential_manifest(data_dir, source, &manifest)?;

    Ok(report)
}

/// One read-back of a written MCP declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpDeclaration {
    pub source: String,
    pub servers: Vec<McpServer>,
}

/// Read every MCP declaration under `<data_dir>/mcp/`, sorted by source.
pub fn read_mcp_declarations(data_dir: &Path) -> Vec<McpDeclaration> {
    let dir = data_dir.join("mcp");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let doc: Option<Doc> = std::fs::read_to_string(&p)
            .ok()
            .and_then(|b| serde_json::from_str(&b).ok());
        if let Some(d) = doc {
            if !d.servers.is_empty() {
                out.push(McpDeclaration {
                    source: d.source,
                    servers: d.servers,
                });
            }
        }
    }
    out
}

/// The on-disk shape of a written declaration.
#[derive(Debug, Clone, Deserialize)]
struct Doc {
    #[serde(default)]
    source: String,
    #[serde(default)]
    servers: Vec<McpServer>,
}

/// Copy transcript files into a quarantine path and write a manifest, so they
/// can be indexed into `session_search` deliberately.
///
/// Returns the number of files copied. Non-transcript files are ignored.
pub fn write_session_import(
    targets_data_dir: &Path,
    source: &str,
    from: &Path,
) -> Result<SessionImport, PantheonError> {
    let dir = targets_data_dir.join("imported-sessions").join(source);
    std::fs::create_dir_all(&dir).map_err(|e| werr("MIGRATE_SESS_MKDIR", e.to_string()))?;

    let mut files = Vec::new();
    let mut formats: Vec<String> = Vec::new();
    collect_transcripts(from, &dir, "", &mut files, &mut formats, 0)?;

    let manifest = dir.join("manifest.json");
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "source": source,
        "from": from.to_string_lossy(),
        "note": "Quarantined transcripts. Index them with session_search deliberately; \
                 the ledger owns live transcripts and will not read this path.",
        "files": files.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
        "formats": formats,
    }))
    .map_err(|e| werr("MIGRATE_SESS_ENCODE", e.to_string()))?;
    std::fs::write(&manifest, body).map_err(|e| werr("MIGRATE_SESS_WRITE", e.to_string()))?;

    Ok(SessionImport {
        source: source.to_string(),
        files,
        manifest,
        formats,
    })
}

/// Depth-limited recursive transcript collection. Bounded so a runaway or
/// symlinked source tree cannot turn one import into an unbounded copy.
fn collect_transcripts(
    from: &Path,
    into: &Path,
    rel_prefix: &str,
    files: &mut Vec<PathBuf>,
    formats: &mut Vec<String>,
    depth: usize,
) -> Result<(), PantheonError> {
    if depth > 4 {
        return Ok(());
    }
    let rd = match std::fs::read_dir(from) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    for entry in rd.flatten() {
        let src = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue; // never follow out of the source tree
        }
        if ft.is_dir() {
            let child = entry.file_name().to_string_lossy().to_string();
            let next = if rel_prefix.is_empty() {
                child
            } else {
                format!("{rel_prefix}__{child}")
            };
            collect_transcripts(&src, into, &next, files, formats, depth + 1)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let Some(fmt) = transcript_format(&src) else {
            continue;
        };
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "record".into());
        // Flatten to a unique name. A bare `d{depth}-{name}` collides when two
        // subdirectories at the same depth hold a transcript with the same
        // file name, and the second copy silently overwrites the first.
        //
        // The prefix is the path *relative to the scan root*, accumulated as
        // the recursion descends. It must never be rebuilt from the absolute
        // source path: `Path::components()` yields `RootDir` for an absolute
        // path, and joining that in produces a leading "/", which makes
        // `into.join(prefix)` discard `into` entirely and resolve back to the
        // source file.
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}__{name}")
        };
        let safe: String = rel
            .chars()
            .map(|c| {
                if c == '/' || c == '\\' || c == ':' {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let mut dest = into.join(&safe);
        // Belt and braces: never clobber, even if a name somehow repeats.
        let mut n = 2;
        while dest.exists() {
            let ext = std::path::Path::new(&safe)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            dest = into.join(format!("{safe}-{n}{ext}"));
            n += 1;
        }
        std::fs::copy(&src, &dest)
            .map_err(|e| werr("MIGRATE_SESS_COPY", format!("{}: {e}", src.display())))?;
        files.push(dest);
        let tag = format!("{fmt}:d{depth}");
        if !formats.contains(&tag) {
            formats.push(tag);
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "carry_tests.rs"]
mod tests;
