//! Signal → proposal. Deterministic templates: the same signals always
//! yield the same proposals, so the nightly pass is reproducible and
//! auditable. Every proposal carries its provenance (the runs/turns it
//! learned from).
//!
//! Memory promotion uses a plain frequency + recency rule - no decay
//! curves. A candidate promotes when it was observed in at least
//! `min_sessions` distinct runs and its newest observation is within
//! `max_age_days`. The old exponential half-life formula
//! (`0.5^(age/half_life)`) is gone: it was plausible-looking math with
//! no empirical basis, and "seen N times recently" says the same thing
//! honestly.

use crate::signals::{normalize, CandidateKind, MemoryCandidate, Signal, Source};
pub use pantheon_api::nightly::{Proposal, ProposalKind, ProposalStatus, TurnRef};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn proposal_id(kind: &str, title: &str, body: &str) -> String {
    format!(
        "nly_{kind}_{:08x}",
        fnv1a(&format!("{title}\n{body}")) & 0xffff_ffff
    )
}

/// Deterministic idempotency key for a memory candidate's normalized text.
pub fn candidate_key(normalized: &str) -> String {
    let mut h = Sha256::new();
    h.update(normalized.as_bytes());
    let digest = h.finalize();
    format!(
        "nightly:{}",
        digest
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if c == ' ' || c == '_' || c == '-' {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "skill".to_string()
    } else {
        out
    }
}

fn provenance_of(turns: &[TurnRef]) -> (Vec<String>, Vec<TurnRef>) {
    let mut runs: Vec<String> = turns.iter().map(|t| t.run_id.clone()).collect();
    runs.sort();
    runs.dedup();
    (runs, turns.to_vec())
}

/// True when `turns` meet the memory promotion rule: observed in at
/// least `min_sessions` distinct runs, newest observation within
/// `max_age_days`. Frequency + recency, no decay curves.
fn recurrence_met(turns: &[TurnRef], min_sessions: usize, max_age_days: i64, now_ms: i64) -> bool {
    let mut runs: Vec<&str> = turns.iter().map(|t| t.run_id.as_str()).collect();
    runs.sort_unstable();
    runs.dedup();
    if runs.len() < min_sessions {
        return false;
    }
    let newest = turns.iter().map(|t| t.ts_ms).max().unwrap_or(0);
    (now_ms - newest).max(0) as f64 / 86_400_000.0 <= max_age_days as f64
}

/// Turn behavior signals into proposals, strongest first. Pure and
/// deterministic.
///
/// Memory lessons only come from signals that meet the recurrence rule
/// (`min_sessions` distinct runs, newest within `max_age_days`): a
/// one-off correction or denial is noise, not a lesson. (Recurring user
/// corrections also flow through the staged-candidate path in
/// `weigh_candidates`; this function deliberately does NOT emit a
/// lesson per correction, so the same steering can't mint two memory
/// keys.)
///
/// Design note: memory uses recurrence, not replay, as its gate. Replay
/// would need a headless agent that can load a candidate lesson into
/// context and re-run held-out tasks; Pantheon ships no such runner, so
/// recurrence across real sessions is the implemented memory gate
/// ("replay where meaningful; otherwise recurrence"). Skill and Persona
/// proposals are NOT recurrence-gated here - their guard is the replay
/// gate's strict-improvement requirement.
pub fn from_signals(
    signals: &[Signal],
    min_sessions: usize,
    max_age_days: i64,
    now_ms: i64,
) -> Vec<Proposal> {
    // Group one-per-event signals so the recurrence rule can see
    // repeats of the same scope across runs.
    let mut denials: HashMap<String, Vec<TurnRef>> = HashMap::new();
    for signal in signals {
        match signal {
            Signal::UserCorrection { .. } => {
                // No direct proposal: recurring steering is promoted via
                // the staged-candidate path (`weigh_candidates`), which
                // enforces the same recurrence rule. Emitting one here
                // too would mint a second memory key for the same text.
            }
            Signal::ApprovalDenied { scope, at } => {
                denials.entry(scope.clone()).or_default().push(at.clone());
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    // First-seen index per denial scope, so each scope emits one lesson.
    let mut denial_first: HashMap<&str, usize> = HashMap::new();
    for (i, signal) in signals.iter().enumerate() {
        if let Signal::ApprovalDenied { scope, .. } = signal {
            denial_first.entry(scope.as_str()).or_insert(i);
        }
    }
    for (i, signal) in signals.iter().enumerate() {
        match signal {
            Signal::RepeatedSequence { tools, hits } => {
                let (runs, turns) = provenance_of(hits);
                let name = slug(&format!("auto-{}", tools.join("-")));
                let title = format!("skill: {} ({}×)", tools.join(" → "), hits.len());
                let mut body = format!(
                    "# {}\n\nLearned from {} successful runs. Use this sequence when the task matches the pattern below.\n\n## Steps\n",
                    title,
                    hits.len()
                );
                for (i, t) in tools.iter().enumerate() {
                    body.push_str(&format!("{}. Call `{t}`\n", i + 1));
                }
                body.push_str(
                    "\n## Notes\n- This skill was proposed automatically from repeated successful tool sequences.\n- Refine the trigger conditions after first use.\n",
                );
                out.push(Proposal {
                    id: proposal_id("skill", &title, &body),
                    kind: ProposalKind::Skill {
                        name,
                        update: false,
                    },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: turns,
                    // Skills touch tool execution: gate on the skill and
                    // tool eval targets.
                    eval_tags: vec!["tools_skills".into(), "exec_skills".into()],
                    status: ProposalStatus::Proposed,
                });
            }
            Signal::UserCorrection { .. } => {
                // Handled by the staged-candidate path; see the grouping
                // loop above. The signal is still audited (SignalObserved).
            }
            Signal::ApprovalDenied { scope, .. } => {
                // One lesson per scope, only on recurrence: emit for
                // the first-seen signal of each scope.
                if denial_first.get(scope.as_str()) != Some(&i) {
                    continue;
                }
                let hits = &denials[scope];
                if !recurrence_met(hits, min_sessions, max_age_days, now_ms) {
                    continue;
                }
                let (runs, turns) = provenance_of(hits);
                let title = format!("lesson: approval denied for `{scope}`");
                let body = format!(
                    "The user denied the approval scope `{scope}`. Do not repeat the same request shape: \
                     prefer a narrower scope, explain why the operation is needed first, or find an approach \
                     that stays inside already-granted permissions."
                );
                let key = format!("nightly-denial-{:08x}", fnv1a(scope) & 0xffff_ffff);
                out.push(Proposal {
                    id: proposal_id("lesson", &title, &body),
                    kind: ProposalKind::MemoryLesson { key },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: turns,
                    eval_tags: Vec::new(),
                    status: ProposalStatus::Proposed,
                });
            }
            Signal::RepeatedFailure { tool, count, at } => {
                // The scan already required `min_failure_repeats`
                // turns; a lesson additionally needs the failures to
                // recur across distinct runs within the window.
                if !recurrence_met(at, min_sessions, max_age_days, now_ms) {
                    continue;
                }
                let (runs, turns) = provenance_of(at);
                let title = format!("lesson: `{tool}` failed {count}× - check preconditions");
                let body = format!(
                    "The tool `{tool}` failed {count} times across recent turns. Before calling it, \
                     verify its preconditions (inputs exist, are well-formed, and the environment is ready) \
                     instead of retrying blindly."
                );
                let key = format!("nightly-prefail-{:08x}", fnv1a(tool) & 0xffff_ffff);
                out.push(Proposal {
                    id: proposal_id("lesson", &title, &body),
                    kind: ProposalKind::MemoryLesson { key },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: turns,
                    eval_tags: Vec::new(),
                    status: ProposalStatus::Proposed,
                });
            }
            Signal::RepeatedPreference {
                topic,
                keyword,
                hits,
            } => {
                let (runs, turns) = provenance_of(hits);
                let title = format!("persona: prefers {topic} ({}×)", hits.len());
                let body = format!(
                    "Standing user preference: the user steered {} turns across {} runs toward \
                     \"{keyword}\". Default to {topic} unless the task clearly needs otherwise. \
                     Proposed automatically; it takes effect only after human approval.",
                    hits.len(),
                    runs.len()
                );
                out.push(Proposal {
                    id: proposal_id("persona", &title, &body),
                    kind: ProposalKind::Persona {
                        topic: topic.to_string(),
                    },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: turns,
                    // No persona-specific eval targets exist yet; the
                    // approval hold is the gate. Tag them here when they
                    // land in /eval.
                    eval_tags: Vec::new(),
                    status: ProposalStatus::Proposed,
                });
            }
        }
    }
    out
}

/// A memory candidate after the promotion decision.
#[derive(Debug, Clone)]
pub struct WeighedCandidate {
    /// Display text (distilled when the LLM step ran, else raw).
    pub text: String,
    pub kind: CandidateKind,
    pub distinct_runs: usize,
    pub sources: Vec<Source>,
    pub promote: bool,
    /// Deterministic idempotency key: `nightly:<sha256-16>`.
    pub key: String,
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
    if let Some(i) = out.rfind(' ') {
        out.truncate(i);
    }
    out
}

/// Decide promotion for memory candidates and distill their text.
///
/// The rule is deliberately plain: promote when the candidate was seen
/// in at least `min_sessions` distinct runs and its newest observation
/// is within `max_age_days`. Frequency without recency doesn't promote;
/// recency without frequency doesn't either. No decay curves.
///
/// `distill` merges near-duplicate texts when the operator enabled LLM
/// steps; otherwise grouping is by exact normalized text and the raw
/// text is kept. Distill output is validated: non-empty, single line,
/// bounded - the model phrases, it never invents, because promotion
/// still requires the group's staged ledger sources.
pub fn weigh_candidates(
    candidates: Vec<MemoryCandidate>,
    min_sessions: usize,
    max_age_days: i64,
    now: i64,
    distill: Option<&dyn crate::llm::DistillBackend>,
) -> Vec<WeighedCandidate> {
    // Group by (kind, normalized text).
    let mut groups: HashMap<(String, String), Vec<MemoryCandidate>> = HashMap::new();
    for c in candidates {
        let key = (c.kind.label().to_string(), normalize(&c.text));
        groups.entry(key).or_default().push(c);
    }

    let mut weighed = Vec::new();
    for ((_kind_label, norm), members) in groups {
        let kind = members[0].kind.clone();
        let mut sources = Vec::new();
        for m in &members {
            sources.extend(m.sources.iter().cloned());
        }
        sources.sort_by(|a, b| (&a.run_id, a.ts_ms).cmp(&(&b.run_id, b.ts_ms)));
        sources.dedup_by(|a, b| a.run_id == b.run_id && a.ts_ms == b.ts_ms);

        let texts: Vec<String> = if let Some(d) = distill {
            let raws: Vec<String> = members.iter().map(|m| m.text.clone()).collect();
            match d.distill(&raws) {
                Ok(facts) => facts
                    .into_iter()
                    .map(|f| bound_fact(&f))
                    .filter(|f| !f.is_empty() && !f.contains('\n'))
                    .collect(),
                // A failed distill degrades to deterministic.
                Err(_) => raws,
            }
        } else {
            members
                .iter()
                .map(|m| bound_fact(&m.text))
                .collect::<Vec<_>>()
        };
        let mut seen = HashSet::new();
        let mut texts: Vec<String> = texts
            .into_iter()
            .filter(|t| seen.insert(normalize(t)))
            .collect();
        if texts.is_empty() {
            continue;
        }
        let display = texts.remove(0);

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
        let promote = distinct_runs >= min_sessions && newest_age_days <= max_age_days as f64;

        weighed.push(WeighedCandidate {
            text: display,
            kind,
            distinct_runs,
            sources,
            promote,
            key: candidate_key(&norm),
        });
    }

    // Deterministic order: most runs first, then key.
    weighed.sort_by(|a, b| {
        b.distinct_runs
            .cmp(&a.distinct_runs)
            .then_with(|| a.key.cmp(&b.key))
    });
    weighed
}

/// Turn promoted memory candidates into auto-applying MemoryLesson
/// proposals, so memory promotion flows through the same
/// propose → audit → apply pipeline as everything else.
pub fn memory_proposals(weighed: &[WeighedCandidate]) -> Vec<Proposal> {
    weighed
        .iter()
        .filter(|w| w.promote)
        .map(|w| {
            let runs: Vec<String> = {
                let mut r: Vec<&str> = w.sources.iter().map(|s| s.run_id.as_str()).collect();
                r.sort_unstable();
                r.dedup();
                r.into_iter().map(|s| s.to_string()).collect()
            };
            let title = format!("memory: {}", w.text.chars().take(80).collect::<String>());
            let body = format!(
                "Durable fact ({}): {}. Observed across {} runs.",
                w.kind.label(),
                w.text,
                w.distinct_runs
            );
            Proposal {
                id: proposal_id("lesson", &title, &body),
                kind: ProposalKind::MemoryLesson { key: w.key.clone() },
                title,
                body,
                provenance_runs: runs,
                provenance_turns: Vec::new(),
                eval_tags: Vec::new(),
                status: ProposalStatus::Proposed,
            }
        })
        .collect()
}

// Small deterministic invariant tests only.
