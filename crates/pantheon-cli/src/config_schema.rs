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
#[path = "config_schema_tests.rs"]
mod tests;
