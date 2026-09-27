//! Environment compatibility vault.
//!
//! Lets 12-factor style deployments feed secrets through the process
//! environment without touching the runtime secret store:
//!
//! - Name `db.password` resolves to env var `PANTHEON_SECRET_DB_PASSWORD`
//!   (uppercase, `.`/`-` become `_`).
//! - Name `env:FOO` resolves to the literal env var `FOO` — but ONLY when
//!   `FOO` matches the vault's allowlist (see
//!   [`EnvVault::with_env_allowlist`]). Without an allowlist entry the
//!   lookup fails closed (`Ok(None)`), so a name can never be used to
//!   exfiltrate an arbitrary host variable like `env:AWS_SECRET_ACCESS_KEY`.
//!
//! The system vault is read-only; store in a durable vault instead.

use crate::error::SecretsError;
use crate::value::SecretValue;
use crate::vault::SecretVault;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

const PREFIX: &str = "PANTHEON_SECRET_";

/// Match one allowlist entry against an env var name.
///
/// - Exact entry `"FOO"` matches only `FOO`.
/// - Trailing-star entry `"PANTHEON_"` ... `"PANTHEON_*"` matches any name
///   with that prefix.
/// - `"*"` alone matches everything: the explicit opt-out for operators
///   who accept the old open behavior.
///
/// Entries are trimmed; empty entries never match. The same matcher gates
/// both `env:` secret lookups and plugin-subprocess env passthrough, so
/// the two boundaries cannot drift apart.
pub fn env_var_allowed(allowlist: &[String], var: &str) -> bool {
    allowlist.iter().any(|entry| {
        let entry = entry.trim();
        if entry.is_empty() {
            return false;
        }
        match entry.strip_suffix('*') {
            Some(prefix) => var.starts_with(prefix),
            None => var == entry,
        }
    })
}

/// Reads secrets from the process environment.
#[derive(Debug, Default)]
pub struct EnvVault {
    vars: Mutex<HashMap<String, SecretValue>>,
    /// `true` when this vault mirrors the real process environment
    /// (`system()`), which forbids `set`/`delete`.
    system: bool,
    /// Env var names readable through the `env:` literal form. Exact names
    /// or `PREFIX_*` entries (see [`env_var_allowed`]). Empty (default) =
    /// `env:` resolves nothing: fail closed.
    env_allowlist: Vec<String>,
}

impl Clone for EnvVault {
    fn clone(&self) -> Self {
        Self {
            vars: Mutex::new(self.vars.lock().expect("EnvVault mutex poisoned").clone()),
            system: self.system,
            env_allowlist: self.env_allowlist.clone(),
        }
    }
}

impl EnvVault {
    /// Snapshot the real process environment, keeping only
    /// `PANTHEON_SECRET_*` variables.
    pub fn system() -> Self {
        let vars = std::env::vars()
            .filter(|(k, _)| k.starts_with(PREFIX))
            .map(|(k, v)| (k, SecretValue::new(v)))
            .collect();
        Self {
            vars: Mutex::new(vars),
            system: true,
            env_allowlist: Vec::new(),
        }
    }

    /// Build a vault from an explicit map (tests, embedded runtimes).
    pub fn from_map<K, V>(vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            vars: Mutex::new(
                vars.into_iter()
                    .map(|(k, v)| (k.into(), SecretValue::new(v.into())))
                    .collect(),
            ),
            system: false,
            env_allowlist: Vec::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, HashMap<String, SecretValue>> {
        self.vars.lock().expect("EnvVault mutex poisoned")
    }

    /// Restrict the `env:` literal form to the given allowlist. Entries are
    /// exact var names or `PREFIX_*` wildcards (see [`env_var_allowed`]).
    /// Reads of non-allowlisted vars fail closed (`Ok(None)`).
    pub fn with_env_allowlist(mut self, allowlist: Vec<String>) -> Self {
        self.env_allowlist = allowlist;
        self
    }

    /// The allowlist governing `env:` reads on this vault.
    pub fn env_allowlist(&self) -> &[String] {
        &self.env_allowlist
    }

    /// Map a requested name to the env var key it reads.
    pub fn env_key(name: &str) -> String {
        if let Some(literal) = name.strip_prefix("env:") {
            return literal.to_string();
        }
        let normalized = name.replace(['.', '-'], "_").to_ascii_uppercase();
        format!("{PREFIX}{normalized}")
    }
}

impl SecretVault for EnvVault {
    fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        crate::error::validate_name(name)?;
        if let Some(literal) = name.strip_prefix("env:") {
            // Fail closed: a literal env read needs an explicit allowlist
            // hit, otherwise the `env:` form is an arbitrary host-variable
            // read gadget.
            if !env_var_allowed(&self.env_allowlist, literal) {
                return Ok(None);
            }
            let value = if self.system {
                // System vaults mirror the live process environment.
                std::env::var(literal).ok().map(SecretValue::new)
            } else {
                self.state().get(literal).cloned()
            };
            return Ok(value);
        }
        let key = Self::env_key(name);
        Ok(self.state().get(&key).cloned())
    }

    fn set(&self, _name: &str, _value: SecretValue) -> Result<(), SecretsError> {
        if self.system {
            return Err(SecretsError::Unsupported(
                "process environment is read-only; store in a durable vault".into(),
            ));
        }
        Err(SecretsError::Unsupported(
            "use MemoryVault or EncryptedFileVault to store; EnvVault is for reads only".into(),
        ))
    }

    fn delete(&self, _name: &str) -> Result<(), SecretsError> {
        if self.system {
            return Err(SecretsError::Unsupported(
                "process environment is read-only".into(),
            ));
        }
        Err(SecretsError::Unsupported(
            "EnvVault is for reads only".into(),
        ))
    }

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        let mut names: Vec<String> = self
            .state()
            .keys()
            .filter(|k| k.starts_with(PREFIX))
            .map(|k| k.trim_start_matches(PREFIX).to_ascii_lowercase())
            .collect();
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
#[path = "env_tests.rs"]
mod tests;
