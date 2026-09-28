//! Phase 1 — Stage: gather candidate memories from ledger events.
//!
//! Memory-focused and minimal: only user-originated signals become
//! candidates. Behavior signals (tool sequences, repeated failures) belong
//! to Reflection's extractor, not here.
//!
//! Sources:
//! - `SteeringProvided` — the operator corrected the agent mid-turn. The
//!   text is a direct preference statement.
//! - `RunProgress` rows of the form `memory_write layer=.. ns=.. key=..
//!   backend=..` — a memory the agent wrote at session scope. The value is
//!   read back from the memory store; only TaskSession-layer writes are
//!   candidates (Agent/Project-layer rows are already long-term).
//! - `ApprovalDenied` — the operator denied a scope: a negative preference.
//!
//! Rewind projection applies: `Ledger::replay` is used per run, so
//! rewound turns never contribute candidates.

use crate::{Candidate, CandidateKind, ConsolidationConfig, Source};
use pantheon_api::error::PantheonError;
use pantheon_api::events::Event;
use pantheon_memory::{LayerKind, MemoryBackend};
use std::collections::HashMap;

/// Parse a `memory_write layer=.. ns=.. key=.. backend=..` RunProgress
/// detail into (layer, namespace, key). Returns None for anything else.
fn parse_memory_write(detail: &str) -> Option<(String, String, String)> {
    let rest = detail.strip_prefix("memory_write ")?;
    let mut layer = None;
    let mut ns = None;
    let mut key = None;
    for tok in rest.split_whitespace() {
        let (k, v) = tok.split_once('=')?;
        match k {
            "layer" => layer = Some(v.to_string()),
            "ns" => ns = Some(v.to_string()),
            "key" => key = Some(v.to_string()),
            _ => {}
        }
    }
    Some((layer?, ns?, key?))
}

/// Map the Debug-format layer name from the RunProgress annotation.
fn parse_layer(name: &str) -> Option<LayerKind> {
    match name {
        "Global" => Some(LayerKind::Global),
        "Agent" => Some(LayerKind::Agent),
        "Project" => Some(LayerKind::Project),
        "TaskSession" => Some(LayerKind::TaskSession),
        "EphemeralTurn" => Some(LayerKind::EphemeralTurn),
        _ => None,
    }
}

/// Split a memory key into FTS-friendly query tokens. Recall sanitizes
/// each whitespace-separated token (stripping `:`, `/`, …), so a raw key
/// like `lesson:foo` would collapse into one unmatchable token —
/// pre-splitting on non-alphanumeric runs keeps every part searchable.
/// The caller still exact-filters on (layer, namespace, key).
fn key_query(key: &str) -> String {
    key.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collect candidates from ledger events at or after `since_ms`.
///
/// Bounded: at most `config.max_runs_scanned` runs are scanned, most
/// recent first.
pub fn stage_candidates(
    ledger: &pantheon_storage::Ledger,
    backend: &dyn MemoryBackend,
    policy: &pantheon_api::capability::Policy,
    config: &ConsolidationConfig,
    since_ms: i64,
) -> Result<Vec<Candidate>, PantheonError> {
    // (kind-label, normalized-text) -> candidate under construction.
    // Same text from the same kind merges; different kinds stay separate
    // (a steering and a denial are different claims even with similar
    // wording).
    let mut grouped: HashMap<(String, String), Candidate> = HashMap::new();

    for (run_id, _status, _created, _title) in ledger.list_runs(config.max_runs_scanned)? {
        let entries = ledger.replay(&run_id)?;
        for entry in entries {
            if entry.ts_ms < since_ms {
                continue;
            }
            match &entry.event {
                Event::SteeringProvided { text, .. } => {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    push_candidate(
                        &mut grouped,
                        CandidateKind::Steering,
                        text.to_string(),
                        Source {
                            run_id: run_id.clone(),
                            ts_ms: entry.ts_ms,
                        },
                    );
                }
                Event::RunProgress { detail, .. } => {
                    if let Some((layer_name, ns, key)) = parse_memory_write(detail) {
                        // Only session-scoped writes are consolidation
                        // material; Agent/Project rows are already durable.
                        if parse_layer(&layer_name) != Some(LayerKind::TaskSession) {
                            continue;
                        }
                        // Read the value back from the store: the ledger
                        // only carries the key. Recall with the key's
                        // tokens as the query, then take the exact
                        // (layer, namespace, key) hit — never a fuzzy
                        // neighbor. A missing record (expired,
                        // forgotten) is not a candidate: we never invent
                        // text.
                        match backend.recall(
                            policy,
                            &[ns.as_str()],
                            &[LayerKind::TaskSession],
                            &key_query(&key),
                            10,
                        ) {
                            Ok(hits) => {
                                let exact = hits.iter().find(|h| {
                                    h.record.layer == LayerKind::TaskSession
                                        && h.record.namespace == ns
                                        && h.record.key == key
                                });
                                if let Some(hit) = exact {
                                    let text = hit.record.value.trim();
                                    if !text.is_empty() {
                                        push_candidate(
                                            &mut grouped,
                                            CandidateKind::SessionMemory,
                                            text.to_string(),
                                            Source {
                                                run_id: run_id.clone(),
                                                ts_ms: entry.ts_ms,
                                            },
                                        );
                                    }
                                }
                            }
                            // Store hiccups must not fail the pass; the
                            // candidate is simply skipped this run.
                            Err(_) => continue,
                        }
                    }
                }
                Event::ApprovalDenied { scope, .. } => {
                    let scope = scope.trim();
                    if scope.is_empty() {
                        continue;
                    }
                    push_candidate(
                        &mut grouped,
                        CandidateKind::Denial,
                        format!("operator denied approval scope: {scope}"),
                        Source {
                            run_id: run_id.clone(),
                            ts_ms: entry.ts_ms,
                        },
                    );
                }
                _ => {}
            }
        }
    }

    Ok(grouped.into_values().collect())
}

fn push_candidate(
    grouped: &mut HashMap<(String, String), Candidate>,
    kind: CandidateKind,
    text: String,
    source: Source,
) {
    let norm = crate::weigh::normalize(&text);
    let map_key = (kind.label().to_string(), norm);
    grouped
        .entry(map_key)
        .and_modify(|c| {
            // Same run+timestamp twice (replay artifacts): don't double
            // count one observation.
            if !c
                .sources
                .iter()
                .any(|s| s.run_id == source.run_id && s.ts_ms == source.ts_ms)
            {
                c.sources.push(source.clone());
            }
        })
        .or_insert(Candidate {
            text,
            kind,
            sources: vec![source],
        });
}

#[cfg(test)]
mod invariant_tests {
    use super::*;

    #[test]
    fn parses_memory_write_detail() {
        let (layer, ns, key) = parse_memory_write(
            "memory_write layer=TaskSession ns=nyx key=lesson:foo backend=native",
        )
        .unwrap();
        assert_eq!(layer, "TaskSession");
        assert_eq!(ns, "nyx");
        assert_eq!(key, "lesson:foo");
    }

    #[test]
    fn rejects_non_memory_write_details() {
        assert!(parse_memory_write("memory_recall query=\"x\" hits=2").is_none());
        assert!(parse_memory_write("memory_write layer=Agent ns=nyx").is_none());
    }

    #[test]
    fn parses_debug_layer_names() {
        assert_eq!(parse_layer("TaskSession"), Some(LayerKind::TaskSession));
        assert_eq!(parse_layer("Agent"), Some(LayerKind::Agent));
        assert_eq!(parse_layer("bogus"), None);
    }

    #[test]
    fn key_query_splits_on_non_alphanumeric() {
        assert_eq!(key_query("lesson:foo"), "lesson foo");
        assert_eq!(
            key_query("consolidated:abcdef1234"),
            "consolidated abcdef1234"
        );
        assert_eq!(key_query("plain"), "plain");
    }
}
