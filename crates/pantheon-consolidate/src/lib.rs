//! Pantheon Consolidation: background memory consolidation.
//!
//! While the operator sleeps, short-term session material is promoted into
//! durable long-term memory. Three phases, run in order:
//!
//! 1. **Stage** — gather candidate memories from recent ledger events
//!    (steering corrections, session-scoped memory writes, approval
//!    denials). Every candidate carries provenance: the runs it came from.
//! 2. **Weigh** — score each candidate (frequency across sessions, recency
//!    with half-life decay). Below-threshold candidates stay staged.
//! 3. **Promote** — write durable facts into long-term memory *with* their
//!    provenance. Only this phase writes. Promotion is append-only and
//!    idempotent: re-running never duplicates a fact.
//!
//! Ledger-native, not transcript re-reads: every promoted fact traces to
//! ledger events; consolidation never invents memories. This is the
//! memory-only half of the self-improvement story — behavior change
//! (skills, persona) belongs to Reflection, not here.
//!
//! LLM use is gated: the optional distill step (merging near-duplicate
//! candidates, phrasing facts) runs only when `[consolidation] enabled`
//! is true, and every LLM call resolves through
//! `AuxiliaryKind::Consolidation` — the auxiliary consolidation-pass
//! model, never the main chat model. Disabled = fully deterministic,
//! zero model calls.

pub mod promote;
pub mod report;
pub mod stage;
pub mod state;
pub mod weigh;

use pantheon_api::error::{Layer, PantheonError};

fn cerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Memory,
        false,
        cause,
        "check the [consolidation] config section and the data dir",
        "",
    )
}

/// Configuration for a consolidation pass. Mirrors the `[consolidation]`
/// config section; every threshold is tunable.
#[derive(Debug, Clone)]
pub struct ConsolidationConfig {
    /// Master switch for LLM-backed steps (distill). Default false:
    /// consolidation spends the operator's tokens, so it must be
    /// explicitly opted into. Stage/weigh/promote are deterministic and
    /// run regardless.
    pub enabled: bool,
    /// Recency half-life for the decay curve, in days. Default 14.
    pub half_life_days: f64,
    /// Distinct sessions a candidate must appear in before promotion.
    /// Default 3: a one-off remark stays staged.
    pub min_sessions: usize,
    /// Minimum decayed score for promotion. Default 2.0.
    pub min_score: f64,
    /// Sources older than this are ignored entirely, in days. Default 30.
    pub max_age_days: i64,
    /// How far back the stage phase scans, in days. Default 30.
    pub lookback_days: i64,
    /// Bound on runs scanned per pass. Default 10_000.
    pub max_runs_scanned: usize,
    /// Memory namespace promoted facts land in (Agent layer).
    pub namespace: String,
    /// Default cron for `pantheon consolidate --schedule`. Not a firing
    /// schedule by itself — the pantheon scheduler owns firing.
    pub cron: String,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            half_life_days: 14.0,
            min_sessions: 3,
            min_score: 2.0,
            max_age_days: 30,
            lookback_days: 30,
            max_runs_scanned: 10_000,
            namespace: "nyx".to_string(),
            cron: "0 3 * * *".to_string(),
        }
    }
}

/// Where a candidate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateKind {
    /// Mid-turn steering: the operator corrected the agent. A direct
    /// statement of preference — the strongest signal.
    Steering,
    /// A memory the agent wrote at session scope. Worth keeping
    /// long-term only if it recurs across sessions.
    SessionMemory,
    /// The operator denied an approval scope. A negative preference:
    /// "don't do this unasked".
    Denial,
}

impl CandidateKind {
    /// Base weight per occurrence, before recency decay.
    pub fn weight(&self) -> f64 {
        match self {
            CandidateKind::Steering => 1.0,
            CandidateKind::SessionMemory => 0.6,
            CandidateKind::Denial => 0.8,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            CandidateKind::Steering => "steering",
            CandidateKind::SessionMemory => "session-memory",
            CandidateKind::Denial => "denial",
        }
    }
}

/// One provenance link: the run an observation came from, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub run_id: String,
    pub ts_ms: i64,
}

/// A candidate memory with full provenance.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub text: String,
    pub kind: CandidateKind,
    pub sources: Vec<Source>,
}

/// A candidate after weighing: grouped, scored, promotion decision made.
#[derive(Debug, Clone)]
pub struct Weighed {
    /// Display text (distilled when the LLM step ran, else normalized raw).
    pub text: String,
    pub kind: CandidateKind,
    pub score: f64,
    pub distinct_runs: usize,
    pub sources: Vec<Source>,
    pub promote: bool,
    /// Deterministic idempotency key: `consolidated:<sha256-16>`.
    pub key: String,
}

/// Outcome of one full pass.
#[derive(Debug, Clone, Default)]
pub struct ConsolidationReport {
    pub staged: usize,
    pub weighed: usize,
    pub promoted: usize,
    /// Under dry-run, how many candidates *would* promote (promote=true
    /// and not already in the backend). `promoted` stays 0 because
    /// nothing is written.
    pub would_promote: usize,
    pub skipped_duplicate: usize,
    pub dry_run: bool,
    pub llm_used: bool,
    pub promoted_keys: Vec<String>,
    pub started_ms: i64,
    pub finished_ms: i64,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Input bundle for a consolidation pass.
pub struct PassInput<'a> {
    pub ledger: &'a pantheon_storage::Ledger,
    pub backend: &'a dyn pantheon_memory::MemoryBackend,
    pub policy: &'a pantheon_api::capability::Policy,
    pub config: &'a ConsolidationConfig,
    pub data_dir: &'a std::path::Path,
    pub dry_run: bool,
    /// Resolved model policy. The LLM distill step looks up
    /// `AuxiliaryKind::Consolidation` here and calls THAT model — never
    /// the chat model. `None` = LLM steps are refused outright (a
    /// deterministic-only pass); the deterministic pipeline is
    /// unaffected.
    pub model_policy: Option<&'a pantheon_api::model::ModelPolicy>,
    /// Optional LLM distiller. Consulted only when `config.enabled` AND
    /// the policy resolves the Consolidation slot. When disabled, or
    /// when no policy is available to resolve the slot, the distiller is
    /// not even looked at — zero LLM calls, guaranteed.
    pub llm: Option<&'a dyn weigh::LlmDistill>,
    pub now_ms: i64,
}

/// Run all three phases in order and persist the report + state.
///
/// The only phase that writes long-term memory is promote, and it is
/// skipped entirely under `dry_run`.
pub fn run_pass(input: PassInput<'_>) -> Result<ConsolidationReport, PantheonError> {
    let started = input.now_ms;
    let mut report = ConsolidationReport {
        started_ms: started,
        dry_run: input.dry_run,
        ..Default::default()
    };

    // Load state for the incremental scan window; a missing file means
    // "scan the full lookback window".
    let mut st = state::load(input.data_dir).unwrap_or_default();
    let since_ms = st
        .last_run_ms
        .unwrap_or_else(|| started - input.config.lookback_days * 86_400_000);

    // Phase 1: stage.
    let candidates = stage::stage_candidates(
        input.ledger,
        input.backend,
        input.policy,
        input.config,
        since_ms,
    )?;
    report.staged = candidates.len();

    // Phase 2: weigh (+ optional distill). The distill step is resolved
    // through the AuxiliaryKind::Consolidation slot: enabled + a policy
    // that carries the slot = an LLM call on THAT model, never chat.
    // Anything else = fully deterministic, zero model calls.
    let resolved = if input.config.enabled {
        match (input.llm, input.model_policy) {
            (Some(llm), Some(policy)) => weigh::resolve_distill(llm, policy),
            _ => None,
        }
    } else {
        None
    };
    report.llm_used = resolved.is_some();
    let weighed = weigh::weigh_candidates(
        candidates,
        input.config,
        started,
        resolved.as_ref().map(|r| r as &dyn weigh::DistillBackend),
    )?;
    report.weighed = weighed.len();

    // Phase 3: promote (skipped under dry-run).
    if !input.dry_run {
        let outcome =
            promote::promote(input.backend, input.policy, &weighed, input.config, started)?;
        report.promoted = outcome.promoted;
        report.skipped_duplicate = outcome.skipped_duplicate;
        report.promoted_keys = outcome.promoted_keys;
        st.last_run_ms = Some(started);
        st.last_summary = Some(report.summary_line());
        st.last_staged = Some(report.staged);
        st.last_promoted = Some(report.promoted);
        st.promoted_keys
            .extend(report.promoted_keys.iter().cloned());
        state::save(input.data_dir, &st).map_err(|e| cerr("CONSOLIDATE_STATE", e))?;
    } else {
        // Dry-run: count what *would* promote (read-only duplicate
        // check, no writes). State is left untouched.
        let mut would = 0;
        let mut skipped = 0;
        for w in weighed.iter().filter(|w| w.promote) {
            let already = input
                .backend
                .list_agent(&input.config.namespace)
                .map(|rows| rows.iter().any(|(k, _)| k == &w.key))
                .unwrap_or(false);
            if already {
                skipped += 1;
            } else {
                would += 1;
            }
        }
        report.would_promote = would;
        report.skipped_duplicate = skipped;
    }

    report.finished_ms = now_ms();

    // Human-readable per-phase reports, always written (even dry-run):
    // the operator can read what *would* happen.
    if let Err(e) = report::write_reports(input.data_dir, &report, &weighed, started) {
        // Reports are observability, not the pass itself: log and continue.
        eprintln!("consolidate: report write failed: {e}");
    }

    Ok(report)
}

impl ConsolidationReport {
    pub fn summary_line(&self) -> String {
        if self.dry_run {
            format!(
                "consolidation: staged {} → would promote {} [dry-run] ({} skipped as duplicates){}",
                self.staged,
                self.would_promote,
                self.skipped_duplicate,
                if self.llm_used { " [llm distill]" } else { "" },
            )
        } else {
            format!(
                "consolidation: staged {} → promoted {} ({} skipped as duplicates){}",
                self.staged,
                self.promoted,
                self.skipped_duplicate,
                if self.llm_used { " [llm distill]" } else { "" },
            )
        }
    }
}

#[cfg(test)]
mod invariant_tests {
    use super::*;

    #[test]
    fn disabled_by_default() {
        assert!(!ConsolidationConfig::default().enabled);
    }

    #[test]
    fn kind_weights_ordered_by_signal_strength() {
        assert!(CandidateKind::Steering.weight() > CandidateKind::Denial.weight());
        assert!(CandidateKind::Denial.weight() > CandidateKind::SessionMemory.weight());
    }

    #[test]
    fn summary_line_marks_dry_run() {
        let r = ConsolidationReport {
            dry_run: true,
            staged: 5,
            promoted: 0,
            ..Default::default()
        };
        assert!(r.summary_line().contains("[dry-run]"));
    }
}
