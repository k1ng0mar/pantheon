//! Memory plane (spec section 11): five layers + runtime state.
//!
//! GLOBAL -> AGENT -> PROJECT -> TASK/SESSION -> EPHEMERAL TURN, plus
//! RUNTIME STATE (operational, not remembered).
//!
//! Writes are capability-gated and never silent:
//!   propose -> policy -> provenance -> validation -> provider
//! A webpage telling the agent "remember this password" cannot reach the
//! store without passing the same five steps as anything else.
use pantheon_core::capability::{Capability, Decision, Policy};
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};

pub mod backend;
pub mod http_backend;
pub mod markdown;
pub mod store;
pub use backend::{BackendInfo, BackendRegistry, BackendSelection};
pub use store::{MemoryStore, Recalled};

/// Backend boundary for external memory providers such as GalaxyMem,
/// Mnemosyne, Honcho, or Hindsight. Providers implement recall and writes;
/// policy and provenance stay at this boundary instead of being delegated
/// blindly to a plugin.
pub trait MemoryBackend: Send + Sync + std::fmt::Debug {
    fn recall(
        &self,
        policy: &Policy,
        layers: &[LayerKind],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Recalled>, PantheonError>;
    fn write(
        &self,
        policy: &Policy,
        proposal: Proposal,
        max_bytes: usize,
    ) -> Result<MemoryRecord, PantheonError>;
    fn list_agent(&self, namespace: &str) -> Result<Vec<(String, String)>, PantheonError>;
}

impl MemoryBackend for MemoryStore {
    fn recall(
        &self,
        policy: &Policy,
        layers: &[LayerKind],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Recalled>, PantheonError> {
        recall(self, policy, layers, query, limit)
    }

    fn write(
        &self,
        policy: &Policy,
        proposal: Proposal,
        max_bytes: usize,
    ) -> Result<MemoryRecord, PantheonError> {
        propose_write(self, policy, proposal, max_bytes)
    }

    fn list_agent(&self, namespace: &str) -> Result<Vec<(String, String)>, PantheonError> {
        self.list_agent(namespace)
    }
}

fn merr(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(code, Layer::Memory, false, cause, remediation, "")
}

/// Where a memory lives. Narrower layers are recalled before wider ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LayerKind {
    /// Persists across everything. Widest scope, hardest to write.
    Global,
    /// One agent's personal memory (Hermes MEMORY.md analogue).
    Agent,
    /// Project-scoped.
    Project,
    /// One task/session.
    TaskSession,
    /// One turn. Never persisted by the store; lives in the transcript.
    EphemeralTurn,
}

/// Why a write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRefusal {
    MissingCapability,
    NoProvenance,
    Empty,
    TooLarge { bytes: usize, max: usize },
    EphemeralNotPersisted,
}

impl WriteRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            WriteRefusal::MissingCapability => "MEM_NO_CAPABILITY",
            WriteRefusal::NoProvenance => "MEM_NO_PROVENANCE",
            WriteRefusal::Empty => "MEM_EMPTY",
            WriteRefusal::TooLarge { .. } => "MEM_TOO_LARGE",
            WriteRefusal::EphemeralNotPersisted => "MEM_EPHEMERAL",
        }
    }
}

/// Who is asking, and on whose behalf. `trust` carries the tier from
/// pantheon-core; the invariant `trust <= source tier` is enforced in
/// `propose_write`: memory can never raise the trust of its material.
/// Only an explicit user action (CLI `memory put`, `memory_confirm` on
/// an existing record) may store or promote a higher tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    /// Where the value came from: `user`, `tool:web_fetch`, `plugin:time-gap`...
    pub origin: String,
    pub trust: pantheon_core::provenance::TrustTier,
    pub recorded_at_ms: i64,
}

/// A write request. Nothing touches the store until this passes all gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub layer: LayerKind,
    /// Namespace inside the layer: agent name, project id, session id.
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub provenance: Provenance,
}

/// Validated, stored memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub layer: LayerKind,
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub provenance: Provenance,
}

/// Validation rules applied before the provider sees anything.
pub fn validate(p: &Proposal, max_bytes: usize) -> Result<(), WriteRefusal> {
    if p.layer == LayerKind::EphemeralTurn {
        // Turn-scoped content belongs in the transcript, not the store.
        return Err(WriteRefusal::EphemeralNotPersisted);
    }
    if p.key.trim().is_empty() || p.value.trim().is_empty() {
        return Err(WriteRefusal::Empty);
    }
    if p.value.len() > max_bytes {
        return Err(WriteRefusal::TooLarge {
            bytes: p.value.len(),
            max: max_bytes,
        });
    }
    if p.provenance.origin.trim().is_empty() {
        return Err(WriteRefusal::NoProvenance);
    }
    Ok(())
}

/// The full write path. Returns the record that was stored.
///
/// Trust invariant: a proposal may not claim a tier above what its origin
/// justifies. Model-sourced proposals (`origin` not `user`/`cli`/`import`)
/// are clamped to Untrusted no matter what the caller asked for: the model
/// cannot launder web content into trusted memory by passing a flattering
/// origin tag. Explicit user actions (CLI put, file import) write User
/// tier directly.
pub fn propose_write(
    store: &MemoryStore,
    policy: &Policy,
    proposal: Proposal,
    max_bytes: usize,
) -> Result<MemoryRecord, PantheonError> {
    // 1. policy — memory.write must be granted explicitly.
    if !matches!(policy.check(&Capability::MemoryWrite), Decision::Allow) {
        let r = WriteRefusal::MissingCapability;
        return Err(merr(
            r.code(),
            format!(
                "memory.write not granted for origin {}",
                proposal.provenance.origin
            ),
            "grant memory.write in the agent policy",
        ));
    }
    // 2. validation (provenance is checked here too).
    if let Err(r) = validate(&proposal, max_bytes) {
        return Err(merr(
            r.code(),
            format!("proposal failed validation: {r:?}"),
            "fix the proposal; nothing was stored",
        ));
    }
    // 3. trust clamp. Anything not authored by an explicit user action
    // lands at Untrusted regardless of the requested tier. The user
    // promotion path is `memory_confirm` / CLI, which writes Memory tier
    // directly below.
    let mut p = proposal;
    if !matches!(p.provenance.origin.as_str(), "user" | "cli" | "import") {
        p.provenance.trust = pantheon_core::provenance::TrustTier::Untrusted;
    }
    // 4. provider.
    store.put(&p)
}

/// Promote an existing record to Memory tier (from Untrusted). This is
/// the explicit user-approval path: the CLI and the `memory_confirm` tool
/// call this after a human says the record is sound. Returns the updated
/// record. No-op (still succeeds) if the record is already Memory tier or
/// better.
pub fn confirm_write(
    store: &MemoryStore,
    policy: &Policy,
    namespace: &str,
    key: &str,
) -> Result<MemoryRecord, PantheonError> {
    if !matches!(policy.check(&Capability::MemoryWrite), Decision::Allow) {
        return Err(merr(
            "MEM_NO_CAPABILITY",
            "memory.write not granted for confirm".into(),
            "grant memory.write in the agent policy",
        ));
    }
    store.promote(namespace, key, pantheon_core::provenance::TrustTier::Memory)
}

/// Recall across layers, narrowest first, with provenance attached.
pub fn recall(
    store: &MemoryStore,
    policy: &Policy,
    layers: &[LayerKind],
    query: &str,
    limit: usize,
) -> Result<Vec<Recalled>, PantheonError> {
    if !matches!(policy.check(&Capability::MemoryRead), Decision::Allow) {
        return Err(merr(
            "MEM_NO_READ_CAPABILITY",
            "memory.read not granted".into(),
            "grant memory.read in the agent policy",
        ));
    }
    store.search(layers, query, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov(origin: &str) -> Provenance {
        Provenance {
            source: "import".into(),
            origin: origin.into(),
            trust: pantheon_core::provenance::TrustTier::User,
            recorded_at_ms: 1,
        }
    }

    /// `coder()` grants `MemoryRead` but deliberately not `MemoryWrite`
    /// (see core capability presets); tests that must reach the store (or
    /// the validation gates) grant it explicitly.
    fn writer_policy() -> Policy {
        Policy::coder().allow(Capability::MemoryWrite)
    }

    fn proposal(layer: LayerKind, ns: &str, k: &str, v: &str, origin: &str) -> Proposal {
        Proposal {
            layer,
            namespace: ns.into(),
            key: k.into(),
            value: v.into(),
            provenance: prov(origin),
        }
    }

    #[test]
    fn write_without_capability_is_refused() {
        let store = MemoryStore::open_in_memory().unwrap();
        let policy = Policy::researcher_readonly();
        let p = proposal(LayerKind::Agent, "nyx", "city", "Kano", "user");
        let err = propose_write(&store, &policy, p, 4096).unwrap_err();
        assert_eq!(err.code, "MEM_NO_CAPABILITY");
    }

    #[test]
    fn write_without_provenance_is_refused() {
        let store = MemoryStore::open_in_memory().unwrap();
        let policy = writer_policy();
        let mut p = proposal(LayerKind::Agent, "nyx", "city", "Kano", "user");
        p.provenance.origin = "  ".into();
        let err = propose_write(&store, &policy, p, 4096).unwrap_err();
        assert_eq!(err.code, "MEM_NO_PROVENANCE");
    }

    #[test]
    fn ephemeral_turns_never_hit_the_store() {
        let store = MemoryStore::open_in_memory().unwrap();
        let p = proposal(LayerKind::EphemeralTurn, "s1", "scratch", "x", "user");
        let err = propose_write(&store, &writer_policy(), p, 4096).unwrap_err();
        assert_eq!(err.code, "MEM_EPHEMERAL");
    }

    #[test]
    fn recall_returns_provenance_and_narrowest_first() {
        let store = MemoryStore::open_in_memory().unwrap();
        let policy = writer_policy();
        propose_write(
            &store,
            &policy,
            proposal(LayerKind::Global, "g", "stack", "rust runtime", "user"),
            4096,
        )
        .unwrap();
        propose_write(
            &store,
            &policy,
            proposal(
                LayerKind::TaskSession,
                "s1",
                "fix",
                "rust parser bug",
                "tool:cargo",
            ),
            4096,
        )
        .unwrap();
        let layers = [
            LayerKind::TaskSession,
            LayerKind::Agent,
            LayerKind::Project,
            LayerKind::Global,
        ];
        let hits = recall(&store, &policy, &layers, "rust", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].record.layer, LayerKind::TaskSession);
        assert_eq!(hits[1].record.layer, LayerKind::Global);
        assert_eq!(hits[1].record.provenance.origin, "user");
        assert_eq!(hits[0].record.provenance.origin, "tool:cargo");
    }

    #[test]
    fn upsert_replaces_value_and_keeps_provenance_fresh() {
        let store = MemoryStore::open_in_memory().unwrap();
        let policy = writer_policy();
        propose_write(
            &store,
            &policy,
            proposal(LayerKind::Agent, "nyx", "tz", "UTC", "user"),
            4096,
        )
        .unwrap();
        let rec = propose_write(
            &store,
            &policy,
            proposal(LayerKind::Agent, "nyx", "tz", "Africa/Lagos", "tool:system"),
            4096,
        )
        .unwrap();
        assert_eq!(rec.value, "Africa/Lagos");
        assert_eq!(rec.provenance.origin, "tool:system");
        let hits = recall(&store, &policy, &[LayerKind::Agent], "tz", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
