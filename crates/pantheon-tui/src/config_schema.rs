//! Pantheon config schema (`config.toml` in the data dir).
//!
//! The schema primitives ([`SecretRef`], [`PolicyPreset`]) live in
//! `pantheon_api::config_schema` so non-TUI crates (dashboard, doctor)
//! can validate against them. This module re-exports them and keeps the
//! TUI-local resolver that needs [`crate::config::Config`].

pub use pantheon_api::config_schema::{PolicyPreset, SecretRef};

/// Resolve the policy for a session.
///
/// Config wins. With no config, `PANTHEON_ALLOW_MEMORY` only decides between
/// the two coder policies, and `PANTHEON_POLICY` picks the preset outright.
pub fn policy_for_config(
    file_cfg: &Option<crate::config::Config>,
) -> pantheon_api::capability::Policy {
    if let Some(preset) = file_cfg.as_ref().and_then(|c| c.policy) {
        return preset.to_policy();
    }
    if let Ok(p) = std::env::var("PANTHEON_POLICY") {
        if let Some(preset) = PolicyPreset::parse(&p) {
            return preset.to_policy();
        }
    }
    let allow_memory = std::env::var("PANTHEON_ALLOW_MEMORY")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    if allow_memory {
        pantheon_api::capability::Policy::coder_with_memory()
    } else {
        pantheon_api::capability::Policy::coder()
    }
}
