//! Pantheon config schema primitives.
//!
//! The full `config.toml` document lives in `pantheon-tui::config`; this
//! module holds the schema pieces other crates may validate against:
//! [`SecretRef`] (env-sourced API keys — raw values are rejected by
//! design) and [`PolicyPreset`] (the execution-policy enum).
//!
//! Moved out of `pantheon-tui` so the dashboard and other control-plane
//! crates can validate config edits without depending on the TUI.

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
    pub fn to_policy(self) -> crate::capability::Policy {
        match self {
            Self::Reader => crate::capability::Policy::researcher_readonly(),
            Self::Coder => crate::capability::Policy::coder(),
            Self::CoderMemory => crate::capability::Policy::coder_with_memory(),
        }
    }
}
