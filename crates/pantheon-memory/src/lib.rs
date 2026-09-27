//! Memory plane (spec section 11): five layers + runtime state.
//!
//! GLOBAL -> AGENT -> PROJECT -> TASK/SESSION -> EPHEMERAL TURN, plus
//! RUNTIME STATE (operational, not remembered).
//!
//! Writes are capability-gated and never silent:
//!   propose -> policy -> provenance -> validation -> provider
//! A webpage telling the agent "remember this password" cannot reach the
//! store without passing the same five steps as anything else.
use pantheon_api::capability::{Capability, Decision, Policy};
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};

pub mod backend;
pub mod http_backend;
pub mod markdown;
pub mod plugins;
pub mod store;
pub use backend::{
    load_selection, open_selected, save_selection, selection_path, BackendInfo, BackendRegistry,
    BackendSelection,
};
pub use plugins::{load_dir as load_memory_plugins, MemoryPluginManifest, StdioBackend};
pub use store::{damage_fts_for_test, fts_health, rebuild_fts, MemoryStore, Recalled};

/// Backend boundary for external memory providers such as GalaxyMem,
/// Mnemosyne, Honcho, Hindsight, OpenViking. Providers implement recall
/// and writes; policy and provenance stay at this boundary instead of
/// being delegated blindly to a plugin.
///
/// Policy enforcement rule: callers must gate through the `*_via` helpers
/// in this crate (`recall_via` / `write_via` / `confirm_via`). Those check
/// capability + validation + trust clamp BEFORE the backend sees anything,
/// so a backend can never be the thing that decides to ignore policy.
/// The trait methods themselves take the policy for native-style backends
/// that want to re-check; external adapters may ignore it.
pub trait MemoryBackend: Send + Sync + std::fmt::Debug {
    /// Recall scoped to `namespaces`.
    ///
    /// The namespace list is part of the contract, not a convenience:
    /// a backend that cannot filter by namespace must refuse rather than
    /// return every agent's memories.
    fn recall(
        &self,
        policy: &Policy,
        namespaces: &[&str],
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
    /// Fetch one record by (namespace, key). Default: unsupported —
    /// external services are query-oriented, not key-get oriented.
    fn get(&self, namespace: &str, key: &str) -> Result<Option<MemoryRecord>, PantheonError> {
        let _ = (namespace, key);
        Err(unsupported("get"))
    }
    /// Remove one record. Default: unsupported (external services manage
    /// deletion through their own surface).
    fn forget(&self, layer: LayerKind, namespace: &str, key: &str) -> Result<bool, PantheonError> {
        let _ = (layer, namespace, key);
        Err(unsupported("forget"))
    }
    /// Promote a record to Memory tier (the human-vouch path). Default:
    /// unsupported — trust tiers live in Pantheon's provenance model and
    /// not every remote service can represent them.
    fn confirm(
        &self,
        policy: &Policy,
        namespace: &str,
        key: &str,
    ) -> Result<MemoryRecord, PantheonError> {
        let _ = (policy, namespace, key);
        Err(unsupported("confirm"))
    }
}

fn unsupported(op: &str) -> PantheonError {
    merr(
        "MEM_BACKEND_UNSUPPORTED",
        format!("backend does not implement `{op}`"),
        "use a backend that supports this operation, or the native store",
    )
}

impl MemoryBackend for MemoryStore {
    fn recall(
        &self,
        policy: &Policy,
        namespaces: &[&str],
        layers: &[LayerKind],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Recalled>, PantheonError> {
        recall(self, policy, namespaces, layers, query, limit)
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

    fn get(&self, namespace: &str, key: &str) -> Result<Option<MemoryRecord>, PantheonError> {
        MemoryStore::get(self, namespace, key)
    }

    fn forget(&self, layer: LayerKind, namespace: &str, key: &str) -> Result<bool, PantheonError> {
        MemoryStore::forget(self, layer, namespace, key)
    }

    fn confirm(
        &self,
        policy: &Policy,
        namespace: &str,
        key: &str,
    ) -> Result<MemoryRecord, PantheonError> {
        confirm_write(self, policy, namespace, key)
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
/// pantheon-api; the invariant `trust <= source tier` is enforced in
/// `propose_write`: memory can never raise the trust of its material.
/// Only an explicit user action (CLI `memory put`, `memory_confirm` on
/// an existing record) may store or promote a higher tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    /// Where the value came from: `user`, `tool:web_fetch`, `plugin:time-gap`...
    pub origin: String,
    pub trust: pantheon_api::provenance::TrustTier,
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
    let gated = gate_proposal(policy, proposal, max_bytes)?;
    // provider.
    store.put(&gated)
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
    store.promote(namespace, key, pantheon_api::provenance::TrustTier::Memory)
}

/// Recall for one agent, across layers, narrowest first.
///
/// `namespace` is the caller's own namespace. Passing an empty slice
/// recalls nothing: a caller that has not established who it is reading as
/// must not get a global view. Cross-agent recall is a deliberate act
/// (pass several namespaces), never a side effect of forgetting to scope.
pub fn recall(
    store: &MemoryStore,
    policy: &Policy,
    namespaces: &[&str],
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
    store.search_scoped(namespaces, layers, query, limit)
}

/// Gated recall against ANY backend. Checks `memory.read` here, before the
/// backend sees the query — external backends must never be the party that
/// decides whether policy allows a read.
pub fn recall_via(
    backend: &dyn MemoryBackend,
    policy: &Policy,
    namespaces: &[&str],
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
    backend.recall(policy, namespaces, layers, query, limit)
}

/// Full write path against ANY backend: policy -> validation -> trust
/// clamp happen HERE (before the proposal crosses the boundary), then the
/// backend stores the already-gated proposal. `propose_write` on the
/// native store re-checks idempotently; external adapters can rely on the
/// gate having run.
pub fn write_via(
    backend: &dyn MemoryBackend,
    policy: &Policy,
    proposal: Proposal,
    max_bytes: usize,
) -> Result<MemoryRecord, PantheonError> {
    let gated = gate_proposal(policy, proposal, max_bytes)?;
    backend.write(policy, gated, max_bytes)
}

/// Gated confirm against ANY backend: policy check first, then the
/// backend's promotion path (native implements it; external backends
/// default to `MEM_BACKEND_UNSUPPORTED`).
pub fn confirm_via(
    backend: &dyn MemoryBackend,
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
    backend.confirm(policy, namespace, key)
}

/// Shared gate for the write path: capability, validation, trust clamp.
/// Returns the gated proposal ready for any provider.
fn gate_proposal(
    policy: &Policy,
    proposal: Proposal,
    max_bytes: usize,
) -> Result<Proposal, PantheonError> {
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
    // lands at Untrusted regardless of the requested tier.
    let mut p = proposal;
    if !matches!(p.provenance.origin.as_str(), "user" | "cli" | "import") {
        p.provenance.trust = pantheon_api::provenance::TrustTier::Untrusted;
    }
    Ok(p)
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
