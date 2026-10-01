//! Approved persona notes → live session injection.
//!
//! Persona proposals never auto-apply: they pass eval-gating and
//! replay-gating, then wait for explicit human approval. Approval writes
//! the note to the memory store's `persona` namespace (agent layer, key
//! `nightly-persona-<topic>`). This module is the read half: the
//! runtime loads those notes into the system prompt of fresh runs, so an
//! approved persona actually shapes future sessions.
//!
//! Trust-tier rule, enforced by construction: [`crate::apply::decide`]
//! with `approve = true` is the only writer to this namespace, so
//! everything read here passed evals, passed replay validation, and got
//! a human yes. The injector itself stays dumb on purpose.

use pantheon_memory::MemoryBackend;

/// Memory namespace holding approved persona notes.
pub const PERSONA_NAMESPACE: &str = "persona";

/// Key prefix for nightly-written persona notes.
pub const PERSONA_KEY_PREFIX: &str = "nightly-persona-";

/// Approved persona notes in stable write order: (key, text). Empty
/// notes are dropped; a read failure degrades to empty (fail-open —
/// a broken memory read must never block a turn). Reads through the
/// session's active backend, whatever it is.
pub fn approved_notes(store: &dyn MemoryBackend) -> Vec<(String, String)> {
    store
        .list_agent(PERSONA_NAMESPACE)
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| k.starts_with(PERSONA_KEY_PREFIX))
        .filter(|(_, v)| !v.trim().is_empty())
        .collect()
}

/// Format approved persona notes as a system-prompt block. Empty string
/// when there are no notes — the caller appends nothing.
pub fn overlay_block(notes: &[(String, String)]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "<nightly_persona>\nThe operator has approved these persona notes, learned from past sessions. Treat them as standing guidance:\n",
    );
    for (key, text) in notes {
        let topic = key.strip_prefix(PERSONA_KEY_PREFIX).unwrap_or(key);
        out.push_str(&format!("- [{topic}] {text}\n"));
    }
    out.push_str("</nightly_persona>");
    out
}

// Small deterministic invariant tests only.
