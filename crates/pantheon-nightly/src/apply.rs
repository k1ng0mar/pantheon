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
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(queue).map_err(|e| format!("encode pending: {e}"))?;
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
    MemoryStore::open(&data_dir.join("memory.db")).map_err(|e| format!("open memory store: {e}"))
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
            std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
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
