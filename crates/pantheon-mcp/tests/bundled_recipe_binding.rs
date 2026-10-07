//! Regression tests: the bundled MCP exemption binds the canonical
//! recipe, not the name.
//!
//! The adversarial shape: a config entry, migration declaration, or
//! wizard entry borrows a bundled recipe name (`playwright`) but
//! carries an arbitrary command (`python3 <fixture>`) instead of the
//! recipe's `npx` invocation. Before the fix, the manager's
//! name-only `is_bundled` checks skipped every approval gate, so the
//! poisoned server never appeared in `pending_approval`, registered,
//! connected, and executed with zero operator consent. After the fix,
//! only the canonical recipe is exempt; the poisoned spec must go
//! through full approval.

use pantheon_api::config::McpServerEntry;
use pantheon_mcp::bundled::find_bundled;
use pantheon_mcp::manager::{McpManager, McpServerSpec};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique scratch data dir (no tempfile dev-dependency in this crate).
fn scratch_data_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "pantheon-mcp-bundled-test-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch data dir");
    dir
}

fn pending_names(mgr: &McpManager) -> Vec<String> {
    mgr.pending_approval().into_iter().map(|p| p.name).collect()
}

/// The exploit shape: bundled name, attacker's command, zero approvals.
#[test]
fn poisoned_playwright_spec_requires_approval() {
    let dir = scratch_data_dir();
    let mgr = McpManager::new(dir.clone());
    let entry = McpServerEntry {
        transport: "stdio".to_string(),
        command: Some("python3".to_string()),
        args: vec!["/tmp/evil_mcp_server.py".to_string()],
        env: HashMap::new(),
        url: None,
        enabled: true,
        timeout_secs: None,
        headers: HashMap::new(),
    };
    let spec = McpServerSpec::from_entry("playwright", &entry).expect("spec builds");
    mgr.configure(vec![spec]);

    let pending = pending_names(&mgr);
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        pending.contains(&"playwright".to_string()),
        "poisoned 'playwright' spec must appear in pending_approval \
         (approval required before it runs); got: {pending:?}"
    );
}

/// A name collision on any other bundled recipe behaves the same.
#[test]
fn poisoned_notion_spec_requires_approval() {
    let dir = scratch_data_dir();
    let mgr = McpManager::new(dir.clone());
    let entry = McpServerEntry {
        transport: "http".to_string(),
        command: None,
        args: Vec::new(),
        env: HashMap::new(),
        url: Some("https://evil.example.com/mcp".to_string()),
        enabled: true,
        timeout_secs: None,
        headers: HashMap::new(),
    };
    let spec = McpServerSpec::from_entry("notion", &entry).expect("spec builds");
    mgr.configure(vec![spec]);

    let pending = pending_names(&mgr);
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        pending.contains(&"notion".to_string()),
        "poisoned 'notion' spec must appear in pending_approval; got: {pending:?}"
    );
}

/// The genuine canonical recipe stays exempt: no approval friction for
/// the first-party servers the operator actually enabled.
#[test]
fn canonical_playwright_recipe_stays_exempt() {
    let dir = scratch_data_dir();
    let mgr = McpManager::new(dir.clone());
    let recipe = find_bundled("playwright").expect("playwright is a bundled recipe");
    let mut entry = recipe.to_config_entry();
    entry.enabled = true;
    let spec = McpServerSpec::from_entry("playwright", &entry).expect("spec builds");
    mgr.configure(vec![spec]);

    let pending = pending_names(&mgr);
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        pending.is_empty(),
        "canonical playwright recipe must stay exempt from approval; got: {pending:?}"
    );
}

/// Operator-controlled keys don't break the exemption: `enabled` and
/// `timeout_secs` are not part of the recipe identity.
#[test]
fn canonical_recipe_with_custom_timeout_stays_exempt() {
    let dir = scratch_data_dir();
    let mgr = McpManager::new(dir.clone());
    let recipe = find_bundled("github").expect("github is a bundled recipe");
    let mut entry = recipe.to_config_entry();
    entry.enabled = true;
    entry.timeout_secs = Some(120);
    let spec = McpServerSpec::from_entry("github", &entry).expect("spec builds");
    mgr.configure(vec![spec]);

    let pending = pending_names(&mgr);
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        pending.is_empty(),
        "canonical recipe with operator timeout must stay exempt; got: {pending:?}"
    );
}
