//! `/checkpoint`, `/checkpoints`, `/restore` — named durable session snapshots.
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
/// A checkpoint whose turn was rewound over still appears (the marker is
/// never rewritten away) but with `turn_no == 0` so callers can report it
/// as unrestorable instead of guessing.
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

/// Next free auto-name: `cp-1`, `cp-2`, … (skips names already taken).
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

/// `/checkpoint [name]` — snapshot the current turn under a durable name.
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

/// `/checkpoints` — list checkpoints for this session.
pub fn cmd_checkpoints(state: &mut TuiState, session: &Arc<Session>) {
    let entries = match replay_entries(state, session) {
        Some(e) => e,
        None => return,
    };
    let cps = list_checkpoints(&entries);
    if cps.is_empty() {
        state.add_status("no checkpoints — /checkpoint [name] to save one".into());
        return;
    }
    state.add_status(format!("checkpoints ({}):", cps.len()));
    for cp in cps {
        if cp.turn_no == 0 {
            state.add_status(format!(
                "  {} — turn rewound over (cannot restore)",
                cp.name
            ));
        } else {
            state.add_status(format!("  {} — turn {}", cp.name, cp.turn_no));
        }
    }
}

/// `/restore <name>` — rewind the session to a checkpoint.
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
