//! Apply + pending approval.
//!
//! Propose-only: no unattended path writes a durable record.
//!
//! - **Memory lessons** are surfaced with their evidence (provenance
//!   runs + observed-count), then parked in
//!   `<data_dir>/nightly/nightly-pending.json` for a human to approve or
//!   deny. A grant writes the record at trust tier `Memory`; a denial
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
use pantheon_api::provenance::TrustTier;
use pantheon_memory::{
    LayerKind, MemoryStore, Proposal as MemProposal, Provenance as MemProvenance,
};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Load the pending queue (empty when the file is absent).
pub fn load_pending(data_dir: &Path) -> Result<Vec<Proposal>, String> {
    match std::fs::read_to_string(pending_path(data_dir)) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| format!("parse pending proposals: {e}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("read pending proposals: {e}")),
    }
}

fn save_pending(data_dir: &Path, pending: &[Proposal]) -> Result<(), String> {
    let path = pending_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(pending).map_err(|e| format!("encode pending: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Queue a skill/persona proposal for human approval. No-op (returns
/// false) when a proposal with the same id is already queued - the
/// operator sees one decision per proposal.
pub fn queue_for_approval(data_dir: &Path, proposal: &Proposal) -> Result<bool, String> {
    let mut pending = load_pending(data_dir)?;
    if pending.iter().any(|p| p.id == proposal.id) {
        return Ok(false);
    }
    let mut p = proposal.clone();
    p.status = ProposalStatus::ReplayPassed;
    pending.push(p);
    save_pending(data_dir, &pending)?;
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
    let mut pending = load_pending(data_dir)?;
    let pos = pending.iter().position(|p| p.id == id);
    let Some(pos) = pos else { return Ok(false) };
    let at = now_ms();
    if approve {
        // Apply BEFORE dequeuing: if application fails the proposal stays
        // in the queue for retry and the failure is audited.
        let proposal = pending[pos].clone();
        if let Err(e) = apply_approved(data_dir, &proposal) {
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
        let mut proposal = pending.remove(pos);
        proposal.status = ProposalStatus::Approved;
        save_pending(data_dir, &pending)?;
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
        let mut proposal = pending.remove(pos);
        proposal.status = ProposalStatus::Denied;
        save_pending(data_dir, &pending)?;
        crate::audit::audit(
            data_dir,
            &crate::audit::NightlyEvent::ApprovalDecided {
                id: proposal.id.clone(),
                approved: false,
                at_ms: at,
            },
        )?;
    }
    let _ = config;
    Ok(true)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
/// [`apply_memory_lesson`].
pub fn apply_skill_or_persona(data_dir: &Path, proposal: &Proposal) -> Result<(), String> {
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
                    provenance: mem_provenance("nightly", TrustTier::Memory),
                })
                .map_err(|e| format!("store persona note: {e}"))?;
            Ok(())
        }
        ProposalKind::MemoryLesson { .. } => {
            // Operator-approved a memory lesson: write it at trust tier
            // Memory now. This is the explicit user-vouch path - the
            // unattended `route` never reaches here.
            apply_memory_lesson(data_dir, proposal)
        }
    }
}

/// Apply a human-approved proposal by kind. The dispatcher [`decide`]
/// calls this after a grant: skill/persona proposals apply their payload,
/// memory lessons write the durable record at trust tier Memory. This is
/// the only path that turns a queued proposal into a durable effect -
/// `route` always queues, never applies.
pub fn apply_approved(data_dir: &Path, proposal: &Proposal) -> Result<(), String> {
    match &proposal.kind {
        ProposalKind::MemoryLesson { .. } => apply_memory_lesson(data_dir, proposal),
        ProposalKind::Skill { .. } | ProposalKind::Persona { .. } => {
            apply_skill_or_persona(data_dir, proposal)
        }
    }
}

/// Auto-apply a memory lesson at trust tier Memory. Idempotent: the key
/// is derived from the lesson text, so re-applying is a no-op write.
pub fn apply_memory_lesson(data_dir: &Path, proposal: &Proposal) -> Result<(), String> {
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
            provenance: mem_provenance("nightly", TrustTier::Memory),
        })
        .map_err(|e| format!("store lesson: {e}"))?;
    Ok(())
}

/// Route a validated proposal to its destination.
///
/// Memory lessons no longer auto-apply: an unattended nightly pass
/// surfaces a proposed lesson with its evidence (provenance runs +
/// observed-count) and parks it in the approval queue, exactly like
/// skill/persona proposals. The operator grants or denies; only a grant
/// writes the durable record. This is the trust/permission model -
/// frequency + recency can *propose* a lesson, they cannot *durable* one.
pub fn route(data_dir: &Path, proposal: &Proposal) -> Result<ApplyOutcome, String> {
    // All proposal kinds (memory lesson, skill, persona) queue for human
    // approval. `apply_memory_lesson` is still exposed for the explicit
    // operator path (`nightly approve <id>`), but the unattended route
    // never calls it directly.
    queue_for_approval(data_dir, proposal)?;
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
/// a half-written SKILL.md.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

// Small deterministic invariant tests only.
