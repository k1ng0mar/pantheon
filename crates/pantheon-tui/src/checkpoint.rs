//! `/checkpoint`, `/checkpoints`, `/restore` - named durable session snapshots.
//!
//! A checkpoint is a `CheckpointCreated { run_id, turn_id, name }` ledger
//! event: append-only and durable across restarts, following the
//! `TurnRewound` pattern. `/restore` rewinds to a checkpoint by emitting
//! `TurnRewound` at the first turn *after* the checkpoint's turn, reusing
//! the same marker-then-truncate path as double-Esc rewind.
//!
//! The pure cores (`effective_turn_ids`, `list_checkpoints`, `plan_restore`)
//! operate on replayed ledger entries so they are directly testable.

use pantheon_api::events::Event;
use pantheon_storage::LedgerEntry;
use std::sync::Arc;

use crate::session::{BlockKind, TuiState};
use pantheon_runtime::session::Session;

/// One checkpoint as listed by `/checkpoints`.
///
/// `turn_no` is 1-based in the effective (post-rewind) turn order; `0`
/// means the checkpoint's turn is no longer in the effective history
/// (it was rewound over) and the checkpoint cannot be restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointInfo {
    pub name: String,
    pub turn_id: String,
    pub turn_no: usize,
}

/// A validated restore: what to rewind and where to truncate the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub name: String,
    /// 1-based turn number kept (the checkpoint's turn).
    pub keep_turn_no: usize,
    /// 1-based turn number of the first dropped turn.
    pub drop_from_turn_no: usize,
    /// Turn id to name in the `TurnRewound` marker: the first turn after
    /// the checkpoint. Naming it (rather than the checkpoint turn)
    /// keeps the checkpoint turn itself in the replay.
    pub rewind_target: String,
    pub dropped: usize,
}

/// Turn ids of the effective history, in order (1-based by position).
pub fn effective_turn_ids(entries: &[LedgerEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        })
        .collect()
}

/// All checkpoints in the effective history, oldest first.
///
/// `turn_no == 0` means the checkpoint's turn is not in the supplied
/// entries. That happens only for raw (un-replayed) entries: on the
/// `/checkpoints` path the entries come from `replay()`, which applies
/// `TurnRewound` by truncating at the rewound turn - so a checkpoint
/// rewound over vanishes from the list entirely instead of appearing
/// with `turn_no == 0`. The `0` branch stays as the honest answer for
/// raw-entry callers (and for `plan_restore`'s error path); it is not
/// reachable via `/checkpoints` today, and the old doc claiming it was
/// is corrected here.
pub fn list_checkpoints(entries: &[LedgerEntry]) -> Vec<CheckpointInfo> {
    let turns = effective_turn_ids(entries);
    entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::CheckpointCreated { name, turn_id, .. } => {
                let turn_no = turns
                    .iter()
                    .position(|t| t == turn_id)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                Some(CheckpointInfo {
                    name: name.clone(),
                    turn_id: turn_id.clone(),
                    turn_no,
                })
            }
            _ => None,
        })
        .collect()
}

/// True when a checkpoint with this name already exists in the effective history.
pub fn checkpoint_exists(entries: &[LedgerEntry], name: &str) -> bool {
    list_checkpoints(entries).iter().any(|c| c.name == name)
}

/// Next free auto-name: `cp-1`, `cp-2`, ... (skips names already taken).
pub fn next_auto_name(entries: &[LedgerEntry]) -> String {
    let mut n = list_checkpoints(entries).len() + 1;
    loop {
        let candidate = format!("cp-{n}");
        if !checkpoint_exists(entries, &candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Validate a restore: resolve the checkpoint, ensure its turn is still in
/// the effective history and that there is at least one later turn to drop.
pub fn plan_restore(entries: &[LedgerEntry], name: &str) -> Result<RestorePlan, String> {
    let turns = effective_turn_ids(entries);
    let cp = list_checkpoints(entries)
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| format!("no checkpoint named '{name}'"))?;
    let idx = turns
        .iter()
        .position(|t| t == &cp.turn_id)
        .ok_or_else(|| format!("checkpoint '{name}' was rewound over and cannot be restored"))?;
    if idx + 1 >= turns.len() {
        return Err(format!("already at checkpoint '{name}'; nothing to rewind"));
    }
    let keep_turn_no = idx + 1;
    Ok(RestorePlan {
        name: name.to_string(),
        keep_turn_no,
        drop_from_turn_no: keep_turn_no + 1,
        rewind_target: turns[idx + 1].clone(),
        dropped: turns.len() - keep_turn_no,
    })
}

/// Block index of the Nth (1-based) user message in the live view.
fn user_message_block(blocks: &[crate::session::TranscriptBlock], turn_no: usize) -> Option<usize> {
    let mut n = 0;
    for (i, b) in blocks.iter().enumerate() {
        if matches!(b.kind, BlockKind::UserMessage(_)) {
            n += 1;
            if n == turn_no {
                return Some(i);
            }
        }
    }
    None
}

fn replay_entries(state: &mut TuiState, session: &Arc<Session>) -> Option<Vec<LedgerEntry>> {
    match session.supervisor.replay(&state.session_id) {
        Ok(e) => Some(e),
        Err(err) => {
            state.add_status(format!("ledger replay failed: {err}"));
            None
        }
    }
}

/// `/checkpoint [name]` - snapshot the current turn under a durable name.
pub fn cmd_checkpoint(state: &mut TuiState, session: &Arc<Session>, cmd: &str) {
    if !state.ready || state.active_run.is_some() {
        state.add_status("/checkpoint: wait for the turn to finish".into());
        return;
    }
    let arg = cmd.strip_prefix("/checkpoint").unwrap_or("").trim();
    let entries = match replay_entries(state, session) {
        Some(e) => e,
        None => return,
    };
    let turns = effective_turn_ids(&entries);
    let Some(turn_id) = turns.last().cloned() else {
        state.add_status("/checkpoint: no turn yet to snapshot".into());
        return;
    };
    let name = if arg.is_empty() {
        next_auto_name(&entries)
    } else {
        if arg.contains(char::is_whitespace) || arg.is_empty() {
            state.add_status("usage: /checkpoint [name]  (no spaces in name)".into());
            return;
        }
        arg.to_string()
    };
    if checkpoint_exists(&entries, &name) {
        state.add_status(format!("checkpoint '{name}' already exists"));
        return;
    }
    if let Err(e) = session.supervisor.emit(Event::CheckpointCreated {
        run_id: state.session_id.clone(),
        turn_id,
        name: name.clone(),
    }) {
        state.add_status(format!("/checkpoint: ledger write failed: {e}"));
        return;
    }
    let turn_no = turns.len();
    state.add_status(format!("checkpoint '{name}' saved at turn {turn_no}"));
}

/// `/checkpoints` - list checkpoints for this session.
pub fn cmd_checkpoints(state: &mut TuiState, session: &Arc<Session>) {
    let entries = match replay_entries(state, session) {
        Some(e) => e,
        None => return,
    };
    let cps = list_checkpoints(&entries);
    if cps.is_empty() {
        state.add_status("no checkpoints - /checkpoint [name] to save one".into());
        return;
    }
    state.add_status(format!("checkpoints ({}):", cps.len()));
    for cp in cps {
        if cp.turn_no == 0 {
            state.add_status(format!(
                "  {} - turn rewound over (cannot restore)",
                cp.name
            ));
        } else {
            state.add_status(format!("  {} - turn {}", cp.name, cp.turn_no));
        }
    }
}

/// `/restore <name>` - rewind the session to a checkpoint.
pub fn cmd_restore(state: &mut TuiState, session: &Arc<Session>, cmd: &str) {
    if !state.ready || state.active_run.is_some() || state.pending_approval.is_some() {
        state.add_status("/restore: wait for the turn to finish".into());
        return;
    }
    let name = cmd.strip_prefix("/restore").unwrap_or("").trim();
    if name.is_empty() {
        state.add_status("usage: /restore <name>".into());
        return;
    }
    let entries = match replay_entries(state, session) {
        Some(e) => e,
        None => return,
    };
    let plan = match plan_restore(&entries, name) {
        Ok(p) => p,
        Err(e) => {
            state.add_status(format!("/restore: {e}"));
            return;
        }
    };
    // Resolve the view truncation BEFORE emitting the marker: the marker
    // write gates the truncation, and a view that cannot be mapped must
    // not leave the ledger ahead of what the operator sees.
    let Some(drop_block) = user_message_block(&state.blocks, plan.drop_from_turn_no) else {
        state.add_status(format!(
            "/restore: turn {} not found in view; ledger untouched",
            plan.drop_from_turn_no
        ));
        return;
    };
    super::session::rewind_to_turn(
        state,
        session,
        &plan.rewind_target,
        drop_block,
        plan.dropped as u32,
        &format!("checkpoint '{}' (turn {})", plan.name, plan.keep_turn_no),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: i64, event: Event) -> LedgerEntry {
        LedgerEntry {
            id: seq,
            run_id: "r".to_string(),
            seq,
            ts_ms: seq * 1000,
            event,
        }
    }

    fn turn(turn_id: &str) -> Event {
        Event::TurnStarted {
            run_id: "r".to_string(),
            turn_id: turn_id.to_string(),
        }
    }

    fn checkpoint(name: &str, turn_id: &str) -> Event {
        Event::CheckpointCreated {
            run_id: "r".to_string(),
            turn_id: turn_id.to_string(),
            name: name.to_string(),
        }
    }

    fn history() -> Vec<LedgerEntry> {
        vec![
            entry(1, turn("t1")),
            entry(2, checkpoint("cp-a", "t1")),
            entry(3, turn("t2")),
            entry(4, checkpoint("cp-b", "t2")),
            entry(5, turn("t3")),
        ]
    }

    #[test]
    fn list_checkpoints_numbers_in_effective_order() {
        let cps = list_checkpoints(&history());
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[0].turn_no, 1);
        assert_eq!(cps[1].turn_no, 2);
        assert!(checkpoint_exists(&history(), "cp-a"));
        assert!(!checkpoint_exists(&history(), "cp-z"));
    }

    #[test]
    fn list_checkpoints_zero_for_turn_missing_from_raw_entries() {
        // Raw entries (no replay truncation): a checkpoint whose turn
        // is absent reports turn_no == 0 instead of being dropped.
        let entries = vec![entry(1, checkpoint("cp-orphan", "t9"))];
        let cps = list_checkpoints(&entries);
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].turn_no, 0);
    }

    #[test]
    fn plan_restore_keeps_checkpoint_turn_and_drops_later() {
        let plan = plan_restore(&history(), "cp-a").unwrap();
        assert_eq!(plan.keep_turn_no, 1);
        assert_eq!(plan.drop_from_turn_no, 2);
        assert_eq!(plan.rewind_target, "t2");
        assert_eq!(plan.dropped, 2);
    }

    #[test]
    fn plan_restore_rejects_rewound_over_and_at_tip() {
        let orphan = vec![entry(1, checkpoint("cp-orphan", "t9"))];
        assert!(plan_restore(&orphan, "cp-orphan").is_err());
        assert!(plan_restore(&history(), "nope").is_err());
        // cp-b sits on t2 with t3 after it, so restoring it is valid
        // and drops exactly t3 (an earlier draft of this test wrongly
        // claimed cp-b was at the tip).
        let plan = plan_restore(&history(), "cp-b").unwrap();
        assert_eq!(plan.keep_turn_no, 2);
        assert_eq!(plan.drop_from_turn_no, 3);
        assert_eq!(plan.rewind_target, "t3");
        assert_eq!(plan.dropped, 1);
        // A checkpoint on the last turn has nothing later to drop.
        let mut at_tip = history();
        at_tip.push(entry(6, checkpoint("cp-c", "t3")));
        assert!(plan_restore(&at_tip, "cp-c").is_err());
    }
}
