//! Event retention for the scheduler daemon.
//!
//! The retention pass used to live only in `pantheon schedule tick
//! --watch` (`pantheon-tui/src/schedule.rs`): a gateway daemon that
//! never runs the TUI tick loop would accumulate ledger events, search
//! chunks, and idempotency claims forever. This module is the daemon's
//! copy — same 24h gate (tracked in `retention.json`), same prune
//! primitives, same "active runs are never pruned" rule. Kept as a
//! separate implementation rather than shared with the TUI because the
//! TUI copy is being retired with the TUI tick loop; until it is, a
//! change to the prune rule belongs in both.
//!
//! Wire-up: the gateway daemon should call [`maybe_run_retention`]
//! from its tick loop (e.g. `pantheon-gateway/src/daemon.rs`).

use crate::durable::DurableClaimLedger;
use pantheon_api::config::{Config, DEFAULT_RETENTION_DAYS};
use std::path::{Path, PathBuf};

/// At most one retention pass per interval; the last-pass timestamp
/// lives in `<data_dir>/retention.json`.
const RETENTION_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

/// What one retention pass pruned.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RetentionReport {
    pub keep_days: u32,
    pub events_pruned: usize,
    pub search_chunks_pruned: usize,
    pub claims_pruned: usize,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run the retention pass now: prune ledger events, the FTS search
/// sidecar, and idempotency claims older than `keep_days`. Runs whose
/// status is not terminal are never pruned, however old their events —
/// the active run's transcript is what a resume rebuilds from.
pub fn run_retention(data_dir: &Path, keep_days: u32) -> Result<RetentionReport, String> {
    let cutoff = now_ms() - keep_days as i64 * 86_400_000;
    let mut report = RetentionReport {
        keep_days,
        ..Default::default()
    };

    let ledger_path = data_dir.join("ledger.db");
    if ledger_path.exists() {
        let ledger = pantheon_storage::ledger::Ledger::open(&ledger_path)
            .map_err(|e| format!("retention: cannot open ledger: {e}"))?;
        report.events_pruned = ledger
            .prune_events_before_active_safe(cutoff)
            .map_err(|e| format!("retention: prune events: {e}"))?;
        // The FTS sidecar lives in the same database file.
        match pantheon_storage::search::SessionSearch::open(&ledger_path) {
            Ok(search) => {
                report.search_chunks_pruned = search
                    .prune_before(cutoff)
                    .map_err(|e| format!("retention: prune search index: {e}"))?;
            }
            Err(e) => eprintln!("retention: search index unavailable, skipping: {e}"),
        }
    }
    let claims_path = data_dir.join("claims.db");
    if claims_path.exists() {
        let claims = DurableClaimLedger::open(&claims_path)
            .map_err(|e| format!("retention: cannot open claim ledger: {e}"))?;
        report.claims_pruned = claims
            .prune_before(cutoff)
            .map_err(|e| format!("retention: prune claims: {e}"))?;
    }
    Ok(report)
}

fn retention_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("retention.json")
}

/// True when a retention pass is due: never ran, or the last pass is
/// older than `RETENTION_INTERVAL_MS`.
fn retention_due(data_dir: &Path) -> bool {
    let last: i64 = std::fs::read_to_string(retention_state_path(data_dir))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("last_prune_ms")?.as_i64())
        .unwrap_or(0);
    now_ms() - last >= RETENTION_INTERVAL_MS
}

fn record_retention_pass(data_dir: &Path) {
    let state = serde_json::json!({ "last_prune_ms": now_ms() });
    let _ = std::fs::write(retention_state_path(data_dir), state.to_string());
}

/// The automatic pass, for the gateway daemon's tick loop. No-op when
/// retention is disabled (`keep_days = 0`) or the last pass is fresh.
/// Logs what was pruned.
pub fn maybe_run_retention(data_dir: &Path) {
    let keep_days = Config::load_or_report(data_dir)
        .map(|c| c.retention_days())
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    if keep_days == 0 || !retention_due(data_dir) {
        return;
    }
    match run_retention(data_dir, keep_days) {
        Ok(r) => {
            record_retention_pass(data_dir);
            println!(
                "retention: pruned {} events, {} search chunks, {} claims older than {}d (active runs kept)",
                r.events_pruned, r.search_chunks_pruned, r.claims_pruned, r.keep_days
            );
        }
        Err(e) => eprintln!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_api::events::Event;
    use pantheon_storage::ledger::Ledger;

    fn data_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Empty data dir: nothing to open, nothing pruned, no error.
    #[test]
    fn run_retention_empty_dir_is_noop() {
        let dir = data_dir();
        let r = run_retention(dir.path(), 90).expect("retention on empty dir");
        assert_eq!(r.keep_days, 90);
        assert_eq!(r.events_pruned, 0);
        assert_eq!(r.search_chunks_pruned, 0);
        assert_eq!(r.claims_pruned, 0);
    }

    /// Fresh data is kept: a completed run's recent events and a fresh
    /// claim are all newer than the cutoff, so the pass prunes nothing
    /// but still runs cleanly end to end.
    #[test]
    fn run_retention_keeps_fresh_data() {
        let dir = data_dir();
        let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("ledger");
        ledger
            .append(&Event::RunStarted {
                run_id: "r1".to_string(),
            })
            .expect("RunStarted");
        ledger
            .append(&Event::RunCompleted {
                run_id: "r1".to_string(),
            })
            .expect("RunCompleted");
        drop(ledger);
        let claims = DurableClaimLedger::open(&dir.path().join("claims.db")).expect("claims");
        assert!(claims.claim("occ-1").expect("claim"));
        drop(claims);

        let r = run_retention(dir.path(), 90).expect("retention");
        assert_eq!(r.events_pruned, 0, "fresh events kept");
        assert_eq!(r.claims_pruned, 0, "fresh claims kept");

        let claims = DurableClaimLedger::open(&dir.path().join("claims.db")).expect("claims");
        assert!(claims.is_claimed("occ-1").expect("is_claimed"));
    }

    /// The 24h gate: the first pass records `retention.json`; a second
    /// call while the pass is fresh is a no-op (state file untouched).
    #[test]
    fn maybe_run_retention_gates_on_24h() {
        let dir = data_dir();
        maybe_run_retention(dir.path());
        let state_path = dir.path().join("retention.json");
        let first = std::fs::read_to_string(&state_path).expect("retention.json written");
        maybe_run_retention(dir.path());
        let second = std::fs::read_to_string(&state_path).expect("retention.json still there");
        assert_eq!(first, second, "fresh pass must not re-run");
    }

    /// `keep_days = 0` disables retention: no pass, no state file.
    #[test]
    fn maybe_run_retention_disabled_when_keep_days_zero() {
        let dir = data_dir();
        std::fs::write(
            dir.path().join("config.toml"),
            "[retention]\nkeep_days = 0\n",
        )
        .expect("config");
        maybe_run_retention(dir.path());
        assert!(
            !dir.path().join("retention.json").exists(),
            "disabled retention must not record a pass"
        );
    }
}
