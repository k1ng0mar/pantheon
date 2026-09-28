//! Phase 3 — Promote: write durable facts into long-term memory.
//!
//! Only this phase writes. Every promoted fact lands in the Agent layer
//! with trust tier `Memory` (recalled context: informative, never
//! authoritative) and provenance naming consolidation plus the source
//! run ids it was learned from.
//!
//! Idempotency is by deterministic key: `consolidated:<sha256-16>` of the
//! normalized text. A re-run finds the existing record and skips it, so
//! promotion is append-only and never duplicates.

use crate::{ConsolidationConfig, Weighed};
use pantheon_api::error::PantheonError;
use pantheon_memory::{LayerKind, MemoryBackend, Proposal, Provenance};

/// Outcome of the promote phase.
#[derive(Debug, Clone, Default)]
pub struct PromoteOutcome {
    pub promoted: usize,
    pub skipped_duplicate: usize,
    pub promoted_keys: Vec<String>,
}

/// Write every `promote`-flagged candidate into long-term memory.
///
/// Skips (counting as duplicate) any key already present in the store —
/// this is what makes re-runs idempotent. Records that fail validation
/// (secret tripwire, over budget) are skipped, not fatal: one bad
/// candidate must not fail the pass.
pub fn promote(
    backend: &dyn MemoryBackend,
    policy: &pantheon_api::capability::Policy,
    weighed: &[Weighed],
    config: &ConsolidationConfig,
    now_ms: i64,
) -> Result<PromoteOutcome, PantheonError> {
    let mut outcome = PromoteOutcome::default();
    for w in weighed.iter().filter(|w| w.promote) {
        // Idempotency: the deterministic key means a previous pass's
        // Idempotency: the deterministic key means a previous pass's
        // promotion is found by exact key comparison and skipped. This
        // uses `list_agent`, not FTS recall: recall is a search API and
        // the `consolidated:<hex>` key does not survive FTS
        // tokenization/sanitization intact on every backend, while
        // `list_agent` returns exact (key, value) pairs for the Agent
        // layer by contract.
        let already = backend
            .list_agent(&config.namespace)
            .map(|rows| rows.iter().any(|(k, _)| k == &w.key))
            .unwrap_or(false);
        if already {
            outcome.skipped_duplicate += 1;
            continue;
        }

        let proposal = Proposal {
            layer: LayerKind::Agent,
            namespace: config.namespace.clone(),
            key: w.key.clone(),
            value: w.text.clone(),
            provenance: Provenance {
                source: "consolidation".to_string(),
                origin: origin_of(&w.sources),
                trust: pantheon_api::provenance::TrustTier::Memory,
                recorded_at_ms: now_ms,
            },
        };
        match pantheon_memory::write_via(backend, policy, proposal, 4096) {
            Ok(_) => {
                outcome.promoted += 1;
                outcome.promoted_keys.push(w.key.clone());
            }
            // Validation refusals (secret detected, too large) and
            // backend errors skip the candidate; the pass continues.
            Err(_) => continue,
        }
    }
    Ok(outcome)
}

/// Human- and machine-readable provenance: the ledger runs this fact was
/// learned from, bounded so the origin field stays small.
fn origin_of(sources: &[crate::Source]) -> String {
    let mut runs: Vec<&str> = sources.iter().map(|s| s.run_id.as_str()).collect();
    runs.sort_unstable();
    runs.dedup();
    const MAX_RUNS: usize = 5;
    let mut out = format!(
        "consolidation:{}",
        runs.iter()
            .take(MAX_RUNS)
            .copied()
            .collect::<Vec<_>>()
            .join(",")
    );
    if runs.len() > MAX_RUNS {
        out.push_str(&format!("+{}more", runs.len() - MAX_RUNS));
    }
    out
}

#[cfg(test)]
mod invariant_tests {
    use super::*;
    use crate::Source;

    #[test]
    fn origin_lists_runs_bounded() {
        let sources: Vec<Source> = (0..8)
            .map(|i| Source {
                run_id: format!("r{i}"),
                ts_ms: 0,
            })
            .collect();
        let o = origin_of(&sources);
        assert!(o.starts_with("consolidation:"));
        assert!(o.contains("+3more"));
    }

    #[test]
    fn origin_dedupes_repeated_runs() {
        let sources = vec![
            Source {
                run_id: "r1".into(),
                ts_ms: 1,
            },
            Source {
                run_id: "r1".into(),
                ts_ms: 2,
            },
        ];
        assert_eq!(origin_of(&sources), "consolidation:r1");
    }
}
