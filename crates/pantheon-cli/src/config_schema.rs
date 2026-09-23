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
pub enum PolicyPreset {
    /// Read-only: filesystem + memory read, nothing executes.
    Reader,
    /// Default coding policy: shell and edits allowed, memory gated.
    Coder,
    /// Coder plus memory tools enabled.
    CoderMemory,
}

impl Default for PolicyPreset {
    fn default() -> Self {
        Self::Coder
    }
}

impl PolicyPreset {
    #[allow(dead_code)] // used in doctor output formatting (next pass)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_ref_rejects_non_env_sources() {
        assert!(SecretRef {
            source: "raw".into(),
            name: "sk-123".into()
        }
        .validate()
        .is_err());
        assert!(SecretRef::from_env("PANTHEON_KEY").validate().is_ok());
        assert!(SecretRef::from_env("").validate().is_err());
        assert!(SecretRef::from_env("bad-name").validate().is_err());
    }

    #[test]
    fn policy_preset_round_trips() {
        for p in [
            PolicyPreset::Reader,
            PolicyPreset::Coder,
            PolicyPreset::CoderMemory,
        ] {
            assert_eq!(PolicyPreset::from_str(p.as_str()), Some(p));
        }
        assert_eq!(PolicyPreset::from_str("nope"), None);
    }
}
