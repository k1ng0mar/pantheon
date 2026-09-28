//! Applying proposals.
//!
//! - `MemoryLesson` → the memory store, `Agent` layer, `reflect` namespace,
//!   at the `Memory` trust tier. Informative, never authoritative, and the
//!   store's trust ceiling means it can never clobber a user-confirmed
//!   record.
//! - `Skill` → a `SKILL.md` under `<data_dir>/skills/<name>/`. Only ever
//!   called after eval-gating AND explicit approval.
//! - `Persona` → agent-layer memory under the `persona` namespace: it
//!   shapes future sessions through recall without editing the user's
//!   config files. Only ever called after eval-gating AND approval.

use crate::{now_ms, PendingProposal, Proposal, ProposalKind};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_memory::{
    LayerKind, MemoryStore, Proposal as MemProposal, Provenance as MemProvenance,
};
use std::path::Path;

/// What applying one proposal did, for the audit log and CLI output.
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub proposal_id: String,
    pub detail: String,
}

impl ApplyOutcome {
    pub fn describe(&self) -> String {
        self.detail.clone()
    }
}

fn err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Runtime,
        false,
        cause,
        "check the data dir is writable (`pantheon doctor`)",
        String::new(),
    )
}

fn mem_provenance() -> MemProvenance {
    MemProvenance {
        source: "reflect".to_string(),
        origin: "reflection-pass".to_string(),
        trust: pantheon_api::provenance::TrustTier::Memory,
        recorded_at_ms: now_ms(),
    }
}

/// Apply one proposal. Callers must have already cleared the gates:
/// memory lessons auto-apply; skills/personas require eval-pass +
/// explicit approval (see [`crate::approve_pending`]).
pub fn apply_proposal(data_dir: &Path, proposal: &Proposal) -> Result<ApplyOutcome, PantheonError> {
    match &proposal.kind {
        ProposalKind::MemoryLesson { key } => {
            let store = MemoryStore::open(&data_dir.join("memory.db"))
                .map_err(|e| err("RFL_MEMORY", format!("open memory store: {e}")))?;
            store
                .put(&MemProposal {
                    layer: LayerKind::Agent,
                    namespace: "reflect".to_string(),
                    key: key.clone(),
                    value: proposal.body.clone(),
                    provenance: mem_provenance(),
                })
                .map_err(|e| err("RFL_MEMORY", format!("store lesson: {e}")))?;
            Ok(ApplyOutcome {
                proposal_id: proposal.id.clone(),
                detail: format!("lesson stored as reflect/{key}"),
            })
        }
        ProposalKind::Skill { name, update } => {
            // Defense in depth: the name is slug-shaped by construction
            // (`propose::slug`), but never let a path escape the skills
            // dir even if a proposal is built by hand.
            if !skill_name_ok(name) {
                return Err(err("RFL_SKILL", format!("unsafe skill name '{name}'")));
            }
            let dir = data_dir.join("skills").join(name);
            std::fs::create_dir_all(&dir)
                .map_err(|e| err("RFL_SKILL", format!("create {}: {e}", dir.display())))?;
            let path = dir.join("SKILL.md");
            // Never silently overwrite a hand-written skill: an update
            // appends a dated reflection note instead.
            if *update && path.exists() {
                let mut existing = std::fs::read_to_string(&path)
                    .map_err(|e| err("RFL_SKILL", format!("read {}: {e}", path.display())))?;
                existing.push_str(&format!("\n\n## Reflection note\n\n{}\n", proposal.body));
                atomic_write(&path, &existing)
                    .map_err(|e| err("RFL_SKILL", format!("write {}: {e}", path.display())))?;
                return Ok(ApplyOutcome {
                    proposal_id: proposal.id.clone(),
                    detail: format!("skill '{name}' updated (note appended)"),
                });
            }
            atomic_write(&path, &proposal.body)
                .map_err(|e| err("RFL_SKILL", format!("write {}: {e}", path.display())))?;
            Ok(ApplyOutcome {
                proposal_id: proposal.id.clone(),
                detail: format!("skill '{name}' written"),
            })
        }
        ProposalKind::Persona { topic } => {
            let store = MemoryStore::open(&data_dir.join("memory.db"))
                .map_err(|e| err("RFL_MEMORY", format!("open memory store: {e}")))?;
            let key = format!("persona-{:08x}", {
                let mut h: u64 = 0xcbf29ce484222325;
                for b in topic.bytes() {
                    h ^= b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
                h & 0xffff_ffff
            });
            store
                .put(&MemProposal {
                    layer: LayerKind::Agent,
                    namespace: "persona".to_string(),
                    key: key.clone(),
                    value: proposal.body.clone(),
                    provenance: mem_provenance(),
                })
                .map_err(|e| err("RFL_MEMORY", format!("store persona note: {e}")))?;
            Ok(ApplyOutcome {
                proposal_id: proposal.id.clone(),
                detail: format!("persona note stored as persona/{key}"),
            })
        }
    }
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

/// Write a file atomically: temp file + rename, so a crashed reflection
/// pass never leaves a half-written SKILL.md behind.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Pending-approval persistence: `<data_dir>/reflect-pending.json`.
/// Written at the end of every pass; `--approve`/`--deny` and the TUI
/// consume it.
pub fn pending_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("reflect-pending.json")
}

pub fn save_pending(data_dir: &Path, pending: &[PendingProposal]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(pending).map_err(|e| format!("encode pending: {e}"))?;
    std::fs::write(pending_path(data_dir), text).map_err(|e| format!("write pending: {e}"))?;
    Ok(())
}

pub fn load_pending(data_dir: &Path) -> Result<Vec<PendingProposal>, String> {
    let path = pending_path(data_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read pending: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("decode pending: {e}"))
}

// Small deterministic invariant tests only.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::propose::ProposalStatus;

    fn lesson_proposal() -> Proposal {
        Proposal {
            id: "rfl_lesson_1".into(),
            kind: ProposalKind::MemoryLesson { key: "k".into() },
            title: "t".into(),
            body: "b".into(),
            provenance_runs: vec![],
            provenance_turns: vec![],
            eval_tags: vec![],
            status: ProposalStatus::Proposed,
        }
    }

    #[test]
    fn outcome_describes() {
        let o = ApplyOutcome {
            proposal_id: "x".into(),
            detail: "did it".into(),
        };
        assert_eq!(o.describe(), "did it");
        assert_eq!(o.proposal_id, "x");
        let _ = lesson_proposal();
    }

    #[test]
    fn pending_path_is_stable() {
        let p = pending_path(Path::new("/d"));
        assert_eq!(p, Path::new("/d/reflect-pending.json"));
    }
}
