//! Durable pass state: what ran, when, and what changed.
//!
//! `<data_dir>/nightly/nightly-state.json` records the last pass so the
//! TUI and the dashboard can show "nightly last ran at ..., proposed N,
//! applied M" without re-running the pipeline.

use std::path::{Path, PathBuf};

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("nightly").join("nightly-state.json")
}

/// The last nightly pass, in durable form.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NightlyState {
    pub last_run_ms: i64,
    pub last_runs_scanned: usize,
    pub last_signals: usize,
    pub last_proposals: usize,
    pub last_applied: usize,
    pub last_pending: usize,
    pub last_dry_run: bool,
    /// Repair targets contained (disabled/paused) by the last pass.
    #[serde(default)]
    pub last_repairs_contained: usize,
    /// Repair targets fixed by the last pass.
    #[serde(default)]
    pub last_repairs_fixed: usize,
}

impl NightlyState {
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(state_path(data_dir)) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| format!("parse nightly state: {e}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("read nightly state: {e}")),
        }
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        let path = state_path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("encode nightly state: {e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

// Small deterministic invariant tests only.
