//! Pantheon config schema (`config.toml` in the data dir).
//!
//! Setup writes this file; doctor validates it; every subsystem reads it.
//! Secrets are never stored here: keys reference environment variables by
//! name through `api_key: { source = "env", name = "..." }`.

use serde::{Deserialize, Serialize};

/// Where an API key comes from. Only env sourcing is supported; raw values
/// in the config file are rejected by design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    pub source: String,
    pub name: String,
}

impl SecretRef {
    pub fn from_env(name: impl Into<String>) -> Self {
        Self {
            source: "env".into(),
            name: name.into(),
        }
    }
    /// Resolve at runtime. Never print the value.
    pub fn resolve(&self) -> Option<String> {
        if self.source == "env" && !self.name.is_empty() {
            std::env::var(&self.name).ok().filter(|v| !v.is_empty())
        } else {
            None
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.source != "env" {
            return Err(format!(
                "secret source {:?} is not supported (only \"env\")",
                self.source
            ));
        }
        if self.name.is_empty() {
            return Err("secret name cannot be empty".into());
        }
        if self
            .name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_')
        {
            Ok(())
        } else {
            Err(format!(
                "secret name {:?} should be an env var name (UPPER_SNAKE)",
                self.name
            ))
        }
    }
}

/// Execution policy preset. Maps to `Policy` at load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PolicyPreset {
    /// Read-only: filesystem + memory read, nothing executes.
    Reader,
    /// Default coding policy: shell and edits allowed, memory gated.
    #[default]
    Coder,
    /// Coder plus memory tools enabled.
    CoderMemory,
}

impl PolicyPreset {
    /// Canonical config spelling. Round-trips with `from_str`, so a preset
    /// read from config can be printed back in the form the user wrote.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Coder => "coder",
            Self::CoderMemory => "coder_memory",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "reader" => Some(Self::Reader),
            "coder" => Some(Self::Coder),
            "coder_memory" => Some(Self::CoderMemory),
            _ => None,
        }
    }

    /// The policy this preset names.
    ///
    /// Every entry point resolves the preset here. It used to inline a
    /// two-way `allow_memory` boolean at four call sites, so `reader` and
    /// `coder` both produced `Policy::coder()` and a user who set
    /// `policy = "reader"` got shell and file writes.
    pub fn to_policy(self) -> pantheon_api::capability::Policy {
        match self {
            Self::Reader => pantheon_api::capability::Policy::researcher_readonly(),
            Self::Coder => pantheon_api::capability::Policy::coder(),
            Self::CoderMemory => pantheon_api::capability::Policy::coder_with_memory(),
        }
    }
}

/// Resolve the policy for a session.
///
/// Config wins. With no config, `PANTHEON_ALLOW_MEMORY` only decides between
/// the two coder policies, and `PANTHEON_POLICY` picks the preset outright.
pub fn policy_for_config(
    file_cfg: &Option<crate::config_doc::Config>,
) -> pantheon_api::capability::Policy {
    if let Some(preset) = file_cfg.as_ref().and_then(|c| c.policy) {
        return preset.to_policy();
    }
    if let Ok(p) = std::env::var("PANTHEON_POLICY") {
        if let Some(preset) = PolicyPreset::from_str(&p) {
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

#[cfg(test)]
#[path = "config_schema_tests.rs"]
mod tests;
