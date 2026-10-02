//! The canonical bundled MCP server catalog: the ONE recipe definition
//! every surface consumes.
//!
//! ## Why the canonical recipe lives here (not in `pantheon-mcp`)
//!
//! The launcher owns `pantheon-mcp`, but the dependency edges run
//! `pantheon-mcp -> pantheon-tools -> pantheon-api`: the agent's
//! `enable_mcp` tool (in `pantheon-tools`) needs the catalog names, pins,
//! and the file-persisting [`set_enabled`] write. Placing the canonical
//! recipe in `pantheon-mcp` would force a `tools -> mcp` edge - a
//! dependency cycle, since the edge runs `mcp -> tools`. So the recipe
//! data lives in the leaf both depend on (`pantheon-api`), and
//! `pantheon_mcp::bundled` is a thin facade re-exporting
//! [`BundledMcpServer`] plus the launcher-side pure helpers. There is
//! exactly one copy of the recipe data; the old "two copies + drift
//! test" arrangement is gone by construction.
//!
//! ## What this module guarantees
//!
//! - [`BundledMcpServer`] rows: the single source of truth for package
//!   pins, launch shapes, secret names, and placement notes.
//! - [`BundledMcpServer::to_config_entry`]: the single materialization
//!   of a recipe into a `[mcp.servers.<name>]` entry (disabled by
//!   default).
//! - [`materialize_recipe_table`]: the single TOML-table writer. Both
//!   the agent path ([`set_enabled`]) and the dashboard path use it, so
//!   one canonical recipe always materializes to byte-identical tables
//!   no matter which surface enabled the server first
//!   first-writer-wins divergence is impossible. `pinned_version` is
//!   stamped only when the recipe carries a real package pin; unpinned
//!   (remote) recipes get no stamp - a fabricated pin would lie about
//!   what code is approved to run.
//! - [`set_enabled`] / [`is_enabled`]: the single enablement state
//!   `[mcp.servers.<name>]` in `<data_dir>/config.toml` - rejecting
//!   non-catalog names. This is the no-arbitrary-MCP-command boundary
//!   for the agent path: the agent can never get an unvetted binary onto
//!   the command line through it.
//!
//! ## MCPs vs plugins: the trust distinction
//!
//! (Mirrors `pantheon_extensions::bundled`.) A bundled MCP server is an
//! out-of-process integration: Pantheon spawns or connects to it and
//! speaks the MCP protocol. Pantheon never executes the server's code,
//! which may run on another machine entirely - trust is in the endpoint
//! and its configuration (command, URL, env), not in shipped code.
//! Bundled MCPs are first-party, so they skip the third-party approval
//! store ([`crate::approval`]); they do **not** skip enablement. A custom
//! (non-catalog) MCP server additionally needs operator consent recorded
//! in `<data_dir>/mcp/.approvals.json` before the launcher connects.
//!
//! ## Content-hash binding
//!
//! The launcher's content hash binds the package spec (`name@pin`) plus
//! the full argument list for launcher-shim servers (`npx`, `uvx`,
//! `docker`, ...), so a pin bump or an arg change lapses approval. For
//! shims the hash binds the *requested* spec, not the bytes the registry
//! served - registry-fetched code can change under a pin, so treat the
//! approval as binding intent, not bytes.

use crate::config::{Config, McpServerEntry};
use crate::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Extension,
        false,
        cause,
        "check the server name against the bundled MCP catalog",
        "",
    )
}

/// One MCP server Pantheon ships: the canonical bundled recipe. All
/// entries are disabled by default - [`is_enabled`] is false for a name
/// with no `[mcp.servers.<name>]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundledMcpServer {
    /// Catalog name. Matches the `[mcp.servers.<name>]` config key.
    pub name: String,
    /// Display label for the pin, e.g. `@playwright/mcp@0.0.83`.
    /// Remote-only recipes (no package) use `"remote <url>"`.
    pub version: String,
    /// The true pinned package spec, e.g. `@playwright/mcp@0.0.83`.
    /// `None` when there is no bundleable package (Cloudflare: remote
    /// only, the npm package is stale and deprecated upstream).
    /// This - and only this - is what `pinned_version` stamps.
    pub package: Option<String>,
    /// One-paragraph operator-facing summary.
    pub description: String,
    /// `stdio`, `sse`, or `http`.
    pub transport: String,
    /// Program to spawn (stdio only).
    pub command: Option<String>,
    /// Arguments for the program (stdio only). Pins live here too, so a
    /// pin bump changes the materialized entry (and lapses approval via
    /// the content hash).
    pub args: Vec<String>,
    /// Endpoint URL (sse/http only).
    pub url: Option<String>,
    /// Environment variable names the server needs. Materialized as
    /// `env = { NAME = "env:NAME" }` so secrets resolve from the
    /// operator's environment at spawn time - never stored in config.
    pub requires_env: Vec<String>,
    /// Non-secret env vars Pantheon sets by default when launching the
    /// server (e.g. telemetry opt-outs). Stored as literal values.
    pub default_env: Vec<(String, String)>,
    /// Operator-facing setup notes: accounts, OAuth clients, scopes,
    /// prerequisites the recipe alone does not capture.
    pub setup_notes: String,
    /// True when the server process runs on the operator's machine and
    /// calls only cloud APIs; false when the server itself is hosted by a
    /// third party (Cloudflare Code Mode).
    pub local_only: bool,
    /// Upstream deprecation/sunset warnings the operator should see
    /// before enabling.
    pub deprecated_warning: Option<String>,
    /// What enabling this server actually authorizes, in plain terms.
    /// Shown by approval surfaces before the operator decides.
    pub privilege_notes: String,
}

impl BundledMcpServer {
    /// The `[mcp.servers.<name>]` table this catalog row materializes.
    /// Disabled by default: enabling is always an explicit write through
    /// [`set_enabled`] (agent path) or the dashboard path - both funnel
    /// through [`materialize_recipe_table`], so the entry is identical
    /// whichever surface wrote it.
    ///
    /// Secrets become `env:NAME` placeholders (resolved from the
    /// operator's environment at spawn time; values are never stored).
    /// `default_env` pairs are stored as literal values.
    pub fn to_config_entry(&self) -> McpServerEntry {
        let mut env = HashMap::new();
        for (key, value) in &self.default_env {
            env.insert(key.clone(), value.clone());
        }
        for name in &self.requires_env {
            env.insert(name.clone(), format!("env:{name}"));
        }
        McpServerEntry {
            transport: self.transport.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env,
            url: self.url.clone(),
            enabled: false,
            timeout_secs: None,
        }
    }
}

/// One catalog row. Pins verified 2026-09-29 against official
/// docs/registries (`docs/bundled-mcp-research.md`).
///
/// Canonical decisions, made once here (previously the two copies
/// disagreed): every `npx` invocation carries `-y` (without it npx
/// prompts on piped stdin and hangs); every registry package is pinned
/// in the arg list (an unpinned arg would float to latest under a pinned
/// claim); `google-workspace` runs `workspace-mcp==1.30.0`.
fn catalog_rows() -> Vec<BundledMcpServer> {
    vec![
        BundledMcpServer {
            name: "github".to_string(),
            version: "ghcr.io/github/github-mcp-server:v1.12.2".to_string(),
            package: Some("ghcr.io/github/github-mcp-server:v1.12.2".to_string()),
            description: "GitHub's official MCP server: repos, issues, PRs, actions, \
                packages and more, driven through toolsets. Runs as a local Docker \
                container calling the GitHub API."
                .to_string(),
            transport: "stdio".to_string(),
            command: Some("docker".to_string()),
            args: vec![
                "run".to_string(),
                "-i".to_string(),
                "--rm".to_string(),
                "-e".to_string(),
                "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                "ghcr.io/github/github-mcp-server:v1.12.2".to_string(),
            ],
            url: None,
            requires_env: vec![
                "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                "GITHUB_OAUTH_CALLBACK_PORT".to_string(),
                "GITHUB_HOST".to_string(),
            ],
            default_env: vec![],
            setup_notes: "A classic PAT needs at minimum `repo`, `read:org` and \
                `read:packages` scopes; grant only what your toolsets need. \
                Alternatively use the browser OAuth flow: set \
                GITHUB_OAUTH_CALLBACK_PORT=8085 and pass -p 127.0.0.1:8085:8085 \
                to docker (the official image bundles app credentials; the \
                resulting token is kept in memory only). For GitHub Enterprise \
                Server or *.ghe.com data residency set GITHUB_HOST. Use \
                GITHUB_TOOLSETS / GITHUB_TOOLS to restrict the exposed tools; \
                --read-only is available for audit-safe use. No-Docker fallback: \
                build from source (go build ./cmd/github-mcp-server) and run \
                with args [\"stdio\"]."
                .to_string(),
            local_only: true,
            deprecated_warning: None,
            privilege_notes: "Spawns a Docker container running GitHub's official \
                MCP server (pinned image v1.12.2). It can read and act on your \
                GitHub account with whatever scopes your personal access token \
                grants - grant only the scopes the task needs. First-party \
                bundle entry; the container image is third-party code run by \
                Docker, not by Pantheon."
                .to_string(),
        },
        BundledMcpServer {
            name: "google-workspace".to_string(),
            version: "workspace-mcp==1.30.0".to_string(),
            package: Some("workspace-mcp==1.30.0".to_string()),
            description: "Community Google Workspace MCP server \
                (taylorwilsdon/google_workspace_mcp): one local server covering \
                Gmail, Drive, Calendar, Docs, Sheets, Slides, Forms, Tasks, \
                Contacts, Chat and more, calling the Google cloud APIs."
                .to_string(),
            transport: "stdio".to_string(),
            command: Some("uvx".to_string()),
            args: vec!["workspace-mcp==1.30.0".to_string()],
            url: None,
            requires_env: vec![
                "GOOGLE_OAUTH_CLIENT_ID".to_string(),
                "GOOGLE_OAUTH_CLIENT_SECRET".to_string(),
            ],
            default_env: vec![],
            setup_notes: "You must create your own OAuth client in your own \
                Google Cloud project and enable the API for each service you use; \
                the redirect URI must match, e.g. \
                http://localhost:${WORKSPACE_MCP_PORT}/oauth2callback. Launch \
                tiers: uvx workspace-mcp --tool-tier core|extended|complete, or \
                cherry-pick --tools gmail drive calendar; --read-only and \
                per-service --permissions are available. Maintainer caution: \
                emails/docs can carry hidden prompt-injection instructions - be \
                deliberate with write tools. Google Chat needs a one-time Chat \
                app configuration and a Workspace account (not free Gmail). Do \
                NOT install the CLI via `uvx workspace-cli` - that PyPI name is \
                squatted by an abandoned package."
                .to_string(),
            local_only: true,
            deprecated_warning: None,
            privilege_notes: "Spawns the community workspace-mcp server (pinned \
                PyPI release 1.30.0) via uvx. It acts on your Google account \
                through your own OAuth client: mail, calendar, drive files, \
                and documents are all in reach of its tools. Treat its outputs \
                as untrusted - emails and docs can carry hidden instructions."
                .to_string(),
        },
        BundledMcpServer {
            name: "chrome-devtools".to_string(),
            version: "chrome-devtools-mcp@1.10.1".to_string(),
            package: Some("chrome-devtools-mcp@1.10.1".to_string()),
            description: "Chrome DevTools MCP: drive a local Chrome for automation, \
                debugging, performance traces and screenshots. No secrets; needs \
                Chrome and Node.js LTS on this machine."
                .to_string(),
            transport: "stdio".to_string(),
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "chrome-devtools-mcp@1.10.1".to_string()],
            url: None,
            requires_env: vec![],
            default_env: vec![
                (
                    "CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS".to_string(),
                    "1".to_string(),
                ),
                (
                    "CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS".to_string(),
                    "1".to_string(),
                ),
            ],
            setup_notes: "Requires a current stable Google Chrome installed on \
                the same machine plus Node.js LTS; the server auto-starts Chrome \
                on the first tool call that needs a browser. Privacy: usage \
                statistics and npm update checks are ON upstream by default - \
                Pantheon disables both via the bundled env above. Performance \
                tools call the Google CrUX API for field data (add --no-performance-crux \
                to disable). Useful flags: --headless, --slim (3 tools only), \
                --browser-url=http://127.0.0.1:9222 to attach to a running \
                Chrome, --user-data-dir, --channel, --executablePath."
                .to_string(),
            local_only: true,
            deprecated_warning: None,
            privilege_notes: "Spawns the official Chrome DevTools MCP server \
                (pinned npm release 1.10.1) via npx. It drives your installed \
                Chrome: navigation, script execution, screenshots. Usage \
                statistics and update checks phone home to Google unless \
                disabled; performance tools call the CrUX API."
                .to_string(),
        },
        BundledMcpServer {
            name: "cloudflare".to_string(),
            version: "remote https://mcp.cloudflare.com/mcp".to_string(),
            package: None,
            description: "Cloudflare Code Mode MCP: broad Cloudflare API coverage \
                via Cloudflare's own hosted remote endpoint. Auth is a per-user \
                browser OAuth flow - there is no static secret to configure."
                .to_string(),
            transport: "http".to_string(),
            command: None,
            args: vec![],
            url: Some("https://mcp.cloudflare.com/mcp".to_string()),
            requires_env: vec![],
            default_env: vec![],
            setup_notes: "Fully remote and cloud-hosted by Cloudflare: nothing \
                runs on this machine. First connect opens a browser OAuth flow \
                where you pick the Cloudflare account; some features require a \
                paid Workers plan. Domain-specific servers also exist \
                (*.mcp.cloudflare.com/mcp: docs, bindings, builds, \
                observability, containers, browser, logs, ai-gateway, autorag, \
                dns-analytics, dex, casb, radar, blog). The npm package \
                @cloudflare/mcp-server-cloudflare is stale (0.2.0, March 2025) \
                and the local server implementations are deprecated upstream - \
                they are deliberately NOT bundled. Clients without remote \
                support can shim with npx mcp-remote <url>."
                .to_string(),
            local_only: false,
            deprecated_warning: None,
            privilege_notes: "Connects to Cloudflare's hosted MCP endpoint \
                (streamable HTTP). Browser OAuth on first connect; its tools \
                act on your Cloudflare account (Workers, DNS, zones). No \
                local code runs - trust is in the endpoint and your account."
                .to_string(),
        },
        BundledMcpServer {
            name: "playwright".to_string(),
            version: "@playwright/mcp@0.0.83".to_string(),
            package: Some("@playwright/mcp@0.0.83".to_string()),
            description: "Playwright's official browser MCP: structured web \
                automation over Chromium/Firefox/WebKit running locally. No \
                secrets; Node.js 18+ required."
                .to_string(),
            transport: "stdio".to_string(),
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "@playwright/mcp@0.0.83".to_string()],
            url: None,
            requires_env: vec![],
            default_env: vec![],
            setup_notes: "Requires Node.js 18+. Browsers (Chromium by default) \
                are downloaded by Playwright on the first run. Options: \
                --browser chrome|firefox|webkit|msedge, --caps vision,pdf,devtools, \
                --cdp-endpoint <url> to drive an external browser, --user-data-dir \
                for persistent profiles. Filesystem access is restricted to \
                workspace roots by default (--allow-unrestricted-file-access \
                relaxes); host/origin allow-lists via --allowed-hosts / \
                --allowed-origins (PLAYWRIGHT_MCP_* env equivalents exist for \
                every flag)."
                .to_string(),
            local_only: true,
            deprecated_warning: None,
            privilege_notes: "Spawns the official Playwright MCP server \
                (pinned npm release 0.0.83) via npx. It drives a real \
                browser (Chromium by default) on your machine: pages it \
                visits run arbitrary web content. Filesystem access is \
                restricted to workspace roots unless relaxed."
                .to_string(),
        },
        BundledMcpServer {
            name: "notion".to_string(),
            version: "@notionhq/notion-mcp-server@2.5.2".to_string(),
            package: Some("@notionhq/notion-mcp-server@2.5.2".to_string()),
            description: "Official Notion MCP server: query and edit pages and \
                data sources through an internal integration token. Local server \
                calling the Notion API."
                .to_string(),
            transport: "stdio".to_string(),
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@notionhq/notion-mcp-server@2.5.2".to_string(),
            ],
            url: None,
            requires_env: vec!["NOTION_TOKEN".to_string()],
            default_env: vec![],
            setup_notes: "Create an internal integration at \
                notion.so/profile/integrations, then explicitly share pages and \
                databases with it (integration Access tab, or per-page \
                \"Connect to integration\") - unshared content is invisible to \
                the server. v2.0.0 is a breaking release (Notion API \
                2025-09-03): database tools were renamed to data-source tools, \
                e.g. post-database-query -> query-data-source; 22 tools total."
                .to_string(),
            local_only: true,
            deprecated_warning: Some(
                "Upstream maintainers state this repository is no longer actively \
                maintained or supported and may be sunset in the future in favor \
                of the hosted Remote Notion MCP. Bundled at 2.5.2 with eyes open; \
                plan a remote-Notion path."
                    .to_string(),
            ),
            privilege_notes: "Spawns Notion's official MCP server (pinned npm \
                release 2.5.2) via npx. It reads and writes the Notion pages \
                and databases you shared with its integration, using your \
                integration token. Note: Notion has marked this repo \
                unmaintained and may sunset it - plan a remote-Notion path."
                .to_string(),
        },
    ]
}

/// Every MCP server Pantheon ships, sorted by name. Deterministic order
/// so dashboard, app, and wizard render the same list.
pub fn bundled_catalog() -> Vec<BundledMcpServer> {
    let mut out = catalog_rows();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The catalog entry for `name`, or `None` when it is not bundled.
/// Only catalog names are toggleable - this is what keeps the agent
/// from proposing (or any surface from flipping) an arbitrary command.
pub fn find(name: &str) -> Option<BundledMcpServer> {
    bundled_catalog().into_iter().find(|s| s.name == name)
}

/// Fill an empty `[mcp.servers.<name>]` TOML table from the one canonical
/// recipe. This is the single writer both the agent path ([`set_enabled`])
/// and the dashboard path use: one canonical recipe in, byte-identical
/// table out, regardless of which surface enabled the server first.
///
/// `pinned_version` is stamped only when the recipe carries a real
/// package pin ([`BundledMcpServer::package`]); unpinned (remote) recipes
/// get no stamp - a fabricated pin would lie about what code is approved
/// to run. The stamp is informational (the launcher ignores unknown
/// keys).
///
/// The caller passes the table empty; keys the operator already set are
/// never overwritten because materialization only runs on empty tables.
pub fn materialize_recipe_table(
    table: &mut toml::map::Map<String, toml::Value>,
    recipe: &BundledMcpServer,
) {
    debug_assert!(
        table.is_empty(),
        "materialize_recipe_table expects an empty table"
    );
    let entry = recipe.to_config_entry();
    table.insert(
        "transport".to_string(),
        toml::Value::String(entry.transport),
    );
    if let Some(command) = entry.command {
        table.insert("command".to_string(), toml::Value::String(command));
    }
    if !entry.args.is_empty() {
        table.insert(
            "args".to_string(),
            toml::Value::Array(entry.args.into_iter().map(toml::Value::String).collect()),
        );
    }
    if let Some(url) = entry.url {
        table.insert("url".to_string(), toml::Value::String(url));
    }
    if !entry.env.is_empty() {
        let mut env_table = toml::map::Map::new();
        for (k, v) in entry.env {
            env_table.insert(k, toml::Value::String(v));
        }
        table.insert("env".to_string(), toml::Value::Table(env_table));
    }
    if let Some(pin) = &recipe.package {
        table.insert(
            "pinned_version".to_string(),
            toml::Value::String(pin.clone()),
        );
    }
}

/// Is the bundled server `name` enabled? Pure read over the config:
/// absent `[mcp.servers.<name>]` (or an entry without `enabled`) =
/// false. Unknown names are false too - inert entries never enable
/// anything.
pub fn is_enabled(config: &Config, name: &str) -> bool {
    config.mcp_server_enabled(name)
}

/// Load the config for enablement decisions without
/// [`Config::load_or_report`]'s process-exit behavior: a library must
/// never `exit(2)` the host because the config has a typo. A missing
/// config means every bundled server is disabled (fail closed); a config
/// that exists but does not parse is reported on stderr and also fails
/// closed - a typo must never silently flip a server on.
pub fn load_config(data_dir: &Path) -> Option<Config> {
    match Config::load(data_dir) {
        Ok(c) => Some(c),
        Err(e) if e.code == "CONFIG_OPEN" => None,
        Err(e) => {
            eprintln!("pantheon: bundled MCP servers disabled: {}", e.cause);
            eprintln!("pantheon: fix: {}", e.remediation);
            None
        }
    }
}

/// Write the single enablement state: set `[mcp.servers.<name>].enabled`
/// in `<data_dir>/config.toml`, materializing the canonical catalog row
/// when the table does not exist yet. Only bundled-catalog names are
/// accepted (`MCP_UNKNOWN_SERVER` otherwise) - this is the
/// no-arbitrary-command boundary: the agent can never get an unvetted
/// binary onto the command line through this path.
///
/// Enabling stamps the recipe's real package pin (`pinned_version` is
/// informational - the launcher ignores unknown keys); unpinned recipes
/// get no stamp. Disabling keeps the row and flips the flag. The
/// document is edited as TOML rather than re-serialized from the
/// [`Config`] struct, so unknown keys survive; comments and original key
/// order do not (the TOML document model drops comments and sorts keys).
pub fn set_enabled(data_dir: &Path, name: &str, enabled: bool) -> Result<(), PantheonError> {
    let server = find(name).ok_or_else(|| {
        merr(
            "MCP_UNKNOWN_SERVER",
            format!("'{name}' is not a bundled MCP server; only catalog servers can be toggled"),
        )
    })?;
    let path = Config::path(data_dir);
    let mut doc: toml::Value = match std::fs::read_to_string(&path) {
        Ok(text) => text
            .parse()
            .map_err(|e| merr("MCP_CONFIG_PARSE", format!("parse {}: {e}", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(toml::map::Map::new())
        }
        Err(e) => {
            return Err(merr(
                "MCP_CONFIG_READ",
                format!("read {}: {e}", path.display()),
            ))
        }
    };
    if !doc.is_table() {
        return Err(merr(
            "MCP_CONFIG_PARSE",
            format!("{} is not a TOML table", path.display()),
        ));
    }
    let root = doc.as_table_mut().expect("checked is_table");
    let mcp = root
        .entry("mcp")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let mcp_table = mcp.as_table_mut().ok_or_else(|| {
        merr(
            "MCP_CONFIG_PARSE",
            "config has a non-table [mcp] value".to_string(),
        )
    })?;
    let servers = mcp_table
        .entry("servers")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let servers_table = servers.as_table_mut().ok_or_else(|| {
        merr(
            "MCP_CONFIG_PARSE",
            "config has a non-table [mcp.servers] value".to_string(),
        )
    })?;
    let entry = servers_table
        .entry(name)
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let table = entry.as_table_mut().ok_or_else(|| {
        merr(
            "MCP_CONFIG_PARSE",
            format!("[mcp.servers.{name}] is not a table"),
        )
    })?;
    // Materialize the canonical row on first enable so the launcher has
    // a complete shape to spawn: never leave a bare `enabled = true`
    // with no command behind. This is the one canonical writer - the
    // dashboard path uses it too, so both surfaces produce
    // byte-identical tables.
    if table.is_empty() {
        materialize_recipe_table(table, &server);
    }
    table.insert("enabled".to_string(), toml::Value::Boolean(enabled));
    let text = toml::to_string_pretty(&doc)
        .map_err(|e| merr("MCP_CONFIG_WRITE", format!("serialize: {e}")))?;
    // Atomic: write tmp then rename, so a crash never leaves half a config.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            merr(
                "MCP_CONFIG_WRITE",
                format!("create {}: {e}", parent.display()),
            )
        })?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| merr("MCP_CONFIG_WRITE", format!("write {}: {e}", path.display())))?;
    Ok(())
}

// Small deterministic invariant tests only. Filesystem behavior
// (set_enabled round-trip, config preservation, catalog-name rejection)
// is covered in `pantheon-eval` (`eval/tests/mcp_enablement.rs`).
