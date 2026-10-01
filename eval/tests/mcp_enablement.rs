//! Behavioral tests for MCP/plugin enablement defaults and the bundled
//! catalog seam.
//!
//! The rule under test: a fresh install enables zero bundled MCPs and
//! zero bundled plugins, and enabling is always explicit. Concretely:
//!
//! - `[mcp.servers.<name>]` without `enabled` parses as **disabled**
//!   (schema default `false`), for bundled and custom servers alike.
//! - `[plugins.<name>]` without `enabled` parses as **disabled**.
//! - The canonical catalog (`pantheon_api::mcp_catalog`) is the single
//!   recipe definition: the `pantheon_mcp::bundled` facade exposes the
//!   same recipes with byte-identical materialization, and both the
//!   dashboard write path and the agent-tool write path materialize
//!   through the one canonical writer — no first-writer-wins divergence.
//! - `pinned_version` is stamped only for recipes with a real package
//!   pin, never fabricated for unpinned (remote) recipes.
//!
//! Policy: only small deterministic unit tests live beside the code.
//! Everything behavioral — filesystem, config parsing — lives here and
//! runs via `cargo test -p pantheon-eval`.

use pantheon_api::config::{Config, McpSection};
use pantheon_api::mcp_catalog;
use pantheon_mcp::bundled as sibling;
use tempfile::tempdir;

/// `[mcp.servers.<name>]` without `enabled` parses as disabled: the
/// schema default is `false` for every server, bundled or custom.
#[test]
fn mcp_server_entry_defaults_disabled() {
    let cfg: Config = toml::from_str(
        r#"
[mcp.servers.myserver]
transport = "stdio"
command = "my-server"
"#,
    )
    .expect("config parses");
    let entry = cfg.mcp.as_ref().unwrap().servers.get("myserver").unwrap();
    assert!(!entry.enabled, "absent `enabled` must parse as disabled");
    assert!(!cfg.mcp_server_enabled("myserver"));
}

/// Absent table = disabled, for catalog names and unknown names alike.
#[test]
fn absent_server_is_disabled() {
    let cfg = Config::default();
    assert!(!cfg.mcp_server_enabled("github"));
    assert!(!cfg.mcp_server_enabled("no-such-server"));
}

/// Upgrade behavior, pinned as a deliberate choice: a table written
/// before the default existed (no `enabled` key, previously parsed as
/// enabled) now parses as disabled. Fail-safe direction — the operator
/// re-enables once, explicitly. No migration rewrites user configs to
/// `enabled = true`.
#[test]
fn legacy_table_without_enabled_is_disabled_on_upgrade() {
    let cfg: Config = toml::from_str(
        r#"
[mcp.servers.legacy]
transport = "http"
url = "https://example.com/mcp"
"#,
    )
    .expect("legacy config parses");
    assert!(
        !cfg.mcp_server_enabled("legacy"),
        "pre-change tables without `enabled` must come up disabled, not silently stay on"
    );
}

/// Explicit opt-in still works: `enabled = true` enables.
#[test]
fn explicit_enabled_true_still_enables() {
    let cfg: Config = toml::from_str(
        r#"
[mcp.servers.myserver]
transport = "stdio"
command = "my-server"
enabled = true
"#,
    )
    .expect("config parses");
    assert!(cfg.mcp_server_enabled("myserver"));
}

/// The sibling catalog's pure helper materializes a default-disabled
/// entry first, then flips the flag. Unknown names are rejected.
#[test]
fn sibling_set_bundled_enabled_materializes_disabled_first() {
    let mut section = McpSection::default();
    sibling::set_bundled_enabled(&mut section, "github", false).expect("catalog name");
    let entry = section.servers.get("github").expect("entry materialized");
    assert!(!entry.enabled, "materialized entry must start disabled");
    assert_eq!(entry.transport, "stdio");
    assert_eq!(entry.command.as_deref(), Some("docker"));
    // Secrets become env refs, never literals.
    assert_eq!(
        entry
            .env
            .get("GITHUB_PERSONAL_ACCESS_TOKEN")
            .map(String::as_str),
        Some("env:GITHUB_PERSONAL_ACCESS_TOKEN")
    );

    sibling::set_bundled_enabled(&mut section, "github", true).expect("enable");
    assert!(section.servers["github"].enabled);

    let err = sibling::set_bundled_enabled(&mut section, "arbitrary", true).unwrap_err();
    assert_eq!(
        err,
        sibling::BundledError::UnknownServer("arbitrary".to_string()),
        "non-catalog names must be rejected"
    );
}

/// The sibling catalog reports a bundled recipe with no config table
/// as disabled; unknown names are `None` (inert).
#[test]
fn sibling_absent_is_disabled() {
    let section = McpSection::default();
    assert_eq!(sibling::is_bundled_enabled(&section, "github"), Some(false));
    assert_eq!(
        sibling::is_bundled_enabled(&section, "no-such-server"),
        None
    );
}

/// #7: one canonical catalog. The `pantheon_mcp::bundled` facade must
/// expose exactly the canonical recipes, and materializing through the
/// facade must produce the identical config entry as the canonical
/// recipe — no drift between the dashboard path and the agent path.
/// (Fails before the fix: the two copies diverge, e.g. playwright ships
/// without `-y` in the launcher copy and notion is unpinned there.)
#[test]
fn canonical_catalog_materialization_matches() {
    for server in mcp_catalog::bundled_catalog() {
        let mut section = McpSection::default();
        sibling::set_bundled_enabled(&mut section, &server.name, true)
            .unwrap_or_else(|_| panic!("facade missing {}", server.name));
        let via_facade = section
            .servers
            .get(&server.name)
            .cloned()
            .expect("materialized");
        let mut via_api = server.to_config_entry();
        via_api.enabled = true;
        assert_eq!(
            via_facade, via_api,
            "materialization drift for {}",
            server.name
        );
    }
    assert_eq!(
        mcp_catalog::bundled_catalog().len(),
        sibling::bundled_catalog().len(),
        "catalog sizes must match"
    );
}

/// #7: `pinned_version` is stamped only for recipes with a real package
/// pin — never fabricated for unpinned (remote) recipes. `cloudflare`
/// is the remote-only recipe: no package exists, so no stamp may be
/// written. (Fails before the fix: the writer stamps every table,
/// including the unpinned one.)
#[test]
fn no_false_pinned_version_for_unpinned_recipes() {
    let dir = tempdir().unwrap();
    mcp_catalog::set_enabled(dir.path(), "cloudflare", true).expect("enable");
    let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(
        !text.contains("pinned_version"),
        "unpinned recipe must not get a fabricated pin stamp:\n{text}"
    );

    // A pinned recipe keeps its stamp.
    let dir = tempdir().unwrap();
    mcp_catalog::set_enabled(dir.path(), "github", true).expect("enable");
    let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(
        text.contains("pinned_version"),
        "pinned recipe keeps its stamp"
    );
}

/// The agent path's write: materializes the pinned row disabled, flips
/// on enable, preserves the rest of the row. Unknown names rejected.
#[test]
fn api_seam_set_enabled_roundtrip() {
    let dir = tempdir().unwrap();
    mcp_catalog::set_enabled(dir.path(), "github", false).expect("materialize disabled");
    let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    let cfg: Config = toml::from_str(&text).expect("written config parses");
    let entry = cfg.mcp.as_ref().unwrap().servers.get("github").unwrap();
    assert!(!entry.enabled, "fresh materialization must be disabled");
    assert_eq!(entry.command.as_deref(), Some("docker"));
    assert!(text.contains("pinned_version"), "pin stamp written");

    mcp_catalog::set_enabled(dir.path(), "github", true).expect("enable");
    let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    let cfg: Config = toml::from_str(&text).expect("written config parses");
    let entry = cfg.mcp.as_ref().unwrap().servers.get("github").unwrap();
    assert!(entry.enabled, "explicit enable flips the flag");
    assert_eq!(entry.command.as_deref(), Some("docker"), "row preserved");

    let err = mcp_catalog::set_enabled(dir.path(), "arbitrary", true).unwrap_err();
    assert_eq!(err.code, "MCP_UNKNOWN_SERVER");
}

/// `[plugins.<name>]` without `enabled` parses as disabled: bundled
/// plugins ship off, and the config file is the single enablement state.
#[test]
fn plugin_entry_defaults_disabled() {
    let cfg: Config = toml::from_str(
        r#"
[plugins.time-gap]
kind = "tool"
"#,
    )
    .expect("config parses");
    assert!(
        !cfg.plugin_enabled("time-gap"),
        "absent `enabled` must parse as disabled"
    );
    assert!(!cfg.plugin_enabled("no-such-plugin"));

    let cfg: Config = toml::from_str(
        r#"
[plugins.time-gap]
enabled = true
"#,
    )
    .expect("config parses");
    assert!(cfg.plugin_enabled("time-gap"), "explicit opt-in works");
}
