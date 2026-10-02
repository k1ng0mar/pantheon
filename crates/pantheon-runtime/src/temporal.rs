//! Ledger-shape helpers for tacit temporal awareness.
//!
//! The pure hint decision lives in `pantheon_api::temporal`; this module
//! only knows how to read "when was the assistant last active" out of
//! replayed ledger entries. Kept separate so the decision logic stays
//! dependency-free and trivially testable.

use pantheon_api::events::Event;
use pantheon_api::message::Role;
use pantheon_storage::LedgerEntry;

/// Timestamp (ms) of the most recent assistant-side activity in replayed
/// entries: the last assistant-role message or tool result. Tool traffic
/// counts because it is part of the assistant's turn - a turn that ends
/// mid-tool-use still means the conversation was alive.
///
/// `None` when there is no assistant activity yet (new session), which the
/// caller treats as "stay silent".
pub fn last_assistant_ts_ms(entries: &[LedgerEntry]) -> Option<i64> {
    entries.iter().rev().find_map(|e| match &e.event {
        Event::AssistantMessage { message, .. } if message.role == Role::Assistant => Some(e.ts_ms),
        Event::ToolMessage { .. } => Some(e.ts_ms),
        _ => None,
    })
}
