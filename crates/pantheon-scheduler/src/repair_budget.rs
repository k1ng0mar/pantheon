//! Per-task daily repair budget (self-heal spend cap).
//!
//! The self-heal investigator is bounded per incident (12 turns, 24 tool
//! calls, 5 minutes), but nothing bounded how many incidents one flapping
//! task could trigger per day - a task failing every minute would burn API
//! budget on investigator sessions without bound. [`RepairBudget`] caps
//! repair investigations per task per UTC day (default 3, via
//! `[repair].max_repairs_per_task_per_day`); counts persist in
//! `<data_dir>/repair-budget.json` so restarts don't reset them.
//!
//! Wiring (tui `SelfHealer::heal`, other leaf): before spawning the
//! investigator, open the budget on the data dir and call
//! [`RepairBudget::try_consume`] with the task id and the cap from
//! [`RepairSection::repair_cap_per_day`](pantheon_api::config::RepairSection::repair_cap_per_day).
//! Skip the investigation (and surface "daily repair budget exhausted"
//! in the outcome) when it returns false.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// File name under the data dir.
const FILE_NAME: &str = "repair-budget.json";
/// UTC days of counters to keep: today plus a small tail, so a repair
/// just after midnight UTC doesn't resurrect a pruned counter.
const KEEP_DAYS: i64 = 3;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct BudgetFile {
    /// UTC day ("YYYY-MM-DD") -> task id -> repairs consumed that day.
    days: HashMap<String, HashMap<String, u32>>,
}

/// Per-task daily repair budget, persisted to
/// `<data_dir>/repair-budget.json`.
pub struct RepairBudget {
    path: PathBuf,
    file: BudgetFile,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// UTC `YYYY-MM-DD` for `now_ms`. Howard Hinnant's civil-date algorithm:
/// days since the Unix epoch -> calendar date, no chrono needed for one
/// formatting call.
fn day_string(now_ms: i64) -> String {
    let days = now_ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

impl RepairBudget {
    /// Open the budget file under `data_dir` (creating parent dirs on
    /// first persist). A missing or corrupt file starts empty - the
    /// fail-open here is deliberate and narrow: [`Self::try_consume`]
    /// fails closed on write errors, so a repair never runs uncounted.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join(FILE_NAME);
        let file: BudgetFile = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BudgetFile::default(),
            Err(e) => {
                return Err(format!(
                    "repair-budget: cannot read {}: {e}",
                    path.display()
                ))
            }
        };
        let mut budget = Self { path, file };
        budget.prune_old_days();
        Ok(budget)
    }

    /// Try to consume one repair investigation for `job_id` against
    /// `max_per_day`. Returns true when the investigation may proceed
    /// the consumption is recorded and persisted. Returns false when the
    /// task already spent its daily budget, or the cap is 0 (repair
    /// investigations disabled). A persistence failure fails closed
    /// (false): a repair must never run uncounted.
    pub fn try_consume(&mut self, job_id: &str, max_per_day: u32, now_ms: i64) -> bool {
        if max_per_day == 0 {
            return false;
        }
        let day = day_string(now_ms);
        let used = self
            .file
            .days
            .get(&day)
            .and_then(|m| m.get(job_id))
            .copied()
            .unwrap_or(0);
        if used >= max_per_day {
            return false;
        }
        self.file
            .days
            .entry(day.clone())
            .or_default()
            .insert(job_id.to_string(), used + 1);
        if self.persist().is_err() {
            // Roll back the in-memory bump: the count must never exceed
            // what survived to disk.
            if used == 0 {
                if let Some(m) = self.file.days.get_mut(&day) {
                    m.remove(job_id);
                    if m.is_empty() {
                        self.file.days.remove(&day);
                    }
                }
            } else if let Some(m) = self.file.days.get_mut(&day) {
                m.insert(job_id.to_string(), used);
            }
            return false;
        }
        true
    }

    /// Repairs consumed by `job_id` today (UTC). Observability for the
    /// "why didn't it heal?" question.
    pub fn used_today(&self, job_id: &str, now_ms: i64) -> u32 {
        let day = day_string(now_ms);
        self.file
            .days
            .get(&day)
            .and_then(|m| m.get(job_id))
            .copied()
            .unwrap_or(0)
    }

    fn persist(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&self.file)
            .map_err(|e| format!("repair-budget: serialize: {e}"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("repair-budget: mkdir: {e}"))?;
        }
        // Write-then-rename: a crash mid-write never leaves a torn file.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("repair-budget: write: {e}"))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("repair-budget: rename: {e}"))?;
        Ok(())
    }

    fn prune_old_days(&mut self) {
        // YYYY-MM-DD is zero-padded, so lexicographic order is
        // chronological order.
        let cutoff = day_string(now_ms() - (KEEP_DAYS - 1) * 86_400_000);
        self.file
            .days
            .retain(|day, _| day.as_str() >= cutoff.as_str());
    }
}

#[cfg(test)]
mod repair_budget_tests {
    use super::*;

    // Fixed instants: 2026-10-01 and 2026-10-02 00:00:00 UTC.
    const DAY1: i64 = 1_790_812_800_000;
    const DAY2: i64 = DAY1 + 86_400_000;

    fn open_temp() -> (RepairBudget, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let budget = RepairBudget::open(dir.path()).unwrap();
        (budget, dir)
    }

    #[test]
    fn consumes_up_to_cap_then_denies() {
        let (mut b, _dir) = open_temp();
        assert!(b.try_consume("task-a", 3, DAY1));
        assert!(b.try_consume("task-a", 3, DAY1));
        assert!(b.try_consume("task-a", 3, DAY1));
        assert!(!b.try_consume("task-a", 3, DAY1));
        assert_eq!(b.used_today("task-a", DAY1), 3);
    }

    #[test]
    fn budget_is_per_task() {
        let (mut b, _dir) = open_temp();
        assert!(b.try_consume("task-a", 1, DAY1));
        assert!(!b.try_consume("task-a", 1, DAY1));
        assert!(b.try_consume("task-b", 1, DAY1));
    }

    #[test]
    fn budget_resets_next_utc_day() {
        let (mut b, _dir) = open_temp();
        assert!(b.try_consume("task-a", 1, DAY1));
        assert!(!b.try_consume("task-a", 1, DAY1));
        assert!(b.try_consume("task-a", 1, DAY2));
        assert_eq!(b.used_today("task-a", DAY1), 1);
        assert_eq!(b.used_today("task-a", DAY2), 1);
    }

    #[test]
    fn zero_cap_disables() {
        let (mut b, _dir) = open_temp();
        assert!(!b.try_consume("task-a", 0, DAY1));
        assert_eq!(b.used_today("task-a", DAY1), 0);
    }

    #[test]
    fn counts_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut b = RepairBudget::open(dir.path()).unwrap();
            assert!(b.try_consume("task-a", 3, DAY1));
            assert!(b.try_consume("task-a", 3, DAY1));
        }
        // A restart must not reset the budget.
        let mut b2 = RepairBudget::open(dir.path()).unwrap();
        assert_eq!(b2.used_today("task-a", DAY1), 2);
        assert!(b2.try_consume("task-a", 3, DAY1));
        assert!(!b2.try_consume("task-a", 3, DAY1));
    }

    #[test]
    fn corrupt_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"not json{{{").unwrap();
        let mut b = RepairBudget::open(dir.path()).unwrap();
        assert!(b.try_consume("task-a", 3, DAY1));
    }

    #[test]
    fn day_string_matches_known_dates() {
        assert_eq!(day_string(DAY1), "2026-10-01");
        assert_eq!(day_string(DAY2), "2026-10-02");
        assert_eq!(day_string(0), "1970-01-01");
    }
}
