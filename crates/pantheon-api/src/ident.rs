//! Shared identifier rules.
//!
//! `is_slug` is a bottom-crate rule for a reason: `pantheon-storage` validates
//! collaborator/agent names in its tables, and depending on
//! `pantheon-agent` from storage would risk a
//! `capability → tools → exec → storage` back-edge (docs/developer/decisions/0001 §3). One canonical check, no drift:
//! `AgentProfile::is_slug` delegates here.

/// A name that appears in table headers, memory namespaces, and artifact
/// ids must be a slug: non-empty, ASCII alphanumeric plus `-` and `_`.
pub fn is_slug(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
