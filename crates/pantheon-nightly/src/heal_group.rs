//! heal_group: apply, fixloop, repair_targets, replay, gate in one file.

pub mod apply {
    //! Apply + pending approval.
    //!
    //! Propose-only: no unattended path writes a durable record.
    //!
    //! - **Memory lessons** are surfaced with their evidence (provenance
    //!   runs + observed-count), then parked in
    //!   `<data_dir>/nightly/nightly-pending.json` for a human to approve or
    //!   deny. A grant writes the record at the configured trust tier
    //!   (`NightlyConfig::memory_trust`, `Memory` by default); a denial
    //!   ends the proposal. Idempotent keys keep re-approval stable.
    //! - **Skill and Persona proposals** pass eval-gating and replay-gating,
    //!   then sit in the same pending queue until a human approves or denies
    //!   them.
    //!
    //! `route` always queues. `apply_memory_lesson` / `apply_approved` are
    //! only reachable through an explicit operator grant (`decide`), never
    //! through the unattended nightly pass.

    use crate::propose::{Proposal, ProposalKind, ProposalStatus};
    use crate::NightlyConfig;
    use pantheon_api::logging::now_ms;
    use pantheon_api::provenance::TrustTier;
    use pantheon_memory::{
        LayerKind, MemoryStore, Proposal as MemProposal, Provenance as MemProvenance,
    };
    use std::path::{Path, PathBuf};

    /// Where pending (awaiting approval) proposals live.
    pub fn pending_path(data_dir: &Path) -> PathBuf {
        data_dir.join("nightly").join("nightly-pending.json")
    }

    #[derive(Debug, Clone)]
    pub enum ApplyOutcome {
        /// A durable record was written by the explicit operator path
        /// (`nightly approve <id>`). Emitted only by
        /// [`apply_memory_lesson`]; the unattended [`route`] never does this.
        AutoApplied,
        /// Proposal queued for human approval. This is the only outcome the
        /// unattended nightly pass produces - propose-only, no auto-during.
        AwaitingApproval,
    }

    /// Validation a proposal earned before it entered the approval queue.
    ///
    /// Bound to the exact draft body and the exact required eval set, so a
    /// queue entry that was edited after validation cannot be approved on the
    /// strength of the old validation. The nightly pass is the only writer;
    /// a mismatch means the stored proposal is no longer the one that passed
    /// the gates.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct ValidationBinding {
        /// SHA-256 (lowercase hex) of the validated draft body.
        pub body_sha256: String,
        /// The exact eval targets that passed, in gate order.
        pub eval_tags: Vec<String>,
    }

    impl ValidationBinding {
        /// Bind a proposal that has just passed eval + replay gating.
        pub fn of(proposal: &Proposal) -> Self {
            Self {
                body_sha256: body_sha256(&proposal.body),
                eval_tags: proposal.eval_tags.clone(),
            }
        }

        /// True when `proposal` is the draft and eval set this binding was
        /// computed from.
        pub fn matches(&self, proposal: &Proposal) -> bool {
            !self.body_sha256.is_empty()
                && self.body_sha256 == body_sha256(&proposal.body)
                && self.eval_tags == proposal.eval_tags
        }
    }

    fn body_sha256(body: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// One pending entry: the proposal plus the validation it earned.
    ///
    /// The binding is flattened into the proposal object so the on-disk shape
    /// stays "an array of proposals" with one extra optional key; entries
    /// written before bindings existed (and memory lessons, which never pass a
    /// gate) simply carry no `validation`.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct QueuedProposal {
        #[serde(flatten)]
        proposal: Proposal,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validation: Option<ValidationBinding>,
    }

    /// Load the pending queue (empty when the file is absent).
    ///
    /// A torn file (unparseable JSON) also loads as empty, with a loud
    /// warning: the queue is written atomically (see [`save_queue`]), so a
    /// torn file can only predate atomic writes or come from a crash
    /// mid-rename - and a queue that errors forever blocks every later
    /// decision. The loss is reported, not silent.
    fn load_queue(data_dir: &Path) -> Result<Vec<QueuedProposal>, String> {
        match std::fs::read_to_string(pending_path(data_dir)) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(queue) => Ok(queue),
                Err(e) => {
                    eprintln!(
                        "nightly: pending queue {} is torn ({e}); treating as empty",
                        pending_path(data_dir).display()
                    );
                    Ok(Vec::new())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("read pending proposals: {e}")),
        }
    }

    fn save_queue(data_dir: &Path, queue: &[QueuedProposal]) -> Result<(), String> {
        let path = pending_path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(queue).map_err(|e| format!("encode pending: {e}"))?;
        atomic_write(&path, &text).map_err(|e| format!("write {}: {e}", path.display()))
    }

    /// Load the pending queue as plain proposals (display path).
    ///
    /// Callers that act on an entry must use [`decide`], which re-checks the
    /// validation binding before applying.
    pub fn load_pending(data_dir: &Path) -> Result<Vec<Proposal>, String> {
        Ok(load_queue(data_dir)?
            .into_iter()
            .map(|entry| entry.proposal)
            .collect())
    }

    /// Queue a proposal for human approval. No-op (returns
    /// false) when a proposal with the same id is already queued - the
    /// operator sees one decision per proposal.
    ///
    /// A skill/persona proposal must arrive with the [`ValidationBinding`] it
    /// earned in the fix loop: queueing without one, or with a binding that no
    /// longer matches the draft or its required eval set, is refused. That is
    /// what stops an unvalidated (or post-validation-edited) proposal from
    /// sitting in the queue looking green. Memory lessons never pass a gate, so
    /// they queue without a binding.
    pub fn queue_for_approval(
        data_dir: &Path,
        proposal: &Proposal,
        validation: Option<&ValidationBinding>,
    ) -> Result<bool, String> {
        let mut queue = load_queue(data_dir)?;
        if queue.iter().any(|entry| entry.proposal.id == proposal.id) {
            return Ok(false);
        }
        let mut p = proposal.clone();
        let validation = if matches!(p.kind, ProposalKind::MemoryLesson { .. }) {
            // Memory lessons are gated by their promotion rule, not eval+replay.
            None
        } else {
            let binding = validation.ok_or_else(|| {
                format!(
                    "proposal '{}' has no validation binding; a skill/persona proposal must pass its gates before it is queued",
                    p.id
                )
            })?;
            if binding.eval_tags.is_empty() {
                return Err(format!(
                    "proposal '{}' carries an empty validated eval set; refusing to queue an untested proposal",
                    p.id
                ));
            }
            if !binding.matches(&p) {
                return Err(format!(
                    "proposal '{}' changed after validation; refusing to queue it - re-run the nightly pass",
                    p.id
                ));
            }
            // Only skill/persona proposals pass a replay gate before
            // queueing, so only they may carry ReplayPassed here. A
            // MemoryLesson keeps its incoming status (Proposed) in the queue.
            p.status = ProposalStatus::ReplayPassed;
            Some(binding.clone())
        };
        queue.push(QueuedProposal {
            proposal: p,
            validation,
        });
        save_queue(data_dir, &queue)?;
        Ok(true)
    }

    /// Resolve a human decision. Approving applies the skill/persona;
    /// denying ends the proposal. Either way the proposal leaves the
    /// pending queue - the queue holds only undecided proposals. Every
    /// decision is audited (`ApprovalDecided`, plus `Applied` on approval).
    /// Returns whether the id was found.
    pub fn decide(
        data_dir: &Path,
        config: &NightlyConfig,
        id: &str,
        approve: bool,
    ) -> Result<bool, String> {
        let mut queue = load_queue(data_dir)?;
        let pos = queue.iter().position(|entry| entry.proposal.id == id);
        let Some(pos) = pos else { return Ok(false) };
        let at = now_ms();
        if approve {
            // Re-check the validation binding before applying: the queue file
            // is human-editable, so a stored proposal is only trustworthy when
            // it is still the exact draft + eval set that passed the gates.
            // A mismatch refuses (fail-closed) rather than applying an
            // unvalidated revision.
            let entry = queue[pos].clone();
            let proposal = entry.proposal.clone();
            if !matches!(proposal.kind, ProposalKind::MemoryLesson { .. }) {
                match &entry.validation {
                    Some(binding) if binding.matches(&proposal) => {}
                    Some(_) => {
                        return Err(format!(
                            "proposal '{id}' changed after validation; refusing to apply it - re-run the nightly pass to revalidate"
                        ))
                    }
                    None => {
                        return Err(format!(
                            "proposal '{id}' carries no validation binding; refusing to apply an unvalidated skill/persona"
                        ))
                    }
                }
            }
            // Apply BEFORE dequeuing: if application fails the proposal stays
            // in the queue for retry and the failure is audited.
            if let Err(e) = apply_approved(data_dir, &proposal, config.memory_trust) {
                crate::audit::audit(
                    data_dir,
                    &crate::audit::NightlyEvent::ApplyFailed {
                        id: proposal.id.clone(),
                        reason: e.clone(),
                        at_ms: at,
                    },
                )?;
                return Err(e);
            }
            queue.remove(pos);
            save_queue(data_dir, &queue)?;
            crate::audit::audit(
                data_dir,
                &crate::audit::NightlyEvent::ApprovalDecided {
                    id: proposal.id.clone(),
                    approved: true,
                    at_ms: at,
                },
            )?;
            crate::audit::audit(
                data_dir,
                &crate::audit::NightlyEvent::Applied {
                    id: proposal.id.clone(),
                    kind: proposal.kind_name().to_string(),
                    at_ms: at,
                },
            )?;
        } else {
            queue.remove(pos);
            save_queue(data_dir, &queue)?;
            crate::audit::audit(
                data_dir,
                &crate::audit::NightlyEvent::ApprovalDecided {
                    id: id.to_string(),
                    approved: false,
                    at_ms: at,
                },
            )?;
        }
        Ok(true)
    }

    fn mem_provenance(source: &str, trust: TrustTier) -> MemProvenance {
        MemProvenance {
            source: source.to_string(),
            origin: "nightly-pass".to_string(),
            trust,
            recorded_at_ms: now_ms(),
        }
    }

    fn memory_store(data_dir: &Path) -> Result<MemoryStore, String> {
        MemoryStore::open(&data_dir.join("memory.db"))
            .map_err(|e| format!("open memory store: {e}"))
    }

    /// Apply a validated, human-approved skill/persona proposal.
    /// Memory lessons are NOT applied here - they go through
    /// [`apply_memory_lesson`]. `trust` is the configured tier
    /// ([`NightlyConfig::memory_trust`]): persona notes are memory-store
    /// writes, so they land at the same tier as lessons.
    pub fn apply_skill_or_persona(
        data_dir: &Path,
        proposal: &Proposal,
        trust: TrustTier,
    ) -> Result<(), String> {
        match &proposal.kind {
            ProposalKind::Skill { name, update } => {
                // Defense in depth: the name is slug-shaped by construction
                // (`propose::slug`), but never let a path escape the skills
                // dir even if a proposal is built by hand.
                if !skill_name_ok(name) {
                    return Err(format!("unsafe skill name '{name}'"));
                }
                let dir = data_dir.join("skills").join(name);
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("create {}: {e}", dir.display()))?;
                let path = dir.join("SKILL.md");
                // Provenance header so a reader can trace the skill back to
                // the runs that taught it.
                let mut body = format!(
                    "<!-- proposed by pantheon-nightly as {} from runs: {} -->\n\n{}",
                    proposal.id,
                    proposal.provenance_runs.join(", "),
                    proposal.body
                );
                if *update && path.exists() {
                    // Never silently overwrite a hand-written skill: an
                    // update appends a dated nightly note instead.
                    let mut existing = std::fs::read_to_string(&path)
                        .map_err(|e| format!("read {}: {e}", path.display()))?;
                    existing.push_str(&format!("\n\n## Nightly note\n\n{body}\n"));
                    body = existing;
                }
                atomic_write(&path, &body).map_err(|e| format!("write {}: {e}", path.display()))?;
                Ok(())
            }
            ProposalKind::Persona { topic } => {
                // Persona notes live in the memory store under the
                // persona namespace: they shape future sessions without
                // touching user config files. The runtime injects approved
                // notes into fresh runs (see `crate::persona`); only
                // `decide(approve = true)` writes here, so the trust-tier
                // rule (evals + replay + human yes) holds by construction.
                let store = memory_store(data_dir)?;
                let key = format!("{}{topic}", crate::persona::PERSONA_KEY_PREFIX);
                store
                    .put(&MemProposal {
                        layer: LayerKind::Agent,
                        namespace: crate::persona::PERSONA_NAMESPACE.to_string(),
                        key: key.clone(),
                        value: proposal.body.clone(),
                        provenance: mem_provenance("nightly", trust),
                    })
                    .map_err(|e| format!("store persona note: {e}"))?;
                Ok(())
            }
            ProposalKind::MemoryLesson { .. } => {
                // Operator-approved a memory lesson: write it at the
                // configured trust tier now. This is the explicit user-vouch
                // path - the unattended `route` never reaches here.
                apply_memory_lesson(data_dir, proposal, trust)
            }
        }
    }

    /// Apply a human-approved proposal by kind. The dispatcher [`decide`]
    /// calls this after a grant: skill/persona proposals apply their payload,
    /// memory lessons write the durable record at the configured trust tier
    /// ([`NightlyConfig::memory_trust`]). This is the only path that turns
    /// a queued proposal into a durable effect - `route` always queues,
    /// never applies.
    pub fn apply_approved(
        data_dir: &Path,
        proposal: &Proposal,
        trust: TrustTier,
    ) -> Result<(), String> {
        match &proposal.kind {
            ProposalKind::MemoryLesson { .. } => apply_memory_lesson(data_dir, proposal, trust),
            ProposalKind::Skill { .. } | ProposalKind::Persona { .. } => {
                apply_skill_or_persona(data_dir, proposal, trust)
            }
        }
    }

    /// Apply a memory lesson at the configured trust tier. Idempotent: the
    /// key is derived from the lesson text, so re-applying is a no-op write.
    pub fn apply_memory_lesson(
        data_dir: &Path,
        proposal: &Proposal,
        trust: TrustTier,
    ) -> Result<(), String> {
        let ProposalKind::MemoryLesson { key } = &proposal.kind else {
            return Err("apply_memory_lesson takes only MemoryLesson proposals".into());
        };
        let store = memory_store(data_dir)?;
        store
            .put(&MemProposal {
                layer: LayerKind::Agent,
                namespace: "nightly".to_string(),
                key: key.clone(),
                value: proposal.body.clone(),
                provenance: mem_provenance("nightly", trust),
            })
            .map_err(|e| format!("store lesson: {e}"))?;
        Ok(())
    }

    /// Route a proposal to its destination.
    ///
    /// Memory lessons no longer auto-apply: an unattended nightly pass
    /// surfaces a proposed lesson with its evidence (provenance runs +
    /// observed-count) and parks it in the approval queue, exactly like
    /// skill/persona proposals. The operator grants or denies; only a grant
    /// writes the durable record. This is the trust/permission model -
    /// frequency + recency can *propose* a lesson, they cannot *durable* one.
    ///
    /// This entry point carries no validation binding, so it can only queue
    /// memory lessons: a skill/persona proposal routed here is refused rather
    /// than queued as if it had passed the gates. Gate-validated proposals go
    /// through [`queue_for_approval`] with the binding from the fix loop.
    pub fn route(data_dir: &Path, proposal: &Proposal) -> Result<ApplyOutcome, String> {
        // All proposal kinds queue for human approval. `apply_memory_lesson`
        // is still exposed for the explicit operator path (`nightly approve
        // <id>`), but the unattended route never calls it directly.
        queue_for_approval(data_dir, proposal, None)?;
        Ok(ApplyOutcome::AwaitingApproval)
    }

    /// Skill names are slug-shaped (`[a-z0-9-]`, non-empty, bounded): the
    /// skill can never escape `<data_dir>/skills/`.
    fn skill_name_ok(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    /// Write a file atomically: temp file + rename, so a crash never leaves
    /// a half-written SKILL.md. Shared by the other nightly writers (state,
    /// escalations, report) so every nightly file follows the same pattern.
    pub(crate) fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, contents)?;
        std::fs::rename(&tmp, path)
    }

    // Small deterministic invariant tests only.

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::propose::{ProposalKind, ProposalStatus};
        use crate::NightlyConfig;
        use pantheon_api::provenance::TrustTier;
        use pantheon_memory::{LayerKind, MemoryStore};

        fn lesson(id: &str, key: &str) -> Proposal {
            Proposal {
                id: id.to_string(),
                kind: ProposalKind::MemoryLesson {
                    key: key.to_string(),
                },
                title: "lesson".to_string(),
                body: "prefer short replies".to_string(),
                provenance_runs: Vec::new(),
                provenance_turns: Vec::new(),
                eval_tags: Vec::new(),
                status: ProposalStatus::Proposed,
            }
        }

        fn record_trust(data_dir: &Path, key: &str) -> TrustTier {
            let store = MemoryStore::open(&data_dir.join("memory.db")).expect("memory store");
            let rec = store
                .get(LayerKind::Agent, "nightly", key)
                .expect("memory read")
                .expect("lesson stored");
            rec.provenance.trust
        }

        /// `config.memory_trust` is honoured, not discarded: before the fix,
        /// `decide` dropped the config (`let _ = config`) and every grant
        /// landed at `Memory` regardless of configuration.
        #[test]
        fn decide_honours_configured_memory_trust() {
            let dir = tempfile::tempdir().unwrap();
            let data_dir = dir.path();
            let proposal = lesson("nly_test", "trust-key");
            assert!(queue_for_approval(data_dir, &proposal, None).unwrap());
            let config = NightlyConfig {
                data_dir: data_dir.to_path_buf(),
                memory_trust: TrustTier::User,
                ..NightlyConfig::default()
            };
            assert!(decide(data_dir, &config, "nly_test", true).unwrap());
            assert_eq!(record_trust(data_dir, "trust-key"), TrustTier::User);
        }

        /// The direct apply path takes the tier as a parameter: a `Memory`
        /// grant still lands at `Memory` (the default), so existing
        /// behaviour is unchanged when the config is default.
        #[test]
        fn apply_memory_lesson_at_memory_tier_by_default() {
            let dir = tempfile::tempdir().unwrap();
            apply_memory_lesson(
                dir.path(),
                &lesson("nly_test2", "mem-key"),
                TrustTier::Memory,
            )
            .unwrap();
            assert_eq!(record_trust(dir.path(), "mem-key"), TrustTier::Memory);
        }

        /// A torn pending queue loads as empty (loudly) instead of erroring
        /// every later decision out of existence.
        #[test]
        fn torn_pending_queue_loads_as_empty() {
            let dir = tempfile::tempdir().unwrap();
            let data_dir = dir.path();
            std::fs::create_dir_all(data_dir.join("nightly")).unwrap();
            std::fs::write(
                data_dir.join("nightly").join("nightly-pending.json"),
                "{not json",
            )
            .unwrap();
            assert!(load_pending(data_dir).unwrap().is_empty());
        }
    }
}

pub mod fixloop {
    //! Bounded fix loop: diagnose → revise → re-check, then escalate.
    //!
    //! When a skill/persona proposal fails eval-gating or replay-gating,
    //! the pass doesn't just reject it - it tries to repair the DRAFT
    //! first:
    //!
    //! - **Eval reject**: the eval failure detail goes to the repair aux
    //!   model (Repair slot), which revises the draft body; the FULL eval
    //!   tag set re-runs against the revised draft. Eval tags are immutable
    //!   once the proposal enters the loop - the loop may not narrow the
    //!   claim to dodge failing evals (that let broken drafts validate
    //!   green).
    //! - **Skipped evals** (no tags): escalates unless the proposal kind is
    //!   explicitly allowlisted to skip eval-gating. Currently no kind is
    //!   allowlisted - memory lessons never reach the gate at all (lib.rs
    //!   handles them on a separate path: the promotion rule IS the gate).
    //! - **Replay reject, fair measurement** ("no strict improvement"): one
    //!   flakiness re-run of the A/B pair per attempt. With LLM steps
    //!   enabled, the repair aux model (Repair slot) may instead sharpen
    //!   the draft once, then both gates re-run on the revised draft.
    //! - **Replay reject, infrastructure** (no tasks, spawn/timeout/scoring
    //!   errors): not repairable by revision - escalate immediately.
    //!
    //! After `max_fix_attempts` the proposal is marked `NeedsAttention` and
    //! recorded in `nightly-escalated.json`; it never loops forever and it
    //! never reaches the approval queue unvalidated. Every attempt and
    //! every escalation is audited (`FixAttempt`, `Escalated`).

    use crate::audit::NightlyEvent;
    use crate::gate::{gate, EvalRunner, EvalVerdict};
    use crate::propose::{Proposal, ProposalKind, ProposalStatus};
    use crate::replay::{replay_gate, ReplayRunner, ReplayStore, ReplayVerdict};
    use crate::{NightlyConfig, NightlyLlm};
    use pantheon_api::logging::now_ms;
    use pantheon_api::model::AuxiliaryModel;
    use std::path::{Path, PathBuf};

    /// How many diagnose → revise → re-check iterations a failing proposal
    /// gets before escalation. Default 3.
    pub const DEFAULT_MAX_FIX_ATTEMPTS: usize = 3;

    /// The fix loop's verdict on one proposal.
    #[derive(Debug)]
    pub enum FixOutcome {
        /// Validation eventually passed; the caller may queue for approval.
        Validated,
        /// The loop gave up (or hit unrepairable infrastructure). The
        /// proposal is marked `NeedsAttention` and recorded; the reason
        /// names the cause.
        Escalated { reason: String },
    }

    /// Where escalated proposals live: `<data_dir>/nightly/nightly-escalated.json`.
    pub fn escalated_path(data_dir: &Path) -> PathBuf {
        data_dir.join("nightly").join("nightly-escalated.json")
    }

    /// One escalation record.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct Escalation {
        pub id: String,
        pub kind: String,
        pub title: String,
        pub reason: String,
        pub attempts: usize,
        pub at_ms: i64,
    }

    /// Load escalations (empty when the file is absent).
    ///
    /// A torn file also loads as empty, with a loud warning: escalations
    /// are written atomically (see [`record_escalation`]), so a torn file
    /// can only predate atomic writes, and an escalation file that errors
    /// forever would block every later escalation.
    pub fn load_escalated(data_dir: &Path) -> Result<Vec<Escalation>, String> {
        match std::fs::read_to_string(escalated_path(data_dir)) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(all) => Ok(all),
                Err(e) => {
                    eprintln!(
                        "nightly: escalations {} are torn ({e}); treating as empty",
                        escalated_path(data_dir).display()
                    );
                    Ok(Vec::new())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("read escalations: {e}")),
        }
    }

    pub(crate) fn record_escalation(data_dir: &Path, esc: &Escalation) -> Result<(), String> {
        let mut all = load_escalated(data_dir)?;
        // One record per proposal: a re-escalation replaces the old one.
        if let Some(pos) = all.iter().position(|e| e.id == esc.id) {
            all[pos] = esc.clone();
        } else {
            all.push(esc.clone());
        }
        let path = escalated_path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(&all).map_err(|e| format!("encode escalations: {e}"))?;
        crate::apply::atomic_write(&path, &text)
            .map_err(|e| format!("write {}: {e}", path.display()))
    }

    /// Proposal kinds explicitly allowed to reach `Validated` with zero
    /// evals run. Currently none: every kind that reaches `gate()` must
    /// run its tagged evals. Memory lessons never reach the gate at all
    /// lib.rs handles them on a separate path ("the promotion rule IS the
    /// gate") - so the empty allowlist is deliberate, not an oversight.
    /// The match is exhaustive so adding a kind forces a conscious choice.
    fn eval_skip_allowed(kind: &ProposalKind) -> bool {
        match kind {
            ProposalKind::MemoryLesson { .. }
            | ProposalKind::Skill { .. }
            | ProposalKind::Persona { .. } => false,
        }
    }

    /// Validate one skill/persona proposal through the bounded fix loop.
    ///
    /// Returns `Validated` when eval-gating and replay-gating both pass
    /// (possibly after repairs), `Escalated` when the loop gives up. Every
    /// attempt and the escalation are pushed onto `events`; the caller
    /// audits them with the rest of the pass.
    ///
    /// `judge` scores replay transcripts (`ReplayCheck::Judge`); `repair`
    /// revises drafts on eval/replay reject - all draft revision goes
    /// through the Repair slot, never Reflection. `repair = None` means
    /// revision is unavailable: the loop falls back to plain retries,
    /// then escalation.
    ///
    /// Eval tags are immutable here: an eval reject revises the draft via
    /// the repair model and re-runs the full tag set; it never prunes
    /// tags to dodge a failing eval.
    #[allow(clippy::too_many_arguments)]
    pub fn validate_with_fix_loop(
        proposal: &mut Proposal,
        eval_runner: &dyn EvalRunner,
        replay_runner: &dyn ReplayRunner,
        replay_store: &ReplayStore,
        judge: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        config: &NightlyConfig,
        events: &mut Vec<NightlyEvent>,
        at_ms: i64,
    ) -> FixOutcome {
        let mut attempts = 0;
        loop {
            // ---- Eval gate: the FULL immutable tag set runs every pass. ----
            let tags: Vec<&str> = proposal.eval_tags.iter().map(String::as_str).collect();
            match gate(proposal, &tags, eval_runner, config) {
                EvalVerdict::Pass(_) => {}
                EvalVerdict::Skipped(reason) => {
                    if !eval_skip_allowed(&proposal.kind) {
                        return escalate(
                            &config.data_dir,
                            proposal,
                            format!(
                                "eval gate skipped ({}); kind '{}' is not allowlisted to skip eval-gating",
                                reason,
                                proposal.kind_name()
                            ),
                            attempts,
                            events,
                            at_ms,
                        );
                    }
                }
                EvalVerdict::Reject(reason) => {
                    if attempts >= config.max_fix_attempts {
                        return escalate(
                            &config.data_dir,
                            proposal,
                            format!("eval gate rejected: {reason}"),
                            attempts,
                            events,
                            at_ms,
                        );
                    }
                    attempts += 1;
                    let detail = match try_eval_sharpen(repair, proposal, &reason) {
                        Some(sharpened) => {
                            proposal.body = sharpened;
                            "sharpened draft via Repair slot; re-running gate"
                        }
                        None => "re-running eval gate (flakiness retry)",
                    };
                    events.push(NightlyEvent::FixAttempt {
                        id: proposal.id.clone(),
                        attempt: attempts,
                        phase: "eval".into(),
                        detail: detail.into(),
                        at_ms,
                    });
                    continue;
                }
            }

            // ---- Replay gate: strict improvement on held-out tasks. ----
            match replay_gate(proposal, replay_store, replay_runner, judge, config) {
                ReplayVerdict::Pass { summary } => {
                    events.push(NightlyEvent::ReplayPassed {
                        id: proposal.id.clone(),
                        summary,
                        at_ms,
                    });
                    return FixOutcome::Validated;
                }
                ReplayVerdict::Fail { reason } => {
                    // Infrastructure failures (no tasks, spawn/timeout/
                    // scoring errors) are not repairable by revision:
                    // escalate immediately, no attempts consumed.
                    if !fair_measurement(&reason) {
                        return escalate(
                            &config.data_dir,
                            proposal,
                            format!("replay gate failed: {reason}"),
                            attempts,
                            events,
                            at_ms,
                        );
                    }
                    if attempts >= config.max_fix_attempts {
                        return escalate(
                            &config.data_dir,
                            proposal,
                            format!("replay gate rejected: {reason}"),
                            attempts,
                            events,
                            at_ms,
                        );
                    }
                    attempts += 1;
                    let detail = match try_llm_sharpen(repair, proposal, &reason) {
                        Some(sharpened) => {
                            proposal.body = sharpened;
                            "sharpened draft via Repair slot; re-running gates"
                        }
                        None => "re-running replay A/B pair (flakiness retry)",
                    };
                    events.push(NightlyEvent::FixAttempt {
                        id: proposal.id.clone(),
                        attempt: attempts,
                        phase: "replay".into(),
                        detail: detail.into(),
                        at_ms,
                    });
                }
            }
        }
    }

    /// A "fair measurement" failure is the one the fix loop can address:
    /// the replay ran cleanly but the proposal didn't strictly improve.
    /// Every other replay failure is infrastructure.
    fn fair_measurement(reason: &str) -> bool {
        reason.contains("no strict improvement")
    }

    /// Ask the repair aux model to revise `proposal` per `prompt`; the
    /// returned full draft becomes the new body. `None` when no repair
    /// model is configured or the call fails - the caller falls back to a
    /// plain retry.
    fn sharpen_draft(
        judge: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        proposal: &Proposal,
        prompt: String,
    ) -> Option<String> {
        let (llm, model) = judge?;
        let draft = Proposal {
            body: prompt,
            ..proposal.clone()
        };
        match llm.refine_proposal(model, &draft) {
            Ok(body) if !body.trim().is_empty() => Some(body),
            _ => None,
        }
    }

    /// With LLM steps enabled, ask the repair aux model to revise a draft
    /// that failed eval-gating, feeding it the eval failure detail. `None`
    /// when no repair model is configured or the call fails - the caller
    /// falls back to a plain gate re-run (flakiness retry).
    fn try_eval_sharpen(
        repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        proposal: &Proposal,
        reason: &str,
    ) -> Option<String> {
        sharpen_draft(
            repair,
            proposal,
            format!(
                "The following skill/persona draft failed eval validation: {reason}.\n\nRevise it so every relevant eval passes with the draft applied. Keep it concrete and minimal.\n\nCurrent draft:\n{}\n\nReply with the full revised draft and nothing else.",
                proposal.body
            ),
        )
    }

    /// With LLM steps enabled, ask the repair aux model to sharpen a draft
    /// that failed to improve replay. `None` when no repair model is
    /// configured or the call fails - the caller falls back to a plain
    /// retry.
    fn try_llm_sharpen(
        repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        proposal: &Proposal,
        reason: &str,
    ) -> Option<String> {
        sharpen_draft(
            repair,
            proposal,
            format!(
                "The following skill/persona draft failed replay validation: {reason}.\n\nRevise it so a held-out task replay strictly improves with the draft applied. Keep it concrete and minimal.\n\nCurrent draft:\n{}\n\nReply with the full revised draft and nothing else.",
                proposal.body
            ),
        )
    }

    fn escalate(
        data_dir: &Path,
        proposal: &mut Proposal,
        reason: String,
        attempts: usize,
        events: &mut Vec<NightlyEvent>,
        at_ms: i64,
    ) -> FixOutcome {
        proposal.status = ProposalStatus::NeedsAttention;
        events.push(NightlyEvent::Escalated {
            id: proposal.id.clone(),
            reason: reason.clone(),
            attempts,
            at_ms,
        });
        let esc = Escalation {
            id: proposal.id.clone(),
            kind: proposal.kind_name().to_string(),
            title: proposal.title.clone(),
            reason: reason.clone(),
            attempts,
            at_ms: now_ms(),
        };
        // A failed escalation write must not lose the verdict: the audit
        // event already records it; the JSON file is the readable surface.
        let _ = record_escalation(data_dir, &esc);
        FixOutcome::Escalated { reason }
    }

    // Small deterministic invariant tests only; behavioral coverage lives
    // in eval/tests/nightly.rs (via the public `validate_with_fix_loop`
    // API).
}

pub mod repair_targets {
    //! Nightly repair of broken operational targets: MCP servers, scheduled
    //! jobs, and tools.
    //!
    //! Detection is snapshot-driven. The host hands the pass one snapshot per
    //! target through [`McpRepairTarget`], [`ScheduleRepairTarget`], and
    //! [`ToolRepairTarget`]; the pass walks a bounded repair ladder per
    //! broken target:
    //!
    //! 1. a cheap deterministic retry (re-probe the server / re-run the
    //!    check) - the flapping-target second chance;
    //! 2. LLM diagnosis through the `Repair` auxiliary slot
    //!    ([`NightlyLlm::diagnose_repair`]) - advisory only, recorded in the
    //!    audit log and any escalation; skipped silently when the slot is
    //!    unconfigured or the call fails;
    //! 3. a bounded config repair (re-resolve env/paths; normalize a broken
    //!    cron expression);
    //! 4. contain: **disable** the MCP server / tool, or **pause** the
    //!    scheduled job, and escalate to `nightly-escalated.json` with kind
    //!    `mcp-server` / `schedule` / `tool`.
    //!
    //! Step 4 is the floor: a target the pass cannot fix is contained and
    //! escalated - never left retrying silently, never repaired into a worse
    //! state. Every attempt is audited with the existing
    //! [`NightlyEvent::FixAttempt`] shape; escalations reuse
    //! [`NightlyEvent::Escalated`] and the fix loop's escalation file.
    //!
    //! The pass never touches `McpManager`, `TickDriver`, or `ToolRegistry`
    //! directly: hosts implement the three traits (production adapters are
    //! host wiring; tests use fakes), so this crate's dependency set is
    //! unchanged. `Disabled` MCP servers and paused jobs are operator intent
    //! and are never repair targets; unapproved MCP servers wait on a human
    //! and are never touched either.
    //!
    //! Boundedness: at most two mutating attempts per target (retry, then
    //! re-resolve) before containment; targets are visited in sorted order;
    //! adapters must bound each call (suggested ≤ 30s) - a repair phase that
    //! hangs the nightly pass is a bug.

    use crate::audit::NightlyEvent;
    use crate::fixloop::{record_escalation, Escalation};
    use crate::{NightlyConfig, NightlyLlm};
    use pantheon_api::model::AuxiliaryModel;
    use pantheon_scheduler::cron::{normalize_cron_expr, CronSchedule};
    use std::path::Path;

    /// One MCP server as the repair phase sees it. The host builds these
    /// from `McpManager::health()` / `check_health()`.
    #[derive(Debug, Clone)]
    pub struct McpServerSnapshot {
        pub name: String,
        /// `ServerStatus::as_str()`: "disabled" | "unapproved" |
        /// "connecting" | "ready" | "backoff" | "failed".
        pub status: String,
        /// Consecutive failures (the manager resets this on success).
        pub failures: u32,
        pub last_error: Option<String>,
    }

    /// One scheduled job as the repair phase sees it. The host builds these
    /// from `schedule.json` plus [`pantheon_scheduler::RunHistory`].
    #[derive(Debug, Clone)]
    pub struct ScheduledJobSnapshot {
        pub id: String,
        /// "cron" | "interval" | "one-shot".
        pub kind: String,
        /// The cron expression, for cron jobs.
        pub cron_expr: Option<String>,
        pub paused: bool,
        /// Consecutive run failures from the run history.
        pub consecutive_failures: u32,
        pub last_error: Option<String>,
    }

    /// Per-tool invocation health over the pass's ledger scan window. Errors
    /// are attributed via failed turns, exactly like the `RepeatedFailure`
    /// signal: an "invocation error" is a call whose turn failed.
    #[derive(Debug, Clone)]
    pub struct ToolCallStats {
        pub name: String,
        pub calls: u64,
        pub errors: u64,
    }

    /// What the host lets the nightly pass do to its MCP servers.
    pub trait McpRepairTarget {
        /// Fresh health snapshot of every managed server.
        fn servers(&mut self) -> Vec<McpServerSnapshot>;
        /// One more real connect attempt (the flapping-server second chance).
        fn retry_connect(&mut self, name: &str) -> Result<(), String>;
        /// Re-resolve the server's config - env vars and paths may have
        /// changed since it was configured - then retry the connection.
        fn re_resolve(&mut self, name: &str) -> Result<(), String>;
        /// Disable the server: stop reconnect attempts, keep the spec for the
        /// operator. The escalation tells the human to mirror the disable in
        /// config.
        fn disable(&mut self, name: &str, reason: &str) -> Result<(), String>;
    }

    /// What the host lets the nightly pass do to its scheduled jobs.
    pub trait ScheduleRepairTarget {
        /// Snapshot of every job plus its run-history counters.
        fn jobs(&mut self) -> Vec<ScheduledJobSnapshot>;
        /// Apply a repaired cron expression (already validated by the pass).
        /// The host persists it to `schedule.json` and resets the job's run
        /// history.
        fn repair_cron(&mut self, id: &str, new_expr: &str) -> Result<(), String>;
        /// Pause the job: it stops firing until a human resumes it.
        fn pause(&mut self, id: &str, reason: &str) -> Result<(), String>;
    }

    /// What the host lets the nightly pass do to its tools.
    pub trait ToolRepairTarget {
        /// Smoke-probe one tool: an empty-args invocation behind the
        /// capability policy. Called only for names in
        /// [`NightlyConfig::tool_probe_allowlist`]; hosts must refuse to
        /// probe anything else (probing an arbitrary tool executes it).
        fn probe(&mut self, name: &str) -> Result<(), String>;
        /// Re-resolve the tool's backing config (plugin path/manifest, MCP
        /// server env/paths) and probe again.
        fn re_resolve(&mut self, name: &str) -> Result<(), String>;
        /// Disable the tool: remove it from the live registry and record the
        /// disable so the next registry build skips it.
        fn disable(&mut self, name: &str, reason: &str) -> Result<(), String>;
    }

    /// The world the repair phase acts on. Every slot is `Option`: a host
    /// that does not manage MCP servers (or schedules, or tools) leaves its
    /// slot `None` and that category is skipped.
    #[derive(Default)]
    pub struct RepairTargets<'a> {
        pub mcp: Option<&'a mut dyn McpRepairTarget>,
        pub schedule: Option<&'a mut dyn ScheduleRepairTarget>,
        pub tools: Option<&'a mut dyn ToolRepairTarget>,
    }

    /// What the phase did to one target.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RepairOutcome {
        /// Healthy (or below threshold): no action.
        Healthy,
        /// A repair step fixed it.
        Repaired,
        /// Disabled (MCP server / tool) or paused (scheduled job), always
        /// paired with an escalation.
        Contained,
        /// Dry run: detected, audited, not mutated.
        SkippedDryRun,
    }

    /// One target's repair result, for the pass report.
    #[derive(Debug, Clone)]
    pub struct RepairReport {
        /// "mcp:<name>" | "schedule:<id>" | "tool:<name>".
        pub target: String,
        pub outcome: RepairOutcome,
        pub detail: String,
    }

    /// Diagnose one broken target through the `Repair` auxiliary slot.
    ///
    /// Advisory only: the returned text is recorded in the audit log and any
    /// escalation; repair *actions* stay deterministic. `None` when no repair
    /// model is configured, LLM steps are off, or the call fails - the
    /// deterministic ladder proceeds either way. The pass never fails for a
    /// missing repair model.
    fn diagnose(
        repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        category: &str,
        target: &str,
        context: &str,
    ) -> Option<String> {
        let (llm, aux) = repair?;
        let prompt = format!(
            "Category: {category}\nTarget: {target}\n{context}\n\
             Diagnose the likely root cause and suggest the concrete, \
             non-destructive config fix (env var, path, command, args, \
             schedule expression). Keep it short and specific."
        );
        match llm.diagnose_repair(aux, &prompt) {
            Ok(text) => {
                let text = text.trim();
                if text.is_empty() {
                    None
                } else {
                    // Audit lines stay readable; the full text rides the
                    // escalation reason.
                    Some(truncate(text, 2000))
                }
            }
            Err(_) => None,
        }
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }

    fn fix_attempt(
        events: &mut Vec<NightlyEvent>,
        id: &str,
        attempt: usize,
        phase: &str,
        detail: String,
        at_ms: i64,
    ) {
        events.push(NightlyEvent::FixAttempt {
            id: id.to_string(),
            attempt,
            phase: phase.to_string(),
            detail,
            at_ms,
        });
    }

    fn escalate_target(events: &mut Vec<NightlyEvent>, data_dir: &Path, esc: Escalation) {
        events.push(NightlyEvent::Escalated {
            id: esc.id.clone(),
            reason: esc.reason.clone(),
            attempts: esc.attempts,
            at_ms: esc.at_ms,
        });
        // A failed escalation write must not lose the verdict: the audit
        // event already records it; the JSON file is the readable surface.
        let _ = record_escalation(data_dir, &esc);
    }

    fn with_diagnosis(base: String, diagnosis: &Option<String>) -> String {
        match diagnosis {
            Some(d) => format!("{base} Repair-model diagnosis: {d}"),
            None => base,
        }
    }

    /// Run the repair phase: MCP servers, then scheduled jobs, then tools.
    ///
    /// `repair` is the resolved (`Repair`-slot) diagnoser, or `None` for a
    /// deterministic-only pass. Every attempt and escalation is pushed onto
    /// `events`; the caller audits them with the rest of the pass.
    #[allow(clippy::too_many_arguments)]
    pub fn run_repair_phase(
        config: &NightlyConfig,
        data_dir: &Path,
        targets: &mut RepairTargets<'_>,
        tool_stats: &[ToolCallStats],
        repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        events: &mut Vec<NightlyEvent>,
        at_ms: i64,
    ) -> Vec<RepairReport> {
        let mut reports = Vec::new();
        if let Some(mcp) = targets.mcp.as_mut() {
            repair_mcp(config, data_dir, *mcp, repair, events, at_ms, &mut reports);
        }
        if let Some(sched) = targets.schedule.as_mut() {
            repair_schedules(
                config,
                data_dir,
                *sched,
                repair,
                events,
                at_ms,
                &mut reports,
            );
        }
        if let Some(tools) = targets.tools.as_mut() {
            repair_tools(
                config,
                data_dir,
                *tools,
                tool_stats,
                repair,
                events,
                at_ms,
                &mut reports,
            );
        }
        reports
    }

    fn is_broken_server(snap: &McpServerSnapshot, max_failures: u32) -> bool {
        (snap.status == "failed" || snap.status == "backoff") && snap.failures >= max_failures
    }

    #[allow(clippy::too_many_arguments)]
    fn repair_mcp(
        config: &NightlyConfig,
        data_dir: &Path,
        mcp: &mut dyn McpRepairTarget,
        repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        events: &mut Vec<NightlyEvent>,
        at_ms: i64,
        reports: &mut Vec<RepairReport>,
    ) {
        let mut snaps = mcp.servers();
        snaps.sort_by(|a, b| a.name.cmp(&b.name));
        for snap in snaps {
            let id = format!("mcp:{}", snap.name);
            // Operator intent or a human's pending decision: never targets.
            if snap.status == "disabled" || snap.status == "unapproved" {
                continue;
            }
            if !is_broken_server(&snap, config.mcp_max_failures) {
                // Label distinctly: only "ready" is healthy. A failed /
                // backoff server below the repair threshold is not
                // healthy, it is below threshold - no action, but the
                // report must not call it Healthy.
                let detail = if snap.status == "ready" {
                    format!("status=ready failures={}", snap.failures)
                } else {
                    format!(
                        "status={} failures={} (below repair threshold, not ready)",
                        snap.status, snap.failures
                    )
                };
                reports.push(RepairReport {
                    target: id,
                    outcome: RepairOutcome::Healthy,
                    detail,
                });
                continue;
            }
            let ctx = format!(
                "Status: {} ({} consecutive failures)\nLast error: {}",
                snap.status,
                snap.failures,
                snap.last_error.as_deref().unwrap_or("(none)")
            );
            if config.dry_run {
                fix_attempt(
                    events,
                    &id,
                    0,
                    "mcp",
                    format!("dry run: would repair ({ctx})"),
                    at_ms,
                );
                reports.push(RepairReport {
                    target: id,
                    outcome: RepairOutcome::SkippedDryRun,
                    detail: ctx,
                });
                continue;
            }
            // Attempt 1: one more real connect - the flapping-server second
            // chance.
            match mcp.retry_connect(&snap.name) {
                Ok(()) => {
                    fix_attempt(
                        events,
                        &id,
                        1,
                        "mcp",
                        format!(
                            "retry_connect succeeded after {} failures; server recovered",
                            snap.failures
                        ),
                        at_ms,
                    );
                    reports.push(RepairReport {
                        target: id,
                        outcome: RepairOutcome::Repaired,
                        detail: "retry_connect recovered the server".into(),
                    });
                    continue;
                }
                Err(e1) => {
                    fix_attempt(
                        events,
                        &id,
                        1,
                        "mcp",
                        format!("retry_connect failed: {e1}"),
                        at_ms,
                    );
                    // Attempt 2 (advisory): repair-model diagnosis.
                    let diagnosis = diagnose(repair, "mcp-server", &snap.name, &ctx);
                    if let Some(d) = &diagnosis {
                        fix_attempt(
                            events,
                            &id,
                            2,
                            "mcp",
                            format!("repair-model diagnosis: {d}"),
                            at_ms,
                        );
                    }
                    // Attempt 3: re-resolve config (env/paths may have
                    // changed), then retry.
                    match mcp.re_resolve(&snap.name) {
                        Ok(()) => {
                            fix_attempt(
                                events,
                                &id,
                                3,
                                "mcp",
                                "re_resolve + retry succeeded; server recovered".into(),
                                at_ms,
                            );
                            reports.push(RepairReport {
                                target: id,
                                outcome: RepairOutcome::Repaired,
                                detail: "re_resolve recovered the server".into(),
                            });
                        }
                        Err(e2) => {
                            let reason = with_diagnosis(
                                format!(
                                    "mcp server '{}' unrecoverable: retry failed ({e1}); \
                                     config re-resolve failed ({e2}). Disabled to stop \
                                     reconnect churn; re-enable in [mcp.servers] after fixing.",
                                    snap.name
                                ),
                                &diagnosis,
                            );
                            match mcp.disable(&snap.name, &reason) {
                                Ok(()) => {}
                                Err(e) => {
                                    // Containment failed too: still escalate
                                    // the human must know. The audit trail
                                    // carries both failures.
                                    fix_attempt(
                                        events,
                                        &id,
                                        4,
                                        "mcp",
                                        format!("disable failed: {e}; escalating anyway"),
                                        at_ms,
                                    );
                                }
                            }
                            escalate_target(
                                events,
                                data_dir,
                                Escalation {
                                    id: id.clone(),
                                    kind: "mcp-server".into(),
                                    title: format!("MCP server '{}' disabled", snap.name),
                                    reason: reason.clone(),
                                    attempts: 3,
                                    at_ms,
                                },
                            );
                            reports.push(RepairReport {
                                target: id,
                                outcome: RepairOutcome::Contained,
                                detail: reason,
                            });
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repair_schedules(
        config: &NightlyConfig,
        data_dir: &Path,
        sched: &mut dyn ScheduleRepairTarget,
        repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        events: &mut Vec<NightlyEvent>,
        at_ms: i64,
        reports: &mut Vec<RepairReport>,
    ) {
        let mut jobs = sched.jobs();
        jobs.sort_by(|a, b| a.id.cmp(&b.id));
        for job in jobs {
            let id = format!("schedule:{}", job.id);
            if job.paused {
                continue;
            }
            // Config-shaped breakage first: a cron expression that cannot
            // parse means the job silently never fires.
            if job.kind == "cron" {
                if let Some(expr) = &job.cron_expr {
                    if CronSchedule::validate(expr).is_err() {
                        repair_bad_cron(
                            config, data_dir, sched, repair, events, at_ms, reports, &job, expr,
                            &id,
                        );
                        continue;
                    }
                }
            }
            if job.consecutive_failures < config.schedule_max_failures {
                reports.push(RepairReport {
                    target: id,
                    outcome: RepairOutcome::Healthy,
                    detail: format!("consecutive_failures={}", job.consecutive_failures),
                });
                continue;
            }
            let ctx = format!(
                "Kind: {}\nConsecutive run failures: {}\nLast error: {}",
                job.kind,
                job.consecutive_failures,
                job.last_error.as_deref().unwrap_or("(none)")
            );
            if config.dry_run {
                fix_attempt(
                    events,
                    &id,
                    0,
                    "schedule",
                    format!("dry run: would pause ({ctx})"),
                    at_ms,
                );
                reports.push(RepairReport {
                    target: id,
                    outcome: RepairOutcome::SkippedDryRun,
                    detail: ctx,
                });
                continue;
            }
            // No retry-the-job ladder: re-firing a failing job is not the
            // nightly's job. Diagnose (advisory), then pause + escalate - a
            // failing cron spamming errors nightly is worse than a paused one.
            let diagnosis = diagnose(repair, "scheduled-job", &job.id, &ctx);
            if let Some(d) = &diagnosis {
                fix_attempt(
                    events,
                    &id,
                    1,
                    "schedule",
                    format!("repair-model diagnosis: {d}"),
                    at_ms,
                );
            }
            let reason = with_diagnosis(
                format!(
                    "scheduled job '{}' failed {} consecutive run(s); last error: {}. \
                     Paused to stop the nightly error spam; resume after fixing.",
                    job.id,
                    job.consecutive_failures,
                    job.last_error.as_deref().unwrap_or("(none)")
                ),
                &diagnosis,
            );
            match sched.pause(&job.id, &reason) {
                Ok(()) => {}
                Err(e) => fix_attempt(
                    events,
                    &id,
                    2,
                    "schedule",
                    format!("pause failed: {e}; escalating anyway"),
                    at_ms,
                ),
            }
            escalate_target(
                events,
                data_dir,
                Escalation {
                    id: id.clone(),
                    kind: "schedule".into(),
                    title: format!("Scheduled job '{}' paused", job.id),
                    reason: reason.clone(),
                    attempts: 1,
                    at_ms,
                },
            );
            reports.push(RepairReport {
                target: id,
                outcome: RepairOutcome::Contained,
                detail: reason,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repair_bad_cron(
        config: &NightlyConfig,
        data_dir: &Path,
        sched: &mut dyn ScheduleRepairTarget,
        repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        events: &mut Vec<NightlyEvent>,
        at_ms: i64,
        reports: &mut Vec<RepairReport>,
        job: &ScheduledJobSnapshot,
        expr: &str,
        id: &str,
    ) {
        if config.dry_run {
            fix_attempt(
                events,
                id,
                0,
                "schedule",
                format!("dry run: cron expression '{expr}' does not parse; would attempt repair"),
                at_ms,
            );
            reports.push(RepairReport {
                target: id.to_string(),
                outcome: RepairOutcome::SkippedDryRun,
                detail: format!("unparseable cron '{expr}'"),
            });
            return;
        }
        // Bounded deterministic repair: normalize common shapes, then
        // re-validate. Anything else is not safely guessable.
        match normalize_cron_expr(expr).filter(|fixed| CronSchedule::validate(fixed).is_ok()) {
            Some(fixed) => match sched.repair_cron(&job.id, &fixed) {
                Ok(()) => {
                    fix_attempt(
                        events,
                        id,
                        1,
                        "schedule",
                        format!("normalized cron '{expr}' → '{fixed}'; job repaired"),
                        at_ms,
                    );
                    reports.push(RepairReport {
                        target: id.to_string(),
                        outcome: RepairOutcome::Repaired,
                        detail: format!("cron '{expr}' → '{fixed}'"),
                    });
                }
                Err(e) => {
                    fix_attempt(
                        events,
                        id,
                        1,
                        "schedule",
                        format!("repair_cron failed for '{expr}' → '{fixed}': {e}"),
                        at_ms,
                    );
                    pause_bad_cron(data_dir, sched, None, events, at_ms, reports, job, expr, id);
                }
            },
            None => {
                let diagnosis = diagnose(
                    repair,
                    "scheduled-job",
                    &job.id,
                    &format!(
                        "Cron expression '{expr}' does not parse and has no safe normalization."
                    ),
                );
                if let Some(d) = &diagnosis {
                    fix_attempt(
                        events,
                        id,
                        1,
                        "schedule",
                        format!("repair-model diagnosis: {d}"),
                        at_ms,
                    );
                }
                pause_bad_cron(
                    data_dir, sched, diagnosis, events, at_ms, reports, job, expr, id,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn pause_bad_cron(
        data_dir: &Path,
        sched: &mut dyn ScheduleRepairTarget,
        diagnosis: Option<String>,
        events: &mut Vec<NightlyEvent>,
        at_ms: i64,
        reports: &mut Vec<RepairReport>,
        job: &ScheduledJobSnapshot,
        expr: &str,
        id: &str,
    ) {
        let reason = with_diagnosis(
            format!(
                "scheduled job '{}' has an unparseable cron expression '{expr}' and no safe \
                 normalization exists; it would silently never fire. Paused; fix the expression.",
                job.id
            ),
            &diagnosis,
        );
        if let Err(e) = sched.pause(&job.id, &reason) {
            fix_attempt(
                events,
                id,
                2,
                "schedule",
                format!("pause failed: {e}; escalating anyway"),
                at_ms,
            );
        }
        escalate_target(
            events,
            data_dir,
            Escalation {
                id: id.to_string(),
                kind: "schedule".into(),
                title: format!("Scheduled job '{}' paused (bad cron)", job.id),
                reason: reason.clone(),
                attempts: 1,
                at_ms,
            },
        );
        reports.push(RepairReport {
            target: id.to_string(),
            outcome: RepairOutcome::Contained,
            detail: reason,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn repair_tools(
        config: &NightlyConfig,
        data_dir: &Path,
        tools: &mut dyn ToolRepairTarget,
        tool_stats: &[ToolCallStats],
        repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
        events: &mut Vec<NightlyEvent>,
        at_ms: i64,
        reports: &mut Vec<RepairReport>,
    ) {
        // Broken by stats: every invocation in the window errored.
        let mut broken: Vec<(String, String)> = tool_stats
            .iter()
            .filter(|s| s.calls >= config.tool_min_calls as u64 && s.errors >= s.calls)
            .map(|s| {
                (
                    s.name.clone(),
                    format!("{} of {} recent invocation(s) errored", s.errors, s.calls),
                )
            })
            .collect();
        // Broken by smoke probe (opt-in allowlist only).
        for name in &config.tool_probe_allowlist {
            if broken.iter().any(|(n, _)| n == name) {
                continue;
            }
            match tools.probe(name) {
                Ok(()) => {}
                Err(e) => broken.push((name.clone(), format!("smoke probe failed: {e}"))),
            }
        }
        broken.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, why) in broken {
            let id = format!("tool:{name}");
            let ctx = format!("Tool '{name}': {why}.");
            if config.dry_run {
                fix_attempt(
                    events,
                    &id,
                    0,
                    "tool",
                    format!("dry run: would repair ({ctx})"),
                    at_ms,
                );
                reports.push(RepairReport {
                    target: id,
                    outcome: RepairOutcome::SkippedDryRun,
                    detail: ctx,
                });
                continue;
            }
            // Attempt 1: re-resolve the backing config, then probe again.
            match tools.re_resolve(&name) {
                Ok(()) => {
                    fix_attempt(
                        events,
                        &id,
                        1,
                        "tool",
                        format!("re_resolve + probe succeeded; tool '{name}' recovered"),
                        at_ms,
                    );
                    reports.push(RepairReport {
                        target: id,
                        outcome: RepairOutcome::Repaired,
                        detail: format!("re_resolve recovered '{name}'"),
                    });
                }
                Err(e1) => {
                    fix_attempt(
                        events,
                        &id,
                        1,
                        "tool",
                        format!("re_resolve failed for '{name}': {e1}"),
                        at_ms,
                    );
                    let diagnosis = diagnose(repair, "tool", &name, &ctx);
                    if let Some(d) = &diagnosis {
                        fix_attempt(
                            events,
                            &id,
                            2,
                            "tool",
                            format!("repair-model diagnosis: {d}"),
                            at_ms,
                        );
                    }
                    let reason = with_diagnosis(
                        format!(
                            "tool '{name}' unrecoverable ({why}); re-resolve failed ({e1}). \
                             Disabled so the agent loop stops offering it; re-enable after fixing.",
                            why = why,
                            e1 = e1,
                        ),
                        &diagnosis,
                    );
                    match tools.disable(&name, &reason) {
                        Ok(()) => {}
                        Err(e) => fix_attempt(
                            events,
                            &id,
                            3,
                            "tool",
                            format!("disable failed: {e}; escalating anyway"),
                            at_ms,
                        ),
                    }
                    escalate_target(
                        events,
                        data_dir,
                        Escalation {
                            id: id.clone(),
                            kind: "tool".into(),
                            title: format!("Tool '{name}' disabled"),
                            reason: reason.clone(),
                            attempts: 2,
                            at_ms,
                        },
                    );
                    reports.push(RepairReport {
                        target: id,
                        outcome: RepairOutcome::Contained,
                        detail: reason,
                    });
                }
            }
        }
    }

    // Small deterministic invariant tests only: id shapes, truncation,
    // broken-server predicate. Behavioral tests live in
    // `eval/tests/nightly_repair.rs`.
}

pub mod replay {
    //! Replay validation: the improvement gate.
    //!
    //! Eval-gating ([`crate::gate`]) proves a proposal doesn't break
    //! anything. Replay proves it makes something *better* - the SkillOpt
    //! learning rule: a Skill/Persona proposal ships only if it strictly
    //! improves a held-out task's score, measured by replaying the task with
    //! and without the candidate applied.
    //!
    //! The held-out task set lives at `<data_dir>/nightly/replay-tasks.json`
    //! and is managed through the crate API so every client (TUI, web,
    //! mobile) shares one set. Tasks the optimizer trains on must never
    //! appear here; this set is validation-only.
    //!
    //! When no replay tasks are configured, the gate REJECTS every
    //! skill/persona proposal - loudly, in the audit log. Strict
    //! improvement is mandatory; "no tasks to measure against" is not a
    //! pass. Configure tasks with the `replay-tasks` CLI.

    use crate::propose::Proposal;
    use crate::NightlyConfig;
    use pantheon_api::model::AuxiliaryModel;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// How a replayed task is scored.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ReplayCheck {
        /// The transcript must contain these tool calls in order. Score 1.0
        /// when the full sequence appears, 0.0 otherwise.
        ToolSequence { tools: Vec<String> },
        /// The transcript must (not) contain this text. Score 1.0 on match.
        Contains { text: String, negate: bool },
        /// An LLM judge scores the transcript 0.0-1.0 against the rubric.
        /// Requires LLM steps enabled; otherwise the task is skipped.
        Judge { rubric: String },
    }

    /// One held-out validation task.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct ReplayTask {
        pub id: String,
        pub name: String,
        pub prompt: String,
        pub check: ReplayCheck,
        /// How this task runs itself. When present, the built-in task runner
        /// executes it directly - no `[nightly] replay_command` needed.
        /// When absent, the task needs the external headless-agent command.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub exec: Option<TaskExec>,
    }

    /// A self-contained, deterministic task execution spec.
    ///
    /// The built-in runner spawns `command` with `args` plus the task
    /// `prompt` as the final argument, in a fresh empty working directory,
    /// with a deadline. Stdout is the transcript that gets scored. Extra
    /// `env` entries override the inherited environment for the run.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct TaskExec {
        /// Program to run, resolved via PATH.
        pub command: String,
        /// Fixed argv before the task prompt.
        #[serde(default)]
        pub args: Vec<String>,
        /// Extra environment variables for the run.
        #[serde(default)]
        pub env: std::collections::HashMap<String, String>,
    }

    /// Runs a task and returns the transcript for scoring.
    ///
    /// The `with_proposal` contract: when `Some`, the runner must make the
    /// proposal's effect visible to the replay - for a Skill proposal, the
    /// candidate `SKILL.md` is staged where the replayed agent will load it;
    /// for Persona, the persona note is injected. When `None`, the replay
    /// runs clean. Implementations must be bounded (timeout).
    pub trait ReplayRunner {
        fn run_transcript(
            &self,
            task: &ReplayTask,
            with_proposal: Option<&Proposal>,
        ) -> Result<String, String>;
    }

    /// Verdict over the whole replay gate.
    #[derive(Debug, Clone)]
    pub enum ReplayVerdict {
        /// Total score with the proposal strictly exceeds the baseline.
        Pass { summary: String },
        /// No strict improvement - or the improvement couldn't be measured
        /// (no replay tasks configured, a replay errored, a score wasn't
        /// finite). The proposal is rejected; the reason names the cause.
        /// Missing or broken replay infrastructure NEVER degrades to a
        /// pass: "couldn't measure" is not "improved".
        Fail { reason: String },
    }

    /// JSON store for the held-out task set.
    pub struct ReplayStore {
        path: PathBuf,
        tasks: Vec<ReplayTask>,
    }

    fn store_path(data_dir: &Path) -> PathBuf {
        data_dir.join("nightly").join("replay-tasks.json")
    }

    impl ReplayStore {
        pub fn open(data_dir: &Path) -> Result<Self, String> {
            let path = store_path(data_dir);
            let tasks = match std::fs::read_to_string(&path) {
                Ok(text) => serde_json::from_str(&text)
                    .map_err(|e| format!("parse {}: {e}", path.display()))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(format!("read {}: {e}", path.display())),
            };
            Ok(Self { path, tasks })
        }

        pub fn list(&self) -> &[ReplayTask] {
            &self.tasks
        }

        /// Save a task. Validation is loud: empty id/name/prompt and empty
        /// check payloads are rejected instead of stored silently.
        pub fn save(&mut self, task: ReplayTask) -> Result<(), String> {
            validate_task(&task)?;
            if let Some(pos) = self.tasks.iter().position(|t| t.id == task.id) {
                self.tasks[pos] = task;
            } else {
                self.tasks.push(task);
            }
            self.persist()
        }

        pub fn delete(&mut self, id: &str) -> Result<bool, String> {
            let before = self.tasks.len();
            self.tasks.retain(|t| t.id != id);
            let removed = self.tasks.len() < before;
            if removed {
                self.persist()?;
            }
            Ok(removed)
        }

        fn persist(&self) -> Result<(), String> {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            let text = serde_json::to_string_pretty(&self.tasks)
                .map_err(|e| format!("encode replay tasks: {e}"))?;
            std::fs::write(&self.path, text)
                .map_err(|e| format!("write {}: {e}", self.path.display()))
        }
    }

    fn validate_task(task: &ReplayTask) -> Result<(), String> {
        if task.id.trim().is_empty() {
            return Err("replay task id must not be empty".into());
        }
        if task.name.trim().is_empty() {
            return Err("replay task name must not be empty".into());
        }
        if task.prompt.trim().is_empty() {
            return Err("replay task prompt must not be empty".into());
        }
        match &task.check {
            ReplayCheck::ToolSequence { tools } => {
                if tools.len() < 2 {
                    return Err("tool_sequence check needs at least 2 tools".into());
                }
            }
            ReplayCheck::Contains { text, .. } => {
                if text.trim().is_empty() {
                    return Err("contains check needs non-empty text".into());
                }
            }
            ReplayCheck::Judge { rubric } => {
                if rubric.trim().is_empty() {
                    return Err("judge check needs a non-empty rubric".into());
                }
            }
        }
        if let Some(exec) = &task.exec {
            if exec.command.trim().is_empty() {
                return Err("exec spec needs a non-empty command".into());
            }
            for k in exec.env.keys() {
                if k.trim().is_empty() || k.contains('=') {
                    return Err("exec env keys must be non-empty and not contain '='".into());
                }
            }
        }
        Ok(())
    }

    /// Score a transcript against a check. Deterministic checks are pure;
    /// [`ReplayCheck::Judge`] needs the LLM judge.
    fn score_transcript(
        check: &ReplayCheck,
        transcript: &str,
        judge: Option<(&dyn crate::llm::NightlyLlm, &AuxiliaryModel)>,
    ) -> Result<f64, String> {
        match check {
            ReplayCheck::ToolSequence { tools } => {
                // Every tool must appear in order. A transcript line counts
                // when it names the tool (tool-call markers or plain text).
                let mut idx = 0;
                for line in transcript.lines() {
                    if idx < tools.len() && line.contains(&tools[idx]) {
                        idx += 1;
                    }
                }
                Ok(if idx == tools.len() { 1.0 } else { 0.0 })
            }
            ReplayCheck::Contains { text, negate } => {
                let found = transcript.contains(text);
                Ok(if found != *negate { 1.0 } else { 0.0 })
            }
            ReplayCheck::Judge { rubric } => {
                let (llm, model) =
                    judge.ok_or_else(|| "judge check requires LLM steps enabled".to_string())?;
                // Judge scoring goes through the refine_proposal contract
                // (see score_with_judge); the prompt is built there.
                score_with_judge(llm, model, rubric, transcript)
            }
        }
    }

    fn score_with_judge(
        llm: &dyn crate::llm::NightlyLlm,
        model: &AuxiliaryModel,
        rubric: &str,
        transcript: &str,
    ) -> Result<f64, String> {
        // The judge prompt is built as a synthetic proposal draft so it
        // flows through the existing `refine_proposal` contract, which is
        // the trait's scoring-shaped entrypoint. The returned text must be
        // a bare number.
        let draft = crate::propose::Proposal {
            id: "nly_judge".into(),
            kind: crate::propose::ProposalKind::MemoryLesson {
                key: "judge".into(),
            },
            title: "replay judge".into(),
            body: format!(
                "Score the following transcript 0.0 to 1.0 against this rubric: {rubric}\n\nTranscript:\n{transcript}\n\nReply with only the number, no other text."
            ),
            provenance_runs: Vec::new(),
            provenance_turns: Vec::new(),
            eval_tags: Vec::new(),
            status: crate::propose::ProposalStatus::Proposed,
        };
        let raw = llm.refine_proposal(model, &draft)?;
        let score: f64 = raw
            .trim()
            .parse()
            .map_err(|_| format!("judge returned non-numeric score: {raw:?}"))?;
        if !score.is_finite() {
            return Err(format!("judge returned non-finite score: {raw:?}"));
        }
        Ok(score.clamp(0.0, 1.0))
    }

    /// The replay gate: strict improvement required.
    ///
    /// For every applicable task, the task is replayed with and without the
    /// proposal. The proposal passes only when the total with-score strictly
    /// exceeds the total baseline score. Equal scores fail - "didn't make it
    /// worse" is what eval-gating already proves; this gate proves "made it
    /// better".
    ///
    /// Anything that prevents a fair measurement fails the proposal:
    /// - no replay tasks configured → reject (configure some first)
    /// - no tasks scorable (e.g. only judge tasks, LLM disabled) → reject
    /// - a replay errors → reject
    /// - a score isn't finite → reject
    pub fn replay_gate(
        proposal: &Proposal,
        store: &ReplayStore,
        runner: &dyn ReplayRunner,
        judge: Option<(&dyn crate::llm::NightlyLlm, &AuxiliaryModel)>,
        _config: &NightlyConfig,
    ) -> ReplayVerdict {
        let tasks = store.list();
        if tasks.is_empty() {
            return ReplayVerdict::Fail {
                reason: format!(
                    "proposal '{}': no replay tasks configured - strict improvement cannot be measured; add tasks with `pantheon nightly replay-tasks add`",
                    proposal.id
                ),
            };
        }
        let mut base_total = 0.0;
        let mut with_total = 0.0;
        let mut scored = 0usize;
        let mut skipped = 0usize;
        for task in tasks {
            if matches!(task.check, ReplayCheck::Judge { .. }) && judge.is_none() {
                skipped += 1;
                continue;
            }
            let base_tx = match runner.run_transcript(task, None) {
                Ok(t) => t,
                Err(e) => {
                    return ReplayVerdict::Fail {
                        reason: format!(
                            "proposal '{}': replay of '{}' failed (baseline): {e}",
                            proposal.id, task.id
                        ),
                    }
                }
            };
            let with_tx = match runner.run_transcript(task, Some(proposal)) {
                Ok(t) => t,
                Err(e) => {
                    return ReplayVerdict::Fail {
                        reason: format!(
                            "proposal '{}': replay of '{}' failed (with proposal): {e}",
                            proposal.id, task.id
                        ),
                    }
                }
            };
            let base = match score_transcript(&task.check, &base_tx, judge) {
                Ok(s) => s,
                Err(e) => {
                    return ReplayVerdict::Fail {
                        reason: format!(
                            "proposal '{}': scoring '{}' failed (baseline): {e}",
                            proposal.id, task.id
                        ),
                    }
                }
            };
            let with = match score_transcript(&task.check, &with_tx, judge) {
                Ok(s) => s,
                Err(e) => {
                    return ReplayVerdict::Fail {
                        reason: format!(
                            "proposal '{}': scoring '{}' failed (with proposal): {e}",
                            proposal.id, task.id
                        ),
                    }
                }
            };
            // A non-finite score means the measurement is broken; it can
            // never count as improvement.
            if !base.is_finite() || !with.is_finite() {
                return ReplayVerdict::Fail {
                    reason: format!(
                        "proposal '{}': non-finite replay score on '{}' (baseline {base:?}, with {with:?})",
                        proposal.id, task.id
                    ),
                };
            }
            base_total += base;
            with_total += with;
            scored += 1;
        }
        if scored == 0 {
            return ReplayVerdict::Fail {
                reason: format!(
                    "proposal '{}': {skipped} judge task(s) skipped (LLM disabled); nothing scorable",
                    proposal.id
                ),
            };
        }
        if with_total > base_total {
            let skipped_note = if skipped > 0 {
                format!(" ({skipped} judge task(s) skipped)")
            } else {
                String::new()
            };
            ReplayVerdict::Pass {
                summary: format!(
                    "replay: {with_total:.2} vs baseline {base_total:.2} across {scored} task(s){skipped_note}"
                ),
            }
        } else {
            ReplayVerdict::Fail {
                reason: format!(
                    "proposal '{}': no strict improvement (with {with_total:.2} vs baseline {base_total:.2} across {scored} task(s))",
                    proposal.id
                ),
            }
        }
    }

    /// A [`ReplayRunner`] that always fails: used when no replay command is
    /// configured (`[nightly] replay_command` absent), so the replay gate
    /// rejects loudly instead of silently passing. Pantheon ships no
    /// headless agent runner; point `replay_command` at one
    /// (`<command> <prompt>`, stdout = transcript) to enable skill/persona
    /// replay validation.
    pub struct UnconfiguredReplayRunner;

    impl ReplayRunner for UnconfiguredReplayRunner {
        fn run_transcript(
            &self,
            task: &ReplayTask,
            _with_proposal: Option<&Proposal>,
        ) -> Result<String, String> {
            Err(format!(
                "no replay command configured ([nightly] replay_command) - cannot replay task '{}'; configure a headless runner to enable replay validation",
                task.id
            ))
        }
    }

    /// A [`ReplayRunner`] that shells out to a headless agent command.
    ///
    /// Runs `<command> <prompt>` with a deadline, capturing stdout as the
    /// transcript. When a proposal is provided, `PANTHEON_REPLAY_SKILL_DIR`
    /// points at a staging dir containing the candidate skill/persona
    /// overlay, so the replayed agent sees exactly the change under test.
    /// (Skill overlays load through Pantheon's normal skill discovery;
    /// persona overlays are staged as `persona.md` for the runner to
    /// inject - the same overlay the runtime applies to live sessions, see
    /// [`crate::persona`].)
    pub struct SubprocessReplayRunner {
        pub command: String,
        pub timeout: Duration,
    }

    impl SubprocessReplayRunner {
        pub fn new(command: String, timeout: Duration) -> Self {
            Self { command, timeout }
        }
    }

    impl ReplayRunner for SubprocessReplayRunner {
        fn run_transcript(
            &self,
            task: &ReplayTask,
            with_proposal: Option<&Proposal>,
        ) -> Result<String, String> {
            use std::process::{Command, Stdio};

            let mut cmd = Command::new(&self.command);
            cmd.arg(&task.prompt)
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut staged: Option<std::path::PathBuf> = None;
            if let Some(p) = with_proposal {
                // Stage the proposal where the headless runner will pick it
                // up. The runner contract: $PANTHEON_REPLAY_SKILL_DIR holds
                // the candidate overlay.
                let dir = stage_proposal_overlay(p)?;
                cmd.env("PANTHEON_REPLAY_SKILL_DIR", &dir);
                staged = Some(dir);
            }
            let mut child = cmd
                .spawn()
                .map_err(|e| format!("spawn replay command '{}': {e}", self.command))?;
            let result = wait_with_drain(&mut child, self.timeout).and_then(|(out, status)| {
                if !status.success() {
                    return Err(format!(
                        "replay command exited {}",
                        status.code().unwrap_or(-1)
                    ));
                }
                String::from_utf8(out).map_err(|e| format!("replay output not UTF-8: {e}"))
            });
            // The staging dir is the runner's, not the child's: remove it
            // once the run ends, whatever the outcome. Leftovers from
            // crashed passes are swept at the next pass start (see
            // [`cleanup_stale_replay_staging`]).
            if let Some(dir) = staged {
                let _ = std::fs::remove_dir_all(&dir);
            }
            result
        }
    }

    /// Sequence making each staging dir unique within this process, so two
    /// concurrent passes staging the same proposal id never share (and
    /// overwrite) one directory.
    static STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Stage a proposal as a skill/persona overlay for the replay runner.
    /// Returns the staging dir path. Memory lessons need no overlay - they
    /// are data, not behavior - so this is a no-op dir for them.
    ///
    /// The path is built only from sanitized parts: the raw proposal id used
    /// to name the dir, so a skill name containing `/` escaped the staging
    /// dir entirely. The pid + sequence suffix keeps concurrent passes with
    /// the same proposal id on separate dirs.
    fn stage_proposal_overlay(p: &Proposal) -> Result<std::path::PathBuf, String> {
        let seq = STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-replay-{}-{}-{}",
            sanitize_id(&p.id),
            std::process::id(),
            seq
        ));
        std::fs::create_dir_all(&dir).map_err(|e| format!("create replay staging dir: {e}"))?;
        match &p.kind {
            crate::propose::ProposalKind::Skill { name, .. } => {
                std::fs::write(dir.join(format!("{}.md", sanitize_id(name))), &p.body)
                    .map_err(|e| format!("stage skill overlay: {e}"))?;
            }
            crate::propose::ProposalKind::Persona { .. } => {
                std::fs::write(dir.join("persona.md"), &p.body)
                    .map_err(|e| format!("stage persona overlay: {e}"))?;
            }
            crate::propose::ProposalKind::MemoryLesson { .. } => {}
        }
        Ok(dir)
    }

    /// Best-effort sweep of staging dirs left behind by crashed passes.
    /// Only dirs older than `max_age` are removed: a live pass holds its
    /// staging dir for at most one task run (bounded by the replay
    /// timeout) and removes it afterwards, so an old dir cannot belong to
    /// a live pass. Called at pass start (see `run_pass`).
    pub fn cleanup_stale_replay_staging(max_age: Duration) {
        let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        let cutoff = std::time::SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(std::time::UNIX_EPOCH);
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("pantheon-replay-") {
                continue;
            }
            let path = entry.path();
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }

    /// Wait for a spawned child up to `timeout`, draining its stdout on a
    /// reader thread the whole time. Without the concurrent drain, a child
    /// that writes more than the pipe buffer blocks forever on write while
    /// the parent polls `try_wait` - the deadline then kills it and the
    /// failure is misreported as a timeout instead of a verbose success.
    /// Returns the drained stdout plus the exit status.
    ///
    /// On timeout the child is killed and the drain thread is detached
    /// (dropped, not joined): a grandchild inheriting stdout could hold
    /// the pipe open past the child's death, and joining would hang the
    /// caller that already decided the run timed out.
    fn wait_with_drain(
        child: &mut std::process::Child,
        timeout: Duration,
    ) -> Result<(Vec<u8>, std::process::ExitStatus), String> {
        use std::io::Read;
        use std::time::Instant;

        let stdout = child.stdout.take();
        let drain = stdout.map(|mut out| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = out.read_to_end(&mut buf);
                buf
            })
        });
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // Reap, then collect the drained output: the reader
                    // thread sees EOF once the child (and any stdout
                    // inheritor it waited on) is gone.
                    let _ = child.wait();
                    let buf = drain
                        .map(|h| h.join().unwrap_or_default())
                        .unwrap_or_default();
                    return Ok((buf, status));
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        // Detach the drain thread (see the fn docs).
                        drop(drain);
                        return Err(format!("replay timed out after {timeout:?}"));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(format!("wait on replay child: {e}")),
            }
        }
    }

    /// A [`ReplayRunner`] that executes the task's own [`TaskExec`] spec
    /// the built-in headless runner. No external agent needed: the task
    /// defines exactly how it runs, so a held-out task is a deterministic
    /// command whose stdout is the scored transcript.
    ///
    /// Isolation: each run gets a fresh empty working directory and a
    /// deadline; the child inherits the process environment plus the
    /// task's `env` overrides. When a proposal is under test,
    /// `PANTHEON_REPLAY_SKILL_DIR` points at the staged overlay
    /// ([`stage_proposal_overlay`]). This is process isolation, not a
    /// sandbox - a task spec is trusted code. For untrusted or
    /// agent-driven replays, point `[nightly] replay_command` at a
    /// sandboxed headless agent instead.
    pub struct TaskSpecReplayRunner {
        pub timeout: Duration,
    }

    impl TaskSpecReplayRunner {
        pub fn new(timeout: Duration) -> Self {
            Self { timeout }
        }
    }

    impl ReplayRunner for TaskSpecReplayRunner {
        fn run_transcript(
            &self,
            task: &ReplayTask,
            with_proposal: Option<&Proposal>,
        ) -> Result<String, String> {
            use std::process::{Command, Stdio};

            let spec = task.exec.as_ref().ok_or_else(|| {
                format!(
                    "replay task '{}' has no exec spec - give it one (`replay-tasks add --exec ...`) or configure [nightly] replay_command",
                    task.id
                )
            })?;
            if spec.command.trim().is_empty() {
                return Err(format!("replay task '{}': exec command is empty", task.id));
            }
            // Fresh empty working directory per run: the task starts with a
            // clean slate, but this is NOT a sandbox - it inherits our
            // environment and can still reach the filesystem. Task specs
            // are trusted code (see the struct docs).
            let workdir = std::env::temp_dir().join(format!(
                "pantheon-replay-run-{}-{}",
                sanitize_id(&task.id),
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&workdir);
            std::fs::create_dir_all(&workdir).map_err(|e| format!("create replay workdir: {e}"))?;

            let mut cmd = Command::new(&spec.command);
            cmd.args(&spec.args)
                .arg(&task.prompt)
                .current_dir(&workdir)
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }
            let mut staged: Option<std::path::PathBuf> = None;
            if let Some(p) = with_proposal {
                let dir = stage_proposal_overlay(p)?;
                cmd.env("PANTHEON_REPLAY_SKILL_DIR", &dir);
                staged = Some(dir);
            }
            let mut child = cmd
                .spawn()
                .map_err(|e| format!("spawn replay task '{}': {e}", task.id))?;
            let result = wait_with_drain(&mut child, self.timeout).and_then(|(out, status)| {
                if !status.success() {
                    return Err(format!(
                        "replay task '{}' exited {}",
                        task.id,
                        status.code().unwrap_or(-1)
                    ));
                }
                String::from_utf8(out)
                    .map_err(|e| format!("replay task '{}' output not UTF-8: {e}", task.id))
            });
            let _ = std::fs::remove_dir_all(&workdir);
            if let Some(dir) = staged {
                let _ = std::fs::remove_dir_all(&dir);
            }
            result
        }
    }

    /// Keep temp-dir names filesystem-safe: task ids are validated
    /// non-empty but may contain anything else.
    fn sanitize_id(id: &str) -> String {
        let clean: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let clean = clean.trim_matches('_').to_string();
        if clean.is_empty() {
            "task".to_string()
        } else {
            clean
        }
    }

    /// The runner hosts should use: picks the right strategy per task.
    ///
    /// 1. Task has an [`TaskExec`] spec → [`TaskSpecReplayRunner`]
    ///    (built-in, deterministic, no configuration needed).
    /// 2. Else `[nightly] replay_command` is set →
    ///    [`SubprocessReplayRunner`] (headless agent).
    /// 3. Else → [`UnconfiguredReplayRunner`] (loud failure, the gate
    ///    rejects - "couldn't measure" is never a pass).
    pub struct CompositeReplayRunner {
        /// External headless-agent command (`<command> <prompt>`), if any.
        pub command: Option<String>,
        /// Deadline per replay, for both strategies.
        pub timeout: Duration,
    }

    impl CompositeReplayRunner {
        pub fn new(command: Option<String>, timeout: Duration) -> Self {
            Self { command, timeout }
        }
    }

    impl ReplayRunner for CompositeReplayRunner {
        fn run_transcript(
            &self,
            task: &ReplayTask,
            with_proposal: Option<&Proposal>,
        ) -> Result<String, String> {
            if task.exec.is_some() {
                return TaskSpecReplayRunner::new(self.timeout).run_transcript(task, with_proposal);
            }
            match &self.command {
                Some(cmd) => SubprocessReplayRunner::new(cmd.clone(), self.timeout)
                    .run_transcript(task, with_proposal),
                None => UnconfiguredReplayRunner.run_transcript(task, with_proposal),
            }
        }
    }

    // Small deterministic invariant tests only.

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::propose::{ProposalKind, ProposalStatus};

        fn skill_proposal(id: &str, name: &str) -> Proposal {
            Proposal {
                id: id.to_string(),
                kind: ProposalKind::Skill {
                    name: name.to_string(),
                    update: false,
                },
                title: "probe".to_string(),
                body: "# probe skill".to_string(),
                provenance_runs: Vec::new(),
                provenance_turns: Vec::new(),
                eval_tags: Vec::new(),
                status: ProposalStatus::Proposed,
            }
        }

        /// A hostile proposal id / skill name must not escape the staging
        /// dir: both are sanitized before they touch a path.
        #[test]
        fn staging_sanitizes_proposal_id_and_skill_name() {
            let dir = stage_proposal_overlay(&skill_proposal("../../evil", "../escape")).unwrap();
            assert!(
                dir.starts_with(std::env::temp_dir()),
                "staging dir must stay under temp: {}",
                dir.display()
            );
            let staged = dir.join("escape.md");
            assert!(
                staged.is_file(),
                "sanitized skill file lands inside the dir"
            );
            assert_eq!(staged.parent().unwrap(), dir);
            assert!(
                !std::env::temp_dir().join("escape.md").exists()
                    || std::env::temp_dir().join("escape.md") == staged,
                "no file escaped to the temp root"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Two passes staging the same proposal id get different dirs.
        #[test]
        fn staging_is_unique_per_pass() {
            let p = skill_proposal("same-id", "same-skill");
            let a = stage_proposal_overlay(&p).unwrap();
            let b = stage_proposal_overlay(&p).unwrap();
            assert_ne!(a, b);
            assert!(a.is_dir() && b.is_dir());
            let _ = std::fs::remove_dir_all(&a);
            let _ = std::fs::remove_dir_all(&b);
        }

        /// A verbose child (> pipe buffer) must be drained, not misreported
        /// as timed out: without the concurrent drain the child blocks on
        /// the full pipe while the parent polls `try_wait`.
        #[test]
        fn verbose_child_output_is_drained_not_timed_out() {
            let task = ReplayTask {
                id: "verbose".to_string(),
                name: "verbose".to_string(),
                prompt: "x".to_string(),
                check: ReplayCheck::Contains {
                    text: "30000".to_string(),
                    negate: false,
                },
                exec: Some(TaskExec {
                    command: "sh".to_string(),
                    args: vec!["-c".to_string(), "seq 1 30000".to_string()],
                    env: std::collections::HashMap::new(),
                }),
            };
            let out = TaskSpecReplayRunner::new(Duration::from_secs(30))
                .run_transcript(&task, None)
                .expect("verbose child must complete, not time out");
            assert!(out.len() > 65_536, "output must exceed the pipe buffer");
            assert!(out.contains("30000"));
        }

        /// The staging dir is removed when the run ends, so a pass leaves
        /// nothing behind for the next pass to sweep.
        #[test]
        fn staging_dir_removed_after_run() {
            let task = ReplayTask {
                id: "cleanup".to_string(),
                name: "cleanup".to_string(),
                prompt: "x".to_string(),
                check: ReplayCheck::Contains {
                    text: "hi".to_string(),
                    negate: false,
                },
                exec: Some(TaskExec {
                    command: "sh".to_string(),
                    args: vec!["-c".to_string(), "echo hi".to_string()],
                    env: std::collections::HashMap::new(),
                }),
            };
            TaskSpecReplayRunner::new(Duration::from_secs(30))
                .run_transcript(
                    &task,
                    Some(&skill_proposal("cleanup-probe-replay", "probe")),
                )
                .expect("run succeeds");
            let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .contains("cleanup-probe-replay")
                })
                .collect();
            assert!(leftovers.is_empty(), "staging leftovers: {leftovers:?}");
        }
    }
}

pub mod gate {
    //! Eval-gating: the no-regression check.
    //!
    //! Skill and persona proposals must pass their relevant `pantheon-eval`
    //! targets before they can be approved. Bounded: at most `max_evals`
    //! targets, each with a timeout. A regression rejects the proposal and
    //! the rejection is audited.
    //!
    //! This is the *no-regression* half of validation. The *improvement*
    //! half - proving the proposal makes a held-out task better - lives in
    //! [`crate::replay`]. A proposal ships only when both pass.

    use crate::NightlyConfig;
    use std::time::{Duration, Instant};

    /// Outcome of one eval target run.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum EvalOutcome {
        Pass,
        Fail(String),
        Timeout,
        Error(String),
    }

    /// Runs eval targets. Implementations must be bounded (timeout).
    pub trait EvalRunner {
        fn run_eval(&self, target: &str, timeout: Duration) -> EvalOutcome;
    }

    /// Verdict over the whole gate.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum EvalVerdict {
        /// All tagged evals passed. Carries a human-readable summary for the
        /// audit log and the approval prompt.
        Pass(String),
        /// The gate ran no evals: the proposal had no eval tags. This is NOT
        /// a pass - an untested proposal must not look validated. The caller
        /// decides whether the proposal's kind is allowlisted to skip
        /// eval-gating; otherwise it escalates.
        Skipped(String),
        /// At least one eval failed/timed out/errored. The proposal is
        /// rejected; the reason names the failing target.
        Reject(String),
    }

    /// Real runner: `cargo test -p pantheon-eval --test <target>`.
    /// Each target runs in its own process with a deadline; an expired
    /// deadline kills the child.
    pub struct SubprocessEvalRunner {
        pub cargo: String,
        /// Default per-target timeout, used when callers don't pass one.
        pub eval_timeout: Duration,
        /// Default cap on targets per gate decision.
        pub max_evals: usize,
    }

    impl SubprocessEvalRunner {
        /// Build a runner with the timeout/cap from [`NightlyConfig`].
        pub fn new(eval_timeout: Duration, max_evals: usize) -> Self {
            Self {
                cargo: "cargo".to_string(),
                eval_timeout,
                max_evals,
            }
        }
    }

    impl Default for SubprocessEvalRunner {
        fn default() -> Self {
            Self::new(Duration::from_secs(120), 3)
        }
    }

    impl EvalRunner for SubprocessEvalRunner {
        fn run_eval(&self, target: &str, timeout: Duration) -> EvalOutcome {
            let mut child = match std::process::Command::new(&self.cargo)
                .args(["test", "-p", "pantheon-eval", "--test", target])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => return EvalOutcome::Error(format!("spawn cargo: {e}")),
            };
            let deadline = Instant::now() + timeout;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        return if status.success() {
                            EvalOutcome::Pass
                        } else {
                            EvalOutcome::Fail(format!(
                                "cargo test -p pantheon-eval --test {target} exited {}",
                                status.code().unwrap_or(-1)
                            ))
                        };
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            return EvalOutcome::Timeout;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => return EvalOutcome::Error(format!("wait on cargo: {e}")),
                }
            }
        }
    }

    /// Gate a proposal: run its tagged evals. All must pass; the first
    /// failure rejects the proposal.
    ///
    /// The required set is exact. Duplicate tags collapse (running the same
    /// target twice proves nothing), but a set larger than `config.max_evals`
    /// is **refused**, never truncated: silently dropping a required eval
    /// would validate the proposal against a partial bar. `max_evals` is a
    /// bound on what the gate will run, not permission to run less than the
    /// proposal claims.
    ///
    /// An empty tag list yields [`EvalVerdict::Skipped`], never a vacuous
    /// `Pass`: a proposal that ran zero evals is not a proposal that passed
    /// its evals. The caller decides whether the proposal's kind is
    /// allowlisted to skip eval-gating.
    pub fn gate(
        proposal: &crate::Proposal,
        tags: &[&str],
        runner: &dyn EvalRunner,
        config: &NightlyConfig,
    ) -> EvalVerdict {
        let mut required: Vec<&str> = Vec::with_capacity(tags.len());
        for tag in tags {
            if !required.contains(tag) {
                required.push(*tag);
            }
        }
        if required.is_empty() {
            // No evals ran: report that explicitly. Callers must treat
            // `Skipped` as *not validated* unless the proposal kind is
            // allowlisted to skip eval-gating - a vacuous pass here let
            // broken drafts validate green.
            return EvalVerdict::Skipped("no evals tagged".into());
        }
        if required.len() > config.max_evals {
            return EvalVerdict::Reject(format!(
                "proposal '{}' requires {} evals but the gate cap is {}; refusing to validate a partial required set",
                proposal.id,
                required.len(),
                config.max_evals
            ));
        }
        let tags = required;
        let mut passed = Vec::new();
        for tag in tags {
            match runner.run_eval(tag, config.eval_timeout) {
                EvalOutcome::Pass => passed.push(tag.to_string()),
                EvalOutcome::Fail(detail) => {
                    return EvalVerdict::Reject(format!(
                        "proposal '{}' rejected: eval '{tag}' failed: {detail}",
                        proposal.id
                    ));
                }
                EvalOutcome::Timeout => {
                    return EvalVerdict::Reject(format!(
                        "proposal '{}' rejected: eval '{tag}' timed out after {:?}",
                        proposal.id, config.eval_timeout
                    ));
                }
                EvalOutcome::Error(detail) => {
                    return EvalVerdict::Reject(format!(
                        "proposal '{}' rejected: eval '{tag}' errored: {detail}",
                        proposal.id
                    ));
                }
            }
        }
        EvalVerdict::Pass(format!("evals passed: {}", passed.join(", ")))
    }

    // Small deterministic invariant tests only.
}
