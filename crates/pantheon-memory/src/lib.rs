//! Memory storage and the external backend boundary. Data layers from global and
//! agent scope down to project, session and turn state.
//!
//! Writes pass capability, provenance, validation and trust checks first.
use pantheon_api::capability::{Capability, Decision, Policy};
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};

pub mod plane;

pub mod backend {
    pub use crate::plane::backend::*;
}
pub mod http_backend {
    pub use crate::plane::http_backend::*;
}
pub mod markdown {
    pub use crate::plane::markdown::*;
}
pub mod plugins {
    pub use crate::plane::plugins::*;
}
pub(crate) use crate::plane::secret_scan;
pub mod store {
    pub use crate::plane::store::*;
}
pub use plane::backend::{
    load_selection, load_selection_result, open_selected, save_selection, selection_path,
    sync_selection_from_config, BackendInfo, BackendRegistry, BackendSelection,
};
pub use plane::plugins::{load_dir as load_memory_plugins, MemoryPluginManifest, StdioBackend};
pub use plane::store::{
    damage_fts_for_test, fts_health, rebuild_fts, LayerBudgets, MemoryConfig, MemoryStore, Recalled,
};

/// Boundary for external memory providers (GalaxyMem, Mnemosyne, Honcho,
/// Hindsight, OpenViking). Providers implement recall and writes; policy and
/// provenance stay here.
///
/// Callers gate through the `*_via` helpers (`recall_via` / `write_via` /
/// `confirm_via`), which check capability, validation and the trust clamp before
/// the backend sees anything. The trait methods take the policy for native
/// backends that want to re-check; external adapters may ignore it.
pub trait MemoryBackend: Send + Sync + std::fmt::Debug {
    /// Recall scoped to `namespaces`. A backend that can't filter by namespace has
    /// to refuse rather than return every agent's memories.
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
    /// Fetch one record by (layer, namespace, key) - the full row
    /// identity. Default: unsupported - external services are
    /// query-oriented, not key-get oriented.
    fn get(
        &self,
        layer: LayerKind,
        namespace: &str,
        key: &str,
    ) -> Result<Option<MemoryRecord>, PantheonError> {
        let _ = (layer, namespace, key);
        Err(unsupported("get"))
    }
    /// Remove one record. Default: unsupported (external services manage
    /// deletion through their own surface).
    fn forget(&self, layer: LayerKind, namespace: &str, key: &str) -> Result<bool, PantheonError> {
        let _ = (layer, namespace, key);
        Err(unsupported("forget"))
    }
    /// Promote a record to Memory tier (the human-vouch path). Default:
    /// unsupported - trust tiers live in Pantheon's provenance model and
    /// not every remote service can represent them.
    fn confirm(
        &self,
        policy: &Policy,
        layer: LayerKind,
        namespace: &str,
        key: &str,
    ) -> Result<MemoryRecord, PantheonError> {
        let _ = (policy, layer, namespace, key);
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

    fn get(
        &self,
        layer: LayerKind,
        namespace: &str,
        key: &str,
    ) -> Result<Option<MemoryRecord>, PantheonError> {
        MemoryStore::get(self, layer, namespace, key)
    }

    fn forget(&self, layer: LayerKind, namespace: &str, key: &str) -> Result<bool, PantheonError> {
        MemoryStore::forget(self, layer, namespace, key)
    }

    fn confirm(
        &self,
        policy: &Policy,
        layer: LayerKind,
        namespace: &str,
        key: &str,
    ) -> Result<MemoryRecord, PantheonError> {
        confirm_write(self, policy, layer, namespace, key)
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
    /// One agent's personal memory file.
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
    TooLarge {
        bytes: usize,
        max: usize,
    },
    EphemeralNotPersisted,
    /// The value trips the secret-pattern scan (`secret_scan`). `class`
    /// names the matched pattern class ("labeled password", "github token
    /// prefix") - never the secret itself.
    SecretDetected {
        class: &'static str,
    },
}

impl WriteRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            WriteRefusal::MissingCapability => "MEM_NO_CAPABILITY",
            WriteRefusal::NoProvenance => "MEM_NO_PROVENANCE",
            WriteRefusal::Empty => "MEM_EMPTY",
            WriteRefusal::TooLarge { .. } => "MEM_TOO_LARGE",
            WriteRefusal::EphemeralNotPersisted => "MEM_EPHEMERAL",
            WriteRefusal::SecretDetected { .. } => "MEM_SECRET",
        }
    }
}

/// Who is asking, and on whose behalf. The `trust <= source tier` invariant is
/// enforced in `propose_write`, so memory can't raise the trust of its material.
/// Only an explicit user action (CLI `memory put`, `memory_confirm`) may store or
/// promote a higher tier.
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
    // Secret tripwire: a page telling the agent "remember this password"
    // must not reach the store, the export, or recall. Fail closed - the
    // error names the pattern class, never the secret.
    if let Some(class) = secret_scan::detect_secret(&p.value) {
        return Err(WriteRefusal::SecretDetected { class });
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
/// justifies. Model-sourced proposals clamp to Untrusted whatever the caller
/// asked, so the model can't launder web content into trusted memory with a
/// flattering origin. Explicit user actions (CLI put, file import) write User.
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

/// Promote an existing record to Memory tier. The explicit user-approval path -
/// CLI and the `memory_confirm` tool call it after a human says the record is
/// sound. No-op if it's already Memory or better.
///
/// `layer` names which row: identity is (layer, namespace, key), and by
/// (namespace, key) alone this is ambiguous.
///
/// The check accepts `Allow` or `Approval`; `Deny` fails closed. The run loop
/// parks `Approval`-gated calls before the tool closure runs, so seeing
/// `Approval` here means the policy was already enforced or a human granted it.
pub fn confirm_write(
    store: &MemoryStore,
    policy: &Policy,
    layer: LayerKind,
    namespace: &str,
    key: &str,
) -> Result<MemoryRecord, PantheonError> {
    if !matches!(
        policy.check(&Capability::MemoryConfirm),
        Decision::Allow | Decision::Approval
    ) {
        return Err(merr(
            "MEM_NO_CAPABILITY",
            "memory.confirm not granted for confirm".into(),
            "grant memory.confirm in the agent policy",
        ));
    }
    store.promote(
        layer,
        namespace,
        key,
        pantheon_api::provenance::TrustTier::Memory,
    )
}

/// Recall for one agent across layers, narrowest first.
///
/// An empty slice recalls nothing - a caller that hasn't established who it reads
/// as doesn't get a global view. Cross-agent recall requires passing several
/// namespaces.
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

/// Gated recall against ANY backend. Checks `memory.read` here, before the backend
/// sees the query, so an external backend is never the party deciding whether policy
/// allows a read.
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

/// Full write path against any backend: policy, validation and the trust clamp
/// run here, before the proposal crosses. `propose_write` re-checks
/// idempotently; external adapters can rely on the gate having run.
pub fn write_via(
    backend: &dyn MemoryBackend,
    policy: &Policy,
    proposal: Proposal,
    max_bytes: usize,
) -> Result<MemoryRecord, PantheonError> {
    let gated = gate_proposal(policy, proposal, max_bytes)?;
    backend.write(policy, gated, max_bytes)
}

/// Gated confirm against any backend: policy first, then the backend's promotion
/// path (native implements it; external ones default to
/// `MEM_BACKEND_UNSUPPORTED`).
///
/// Accepts `Allow` or `Approval` - see [`confirm_write`]; the run loop parks
/// `Approval`-gated calls, so this only sees `Approval` after enforcement.
pub fn confirm_via(
    backend: &dyn MemoryBackend,
    policy: &Policy,
    layer: LayerKind,
    namespace: &str,
    key: &str,
) -> Result<MemoryRecord, PantheonError> {
    if !matches!(
        policy.check(&Capability::MemoryConfirm),
        Decision::Allow | Decision::Approval
    ) {
        return Err(merr(
            "MEM_NO_CAPABILITY",
            "memory.confirm not granted for confirm".into(),
            "grant memory.confirm in the agent policy",
        ));
    }
    backend.confirm(policy, layer, namespace, key)
}

/// Clamp a tier reported by a non-native backend to the operator's ceiling
/// (default Untrusted). A compromised server shouldn't be able to launder
/// `user`-tier records with injected instructions into the recall stream.
pub fn clamp_trust(
    tier: pantheon_api::provenance::TrustTier,
    ceiling: pantheon_api::provenance::TrustTier,
) -> pantheon_api::provenance::TrustTier {
    if tier.rank() > ceiling.rank() {
        ceiling
    } else {
        tier
    }
}

/// Hard ceiling on one response from a non-native backend, both transports: a
/// stdio response line or an HTTP body. Records already cap at 4096 bytes, so a
/// normal recall sits far below; this stops a buggy or hostile backend streaming
/// an unlimited body. Exceeding it is `MEM_HTTP_OVERSIZE` /
/// `MEM_PLUGIN_OVERSIZE`.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Startup line for a backend that failed to open. Session degrades to no memory
/// instead of silently falling back to SQLite, and this reports it rather than
/// leaving the operator to notice the agent forgot everything. Pure so tests can
/// pin the wording.
pub fn unavailable_message(sel: &crate::backend::BackendSelection, err: &PantheonError) -> String {
    let backend = if sel.name == "native" {
        "native".to_string()
    } else {
        sel.name.clone()
    };
    format!(
        "pantheon: memory backend '{backend}' is unavailable: {} {} — running WITHOUT memory \
(memory tools are not registered this session, so recall/write calls fail loudly rather \
than being silently dropped). Fix the backend or switch back with \
`pantheon memory backend select {backend}`; existing memories stay where they were - \
switching backends is not migration.",
        err.code, err.cause
    )
}

/// Error codes a memory server may return in `error`/`code`. The field is
/// attacker-controlled, so anything outside this maps to `fallback` rather than
/// becoming a code verbatim.
const KNOWN_REMOTE_CODES: &[&str] = &[
    "MEM_BACKEND_CONFIG",
    "MEM_BACKEND_UNKNOWN",
    "MEM_BACKEND_UNSUPPORTED",
    "MEM_BUDGET_EXCEEDED",
    "MEM_COUNT",
    "MEM_DELETE",
    "MEM_EMPTY",
    "MEM_EPHEMERAL",
    "MEM_EXPORT",
    "MEM_FTS_DELETE",
    "MEM_FTS_DROP",
    "MEM_FTS_OPEN",
    "MEM_FTS_REBUILD",
    "MEM_FTS_RECREATE",
    "MEM_FTS_UNUSABLE",
    "MEM_HTTP_BODY",
    "MEM_HTTP_CONN",
    "MEM_HTTP_DECODE",
    "MEM_HTTP_ENCODE",
    "MEM_HTTP_NO_URL",
    "MEM_HTTP_REMOTE",
    "MEM_IMPORT_READ",
    "MEM_LOCK",
    "MEM_MKDIR",
    "MEM_NATIVE_OPEN",
    "MEM_NOT_FOUND",
    "MEM_NO_CAPABILITY",
    "MEM_NO_PROVENANCE",
    "MEM_NO_READ_CAPABILITY",
    "MEM_OPEN",
    "MEM_PLUGIN_DECODE",
    "MEM_PLUGIN_EMPTY",
    "MEM_PLUGIN_ERROR",
    "MEM_PLUGIN_IO",
    "MEM_PLUGIN_MANIFEST",
    "MEM_PLUGIN_REMOTE",
    "MEM_PLUGIN_SPAWN",
    "MEM_PLUGIN_TIMEOUT",
    "MEM_PRAGMA",
    "MEM_PUT",
    "MEM_QUERY",
    "MEM_SCHEMA",
    "MEM_SEARCH",
    "MEM_SECRET",
    "MEM_SELECTION_WRITE",
    "MEM_SYNC_MKDIR",
    "MEM_SYNC_READ",
    "MEM_SYNC_RENAME",
    "MEM_SYNC_WRITE",
    "MEM_TOO_LARGE",
];

/// Sanitize a remote error code: allowlisted codes pass through, anything
/// else (including empty) becomes `fallback` (`MEM_HTTP_REMOTE` /
/// `MEM_PLUGIN_REMOTE`).
pub fn sanitize_remote_code(code: &str, fallback: &'static str) -> String {
    let c = code.trim();
    if !c.is_empty() && KNOWN_REMOTE_CODES.contains(&c) {
        c.to_string()
    } else {
        fallback.to_string()
    }
}

/// Remote `cause` strings are free text; strip control characters and cap
/// the length so a hostile server cannot smuggle log-forging newlines or
/// megabyte dumps into error paths.
pub fn sanitize_remote_text(s: &str) -> String {
    const MAX: usize = 500;
    let mut out = String::new();
    for ch in s.chars() {
        if out.len() >= MAX {
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}

/// Shared gate for the write path: capability, validation, trust clamp.
/// Returns the gated proposal ready for any provider.
fn gate_proposal(
    policy: &Policy,
    proposal: Proposal,
    max_bytes: usize,
) -> Result<Proposal, PantheonError> {
    // 1. policy - memory.write must be granted explicitly.
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
    // 2. validation (provenance is checked here too; the secret scan
    // refuses values that look like leaked credentials).
    if let Err(r) = validate(&proposal, max_bytes) {
        let remediation = match &r {
            WriteRefusal::SecretDetected { .. } => {
                "remove the secret material; store where to find it (vault name, env var) instead of the value itself"
            }
            _ => "fix the proposal; nothing was stored",
        };
        return Err(merr(
            r.code(),
            format!("proposal failed validation: {r:?}"),
            remediation,
        ));
    }
    // 3. trust clamp. Anything not authored by an explicit user action lands at
    // Untrusted whatever was requested. `memory.md` is the file importer -
    // human-authored, or a v2 file carrying each record's tier - so it keeps its
    // tier here; the importer separately caps it at the store's existing tier.
    let mut p = proposal;
    if !matches!(
        p.provenance.origin.as_str(),
        "user" | "cli" | "import" | "memory.md"
    ) {
        p.provenance.trust = pantheon_api::provenance::TrustTier::Untrusted;
    }
    Ok(p)
}
