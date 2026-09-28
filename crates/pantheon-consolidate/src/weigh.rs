//! Phase 2 — Weigh: group, score, and decide promotion.
//!
//! Scoring is deterministic: for each source occurrence,
//! `kind_weight * 0.5^(age_days / half_life_days)`, summed. A candidate
//! promotes when its score clears `min_score` **and** it was observed in
//! at least `min_sessions` distinct runs **and** its newest source is
//! within `max_age_days`. Frequency without recency doesn't promote;
//! recency without frequency doesn't either.
//!
//! The optional distill step (LLM, only when consolidation is enabled)
//! merges near-duplicate candidates into single well-phrased facts.
//! Without it, grouping is by exact normalized text and the raw text is
//! kept. Distill output is validated: a distilled fact must be a
//! non-empty single line under the length cap, or it is dropped — the
//! model never smuggles in invented memories, because promotion still
//! requires the underlying staged sources.

use crate::{Candidate, ConsolidationConfig, Weighed};
use pantheon_api::error::PantheonError;
use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel, ModelPolicy};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Normalize text for grouping: lowercase, collapse whitespace, strip
/// surrounding punctuation. Deterministic and locale-free.
pub fn normalize(text: &str) -> String {
    let collapsed: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    collapsed
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string()
}

/// Deterministic idempotency key for a candidate's normalized text.
pub fn candidate_key(normalized: &str) -> String {
    let mut h = Sha256::new();
    h.update(normalized.as_bytes());
    let digest = h.finalize();
    format!(
        "consolidated:{}",
        digest
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// An LLM-backed distiller. Implementations take staged candidate texts
/// and return durable facts, one per line. The default (and the only one
/// used when consolidation is disabled) is [`DeterministicDistill`].
pub trait DistillBackend: Send + Sync {
    fn distill(&self, texts: &[String]) -> Result<Vec<String>, String>;
    fn name(&self) -> &'static str {
        "distill"
    }
}

/// No-model distiller: identity. Used whenever consolidation is
/// disabled or no provider is configured — zero LLM calls.
pub struct DeterministicDistill;

impl DistillBackend for DeterministicDistill {
    fn distill(&self, texts: &[String]) -> Result<Vec<String>, String> {
        Ok(texts.to_vec())
    }
    fn name(&self) -> &'static str {
        "deterministic"
    }
}

/// LLM distiller with a routing contract, mirroring Reflection's
/// `LlmRefiner`: implementations MUST place the call on the provided
/// [`AuxiliaryModel`] (the resolved `AuxiliaryKind::Consolidation`
/// slot) — never on the chat/default model directly. That keeps nightly
/// consolidation cheap and out of the interactive model's context.
pub trait LlmDistill: Send + Sync {
    /// Merge staged candidate texts into durable facts using `model`.
    /// Must not invent new claims — merge and phrase only; promotion
    /// still requires the group's staged ledger sources.
    fn distill(&self, model: &AuxiliaryModel, texts: &[String]) -> Result<Vec<String>, String>;
}

/// A [`DistillBackend`] bound to one resolved auxiliary model: the
/// adapter `run_pass` hands to the weigh phase. Construction is the only
/// place the `AuxiliaryKind::Consolidation` slot is resolved, so every
/// LLM call in the pipeline provably routes through it.
pub struct ResolvedDistill<'a> {
    llm: &'a dyn LlmDistill,
    aux: &'a AuxiliaryModel,
}

impl DistillBackend for ResolvedDistill<'_> {
    fn distill(&self, texts: &[String]) -> Result<Vec<String>, String> {
        self.llm.distill(self.aux, texts)
    }
    fn name(&self) -> &'static str {
        "auxiliary"
    }
}

/// Resolve the consolidation distiller for one pass: returns `Some`
/// only when the policy carries an `AuxiliaryKind::Consolidation` entry.
/// The caller additionally gates on `config.enabled` — a misconfigured
/// caller can't spend tokens by accident, and consolidation can never
/// borrow the chat model directly because the only `DistillBackend` an
/// LLM distiller can reach is this adapter.
pub fn resolve_distill<'a>(
    llm: &'a dyn LlmDistill,
    policy: &'a ModelPolicy,
) -> Option<ResolvedDistill<'a>> {
    policy
        .auxiliary(&AuxiliaryKind::Consolidation)
        .map(|aux| ResolvedDistill { llm, aux })
}

/// Max length of one promoted fact. Longer texts are truncated at a word
/// boundary; the memory layer budget is the real ceiling.
pub const MAX_FACT_CHARS: usize = 500;

fn bound_fact(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= MAX_FACT_CHARS {
        return t.to_string();
    }
    let mut out: String = t.chars().take(MAX_FACT_CHARS).collect();
    // Truncate at the last word boundary rather than mid-word.
    if let Some(i) = out.rfind(' ') {
        out.truncate(i);
    }
    out
}

/// Group candidates by normalized text, score them, and mark promotion.
///
/// `distill` is `Some` only when the operator enabled consolidation *and*
/// a provider backend was constructed; otherwise grouping is exact.
pub fn weigh_candidates(
    candidates: Vec<Candidate>,
    config: &ConsolidationConfig,
    now: i64,
    distill: Option<&dyn DistillBackend>,
) -> Result<Vec<Weighed>, PantheonError> {
    // Group by (kind, normalized text).
    let mut groups: HashMap<(String, String), Vec<Candidate>> = HashMap::new();
    for c in candidates {
        let key = (c.kind.label().to_string(), normalize(&c.text));
        groups.entry(key).or_default().push(c);
    }

    // Optional LLM merge: distill each group's texts into facts. The
    // group's sources are the union of member sources, so provenance
    // survives distillation.
    let mut weighed = Vec::new();
    for ((_kind_label, norm), members) in groups {
        let kind = members[0].kind.clone();
        let mut sources = Vec::new();
        for m in &members {
            sources.extend(m.sources.iter().cloned());
        }
        // De-dupe identical (run, ts) observations.
        sources.sort_by(|a, b| (&a.run_id, a.ts_ms).cmp(&(&b.run_id, b.ts_ms)));
        sources.dedup_by(|a, b| a.run_id == b.run_id && a.ts_ms == b.ts_ms);

        let texts: Vec<String> = if let Some(d) = distill {
            let raws: Vec<String> = members.iter().map(|m| m.text.clone()).collect();
            match d.distill(&raws) {
                // Validate the model's output: non-empty, single line,
                // bounded. Anything else is dropped — the model phrases,
                // it never invents, because promotion still requires the
                // group's staged ledger sources.
                Ok(facts) => facts
                    .into_iter()
                    .map(|f| bound_fact(&f))
                    .filter(|f| !f.is_empty() && !f.contains('\n'))
                    .collect(),
                // A failed distill degrades to deterministic: the raw
                // texts survive, the pass doesn't.
                Err(_) => raws,
            }
        } else {
            members
                .iter()
                .map(|m| bound_fact(&m.text))
                .collect::<Vec<_>>()
        };
        // De-dupe identical display texts within the group.
        let mut seen = std::collections::HashSet::new();
        let mut texts: Vec<String> = texts
            .into_iter()
            .filter(|t| seen.insert(normalize(t)))
            .collect();
        if texts.is_empty() {
            continue;
        }
        let display = texts.remove(0);

        let score = score_sources(&sources, &kind, config, now);
        let distinct_runs: usize = {
            let mut runs: Vec<&str> = sources.iter().map(|s| s.run_id.as_str()).collect();
            runs.sort_unstable();
            runs.dedup();
            runs.len()
        };
        let newest_age_days = sources
            .iter()
            .map(|s| (now - s.ts_ms).max(0) as f64 / 86_400_000.0)
            .fold(f64::INFINITY, f64::min);
        let promote = score >= config.min_score
            && distinct_runs >= config.min_sessions
            && newest_age_days <= config.max_age_days as f64;

        weighed.push(Weighed {
            text: display,
            kind,
            score,
            distinct_runs,
            sources,
            promote,
            key: candidate_key(&norm),
        });
    }

    // Deterministic order: highest score first, then key.
    weighed.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(weighed)
}

fn score_sources(
    sources: &[crate::Source],
    kind: &crate::CandidateKind,
    config: &ConsolidationConfig,
    now: i64,
) -> f64 {
    let half_life = config.half_life_days.max(0.1);
    sources
        .iter()
        .map(|s| {
            let age_days = (now - s.ts_ms).max(0) as f64 / 86_400_000.0;
            kind.weight() * 0.5f64.powf(age_days / half_life)
        })
        .sum()
}

#[cfg(test)]
mod invariant_tests {
    use super::*;
    use crate::{CandidateKind, Source};

    fn src(run: &str, age_days: f64, now: i64) -> Source {
        Source {
            run_id: run.to_string(),
            ts_ms: now - (age_days * 86_400_000.0) as i64,
        }
    }

    #[test]
    fn normalize_collapses_case_and_whitespace() {
        assert_eq!(normalize("  Hello,   WORLD! "), "hello, world");
        assert_eq!(normalize("a\nb"), "a b");
    }

    #[test]
    fn candidate_key_is_stable_and_prefixed() {
        let a = candidate_key("hello world");
        let b = candidate_key("hello world");
        let c = candidate_key("hello other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("consolidated:"));
    }

    #[test]
    fn fresh_triple_repetition_promotes() {
        let now = 1_700_000_000_000i64;
        let cfg = ConsolidationConfig::default();
        let cands = vec![Candidate {
            text: "Prefer tabs".into(),
            kind: CandidateKind::Steering,
            sources: vec![
                src("r1", 1.0, now),
                src("r2", 2.0, now),
                src("r3", 3.0, now),
            ],
        }];
        let w = weigh_candidates(cands, &cfg, now, None).unwrap();
        assert_eq!(w.len(), 1);
        assert!(
            w[0].promote,
            "score={} runs={}",
            w[0].score, w[0].distinct_runs
        );
        assert_eq!(w[0].distinct_runs, 3);
    }

    #[test]
    fn one_off_remark_stays_staged() {
        let now = 1_700_000_000_000i64;
        let cfg = ConsolidationConfig::default();
        let cands = vec![Candidate {
            text: "Use tabs".into(),
            kind: CandidateKind::Steering,
            sources: vec![src("r1", 1.0, now)],
        }];
        let w = weigh_candidates(cands, &cfg, now, None).unwrap();
        assert!(!w[0].promote);
    }

    #[test]
    fn stale_repetition_does_not_promote() {
        let now = 1_700_000_000_000i64;
        let cfg = ConsolidationConfig::default();
        // Three repetitions, all 60 days old: decay kills the score and
        // the newest source exceeds max_age_days.
        let cands = vec![Candidate {
            text: "Use tabs".into(),
            kind: CandidateKind::Steering,
            sources: vec![
                src("r1", 60.0, now),
                src("r2", 61.0, now),
                src("r3", 62.0, now),
            ],
        }];
        let w = weigh_candidates(cands, &cfg, now, None).unwrap();
        assert!(!w[0].promote, "score={}", w[0].score);
    }

    #[test]
    fn deterministic_distill_is_identity() {
        let d = DeterministicDistill;
        let texts = vec!["a".to_string(), "b".to_string()];
        assert_eq!(d.distill(&texts).unwrap(), texts);
    }
}
