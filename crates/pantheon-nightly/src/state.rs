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
    /// Last pass outcome, exactly one of `ok` | `skipped` | `failed`
    /// (never a vague label). `skipped` = dry run; `failed` = the pass
    /// errored before finishing. Empty in state files written before
    /// this field existed; [`NightlyState::load`] normalizes those
    /// (a recorded pass completed, so it was `ok`, or `skipped` for a
    /// recorded dry run) instead of surfacing a blank label.
    #[serde(default)]
    pub last_outcome: String,
    /// Why the last pass was `skipped` or `failed`; empty for `ok`.
    #[serde(default)]
    pub last_outcome_reason: String,
}

impl NightlyState {
    /// Load the last-pass state. A torn file loads as the default, with
    /// a loud warning: state is written atomically (see [`Self::save`]),
    /// so a torn file can only predate atomic writes, and a state file
    /// that errors forever would hide every later pass from status
    /// surfaces. The summary is recomputed next pass; nothing durable
    /// besides the summary lives here.
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(state_path(data_dir)) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(state) => Ok(Self::normalize(state)),
                Err(e) => {
                    eprintln!(
                        "nightly: state {} is torn ({e}); treating as never-run",
                        state_path(data_dir).display()
                    );
                    Ok(Self::default())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("read nightly state: {e}")),
        }
    }

    /// Fill in the outcome for state files written before the field
    /// existed. Only a completed pass ever wrote state (failures are
    /// recorded separately since this change), so a recorded pass with
    /// no outcome was a success - or a dry run, which is `skipped`.
    fn normalize(mut state: Self) -> Self {
        if state.last_outcome.is_empty() && state.last_run_ms > 0 {
            if state.last_dry_run {
                state.last_outcome = "skipped".to_string();
                state.last_outcome_reason = "dry run: no changes applied".to_string();
            } else {
                state.last_outcome = "ok".to_string();
            }
        }
        state
    }

    /// Save atomically (temp file + rename), so a crash mid-write never
    /// leaves a torn state file for [`Self::load`] to trip over.
    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        let path = state_path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("encode nightly state: {e}"))?;
        crate::apply::atomic_write(&path, &text)
            .map_err(|e| format!("write {}: {e}", path.display()))
    }
}

// Small deterministic invariant tests only.

#[cfg(test)]
mod tests {
    use super::*;

    /// Legacy state files (no outcome field) normalize to the only
    /// honest label: a recorded pass completed, so `ok` - or
    /// `skipped` for a recorded dry run. A fresh default (never ran)
    /// stays blank, and the dashboard renders "no passes yet" for it.
    #[test]
    fn normalize_fills_legacy_outcome() {
        let mut legacy = NightlyState::default();
        legacy.last_run_ms = 1_790_449_200_000;
        let norm = NightlyState::normalize(legacy);
        assert_eq!(norm.last_outcome, "ok");
        assert!(norm.last_outcome_reason.is_empty());

        let mut dry = NightlyState::default();
        dry.last_run_ms = 1_790_449_200_000;
        dry.last_dry_run = true;
        let norm = NightlyState::normalize(dry);
        assert_eq!(norm.last_outcome, "skipped");
        assert_eq!(norm.last_outcome_reason, "dry run: no changes applied");

        let never = NightlyState::normalize(NightlyState::default());
        assert!(never.last_outcome.is_empty());
    }
}
