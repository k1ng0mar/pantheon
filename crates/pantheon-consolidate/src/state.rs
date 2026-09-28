//! Persisted consolidation state: `<data_dir>/consolidation/state.json`.
//!
//! Incremental passes remember where they left off (`last_run_ms`) and
//! which keys were already promoted (`promoted_keys`), so the scan
//! window advances and reruns stay idempotent even if the promotion
//! check misses.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// On-disk state for incremental passes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsolidationState {
    /// Wall-clock ms of the last completed pass. The next pass stages
    /// events at or after this timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<i64>,
    /// Summary line of the last pass, surfaced by `--status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<String>,
    /// Staged count from the last completed (non-dry-run) pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_staged: Option<usize>,
    /// Promoted count from the last completed (non-dry-run) pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_promoted: Option<usize>,
    /// Keys promoted by previous passes. A belt-and-braces duplicate
    /// guard behind the store-level idempotency check.
    #[serde(default)]
    pub promoted_keys: Vec<String>,
}

fn state_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("consolidation").join("state.json")
}

/// Load state; a missing or corrupt file is "no state yet", never fatal.
pub fn load(data_dir: &Path) -> Result<ConsolidationState, String> {
    let path = state_path(data_dir);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConsolidationState::default())
        }
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Save state, creating the directory as needed.
pub fn save(data_dir: &Path, state: &ConsolidationState) -> Result<(), String> {
    let path = state_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| format!("encode state: {e}"))?;
    std::fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}
