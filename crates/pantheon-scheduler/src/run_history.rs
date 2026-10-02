//! Per-job run history: consecutive-failure counters for the nightly
//! repair loop.
//!
//! The tick driver reports each fired run's [`RunOutcome`] through a
//! channel; nothing durable recorded it (the gateway loop dropped the
//! receiver). [`RunHistory`] is the durable sink: one JSON file,
//! `<data_dir>/schedule-run-history.json`, mapping job id → counters.
//! `Completed` resets the consecutive-failure count; `TimedOut`,
//! `Panicked`, and `Failed` (app-level task failure) increment it;
//! `Replaced` (a superseded generation) is neutral - a replace is not a
//! failure.
//!
//! The nightly repair loop reads these counters through its
//! `ScheduleRepairTarget` adapter: a job with
//! `consecutive_failures >= schedule_max_failures` is broken and gets
//! paused + escalated.

use crate::tick::RunOutcome;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Path of the run-history file: `<data_dir>/schedule-run-history.json`.
pub fn run_history_path(data_dir: &Path) -> PathBuf {
    data_dir.join("schedule-run-history.json")
}

/// Counters for one job's recent runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobRunStats {
    /// Consecutive `TimedOut`/`Panicked` outcomes. Reset by `Completed`.
    pub consecutive_failures: u32,
    /// "completed" | "timed_out" | "panicked" | "replaced".
    pub last_outcome: String,
    pub last_error: Option<String>,
    pub last_run_ms: i64,
    pub total_runs: u64,
    pub total_failures: u64,
}

/// Durable per-job run history. The map is the whole file: `open`,
/// mutate via [`RunHistory::record`], and every mutation persists.
pub struct RunHistory {
    path: PathBuf,
    stats: HashMap<String, JobRunStats>,
}

impl RunHistory {
    /// Open (creating if missing) on `<data_dir>`. A corrupt file is an
    /// error naming the file - a silently empty history would hide
    /// failing jobs from the repair loop.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let path = run_history_path(data_dir);
        let stats = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("cannot parse {}: {e}", path.display()))?,
        };
        Ok(Self { path, stats })
    }

    /// Record one fired run's outcome and persist. `error` carries the
    /// executor's failure detail for `TimedOut`/`Panicked` runs.
    pub fn record(
        &mut self,
        job_id: &str,
        outcome: RunOutcome,
        error: Option<String>,
        at_ms: i64,
    ) -> Result<(), String> {
        let st = self.stats.entry(job_id.to_string()).or_default();
        st.total_runs += 1;
        st.last_run_ms = at_ms;
        match outcome {
            RunOutcome::Completed => {
                st.consecutive_failures = 0;
                st.last_outcome = "completed".to_string();
                st.last_error = None;
            }
            RunOutcome::TimedOut | RunOutcome::Panicked | RunOutcome::Failed => {
                st.consecutive_failures = st.consecutive_failures.saturating_add(1);
                st.total_failures += 1;
                st.last_outcome = match outcome {
                    RunOutcome::TimedOut => "timed_out",
                    RunOutcome::Failed => "failed",
                    _ => "panicked",
                }
                .to_string();
                st.last_error = error;
            }
            RunOutcome::Replaced => {
                // A superseded generation is not a failure: leave the
                // counters alone, record the outcome.
                st.last_outcome = "replaced".to_string();
            }
        }
        self.save()
    }

    /// This job's counters, if it has any recorded runs.
    pub fn stats(&self, job_id: &str) -> Option<&JobRunStats> {
        self.stats.get(job_id)
    }

    /// Drop one job's history (job deleted or repaired). Persists.
    pub fn reset(&mut self, job_id: &str) -> Result<(), String> {
        self.stats.remove(job_id);
        self.save()
    }

    /// Drop history for jobs no longer in the store, so deleted jobs
    /// don't accumulate rows forever. Persists.
    pub fn retain<'a>(&mut self, active: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
        let keep: std::collections::HashSet<&str> = active.into_iter().collect();
        self.stats.retain(|id, _| keep.contains(id.as_str()));
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(&self.stats)
            .map_err(|e| format!("encode history: {e}"))?;
        std::fs::write(&self.path, text).map_err(|e| format!("write {}: {e}", self.path.display()))
    }
}
