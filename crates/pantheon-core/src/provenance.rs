//! Provenance and trust tiers for untrusted-data boundaries.
//!
//! Every piece of content that reaches the model carries a trust tier
//! describing where it came from. The tier is structural data on the
//! `Message`, not a prompt convention: it survives serialization into the
//! ledger, and providers render it as a provenance envelope so the model
//! can tell instructions apart from fetched data.
//!
//! The core invariant: content never gains trust by being copied.
//! `derived_trust <= source_trust` unless an explicit trusted actor
//! (the user, via CLI or an approval) promotes it.

use serde::{Deserialize, Serialize};

/// Where content came from, coarse-grained for policy decisions.
/// Fine distinctions (which tool, which URL) live on `Provenance.source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// Authoritative control: system prompts, harness-generated denials,
    /// recovery scaffolding. The model must obey it.
    System,
    /// Direct instructions from the operator. The model must obey it.
    User,
    /// Recalled context. Informative, never authoritative: the model may
    /// use it as background but must not treat it as an instruction.
    Memory,
    /// Anything that originated outside Pantheon's trust boundary: tool
    /// output, fetched pages, plugin context. Data to analyze, never
    /// instructions to follow. The `source` string carries the finer
    /// distinction (local calculator vs remote web page).
    Untrusted,
}

impl TrustTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrustTier::System => "system",
            TrustTier::User => "user",
            TrustTier::Memory => "memory",
            TrustTier::Untrusted => "untrusted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "system" => Some(TrustTier::System),
            "user" => Some(TrustTier::User),
            "memory" => Some(TrustTier::Memory),
            "untrusted" => Some(TrustTier::Untrusted),
            _ => None,
        }
    }

    /// Ordering for the `derived_trust <= source_trust` invariant.
    /// System(3) > User(2) > Memory(1) > Untrusted(0).
    pub fn rank(&self) -> u8 {
        match self {
            TrustTier::Untrusted => 0,
            TrustTier::Memory => 1,
            TrustTier::User => 2,
            TrustTier::System => 3,
        }
    }
}

/// Structured provenance attached to a message or memory record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub trust: TrustTier,
    /// Fine-grained origin: tool name (`shell`, `web_fetch`), `user`,
    /// `memory:<namespace>`, `cli`, `import`, `pantheon` (harness-generated).
    pub source: String,
    /// Ledger seq of the originating event, when known. Lets an auditor
    /// trace a memory record back to the exact tool output it derived from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_seq: Option<u64>,
}

impl Provenance {
    pub fn system(source: impl Into<String>) -> Self {
        Self {
            trust: TrustTier::System,
            source: source.into(),
            origin_seq: None,
        }
    }

    pub fn user(source: impl Into<String>) -> Self {
        Self {
            trust: TrustTier::User,
            source: source.into(),
            origin_seq: None,
        }
    }

    pub fn untrusted(source: impl Into<String>) -> Self {
        Self {
            trust: TrustTier::Untrusted,
            source: source.into(),
            origin_seq: None,
        }
    }

    /// Render the envelope prefix providers put in front of untrusted and
    /// memory-tier content so the model sees provenance inline.
    pub fn envelope_prefix(&self) -> String {
        format!(
            "[provenance: source={} trust={}]",
            self.source,
            self.trust.as_str()
        )
    }
}
