//! The secrets broker: resolution + injection at the execution boundary.
//!
//! Guarantees:
//! - Resolution order is deterministic: durable vaults in insertion order,
//!   then the environment. First hit wins.
//! - A durable vault whose platform store cannot answer
//!   ([`SecretsError::Backend`]) is skipped, not fatal: a broken keyring
//!   must never fail a resolve the next vault could satisfy. Deeper
//!   failures (tampered encrypted vault, I/O) still propagate loudly.
//! - Secrets travel only as [`SecretValue`]; `describe`/`Debug` never leak
//!   contents, so events, `/explain`, and logs stay clean.
//! - `inject` returns `None` for absent secrets rather than erroring, so
//!   callers degrade gracefully when a secret isn't configured.

use crate::env::EnvVault;
use crate::error::SecretsError;
use crate::value::SecretValue;
use crate::vault::SecretVault;

/// Resolves and injects secrets for a run.
#[derive(Debug, Default)]
pub struct SecretsBroker {
    durable: Vec<Box<dyn SecretVault>>,
    env: EnvVault,
}

impl SecretsBroker {
    /// Broker with no durable vaults; falls back to the environment only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Broker over the real process environment (`PANTHEON_SECRET_*`),
    /// plus the OS keychain first when this host has a usable platform
    /// credential store (spec §13 ranks it above every other durable
    /// backend).
    pub fn from_system_env() -> Self {
        let mut durable: Vec<Box<dyn SecretVault>> = Vec::new();
        if crate::keychain::KeychainVault::platform_available().is_ok() {
            durable.push(Box::new(crate::keychain::KeychainVault::new()));
        }
        Self {
            durable,
            env: EnvVault::system(),
        }
    }

    /// Like `from_system_env`, but also picks up the legacy `PANTHEON_API_KEY`
    /// env var (and any config-named env var passed via `key_env`) by injecting
    /// it into a memory vault. The mirror is ranked *with the environment*
    /// (in front of the keychain), so exporting a rotated key always beats a
    /// stale stored one; an explicit `--key` still beats both via
    /// [`Self::with_vault_front`].
    pub fn from_system_env_with_api_key(key_env: Option<&str>) -> Self {
        let mem = crate::vault::MemoryVault::new();
        let found = key_env
            .and_then(|env| std::env::var(env).ok())
            .or_else(|| std::env::var("PANTHEON_API_KEY").ok());
        if let Some(k) = found {
            let _ = mem.set("PANTHEON_API_KEY", SecretValue::new(k));
        }
        Self::from_system_env().with_vault_front(Box::new(mem))
    }

    /// Add a durable vault (OS keychain, encrypted local, memory). Durable
    /// vaults are consulted in insertion order before the environment.
    pub fn with_vault(mut self, vault: Box<dyn SecretVault>) -> Self {
        self.durable.push(vault);
        self
    }

    /// Add a vault consulted before every existing one. Explicit overrides
    /// (a `--key` flag, a per-call credential) must beat config- and
    /// env-seeded vaults, and durable vaults resolve in insertion order.
    pub fn with_vault_front(mut self, vault: Box<dyn SecretVault>) -> Self {
        self.durable.insert(0, vault);
        self
    }

    /// Replace the environment source (tests use this).
    pub fn with_env(mut self, env: EnvVault) -> Self {
        self.env = env;
        self
    }

    /// Resolve a secret: durable vaults first, then the environment.
    ///
    /// A vault whose platform store cannot answer ([`SecretsError::Backend`])
    /// is skipped — a broken keyring degrades to the next vault instead of
    /// failing the run. Any other error (tampered encrypted vault, bad
    /// request) propagates immediately.
    pub fn resolve(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        crate::error::validate_name(name)?;
        for vault in &self.durable {
            match vault.get(name) {
                Ok(Some(value)) => return Ok(Some(value)),
                Ok(None) => {}
                Err(SecretsError::Backend(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        self.env.get(name)
    }

    /// Execution-boundary injection. Same as [`resolve`], named for the call
    /// site where a run's tool/process/session credentials are handed over.
    ///
    /// The returned value must be passed to the execution boundary directly;
    /// it must never be logged or embedded into model context.
    pub fn inject(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        self.resolve(name)
    }

    /// Store into the first durable vault that can accept it (error when
    /// none is configured).
    ///
    /// A higher-ranked vault whose platform store cannot answer falls
    /// through to the next one — a broken keyring must not block a write
    /// the encrypted file could take — while every other failure
    /// propagates.
    pub fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        if self.durable.is_empty() {
            return Err(SecretsError::Unsupported(
                "no durable vault configured".into(),
            ));
        }
        let mut first_backend: Option<SecretsError> = None;
        for vault in &self.durable {
            match vault.set(name, value.clone()) {
                Ok(()) => return Ok(()),
                Err(SecretsError::Backend(e)) => {
                    if first_backend.is_none() {
                        first_backend = Some(SecretsError::Backend(e));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        // Every vault failed; the highest-ranked failure is the real one.
        Err(first_backend.expect("durable is non-empty, so at least one attempt ran"))
    }

    /// Redacted one-line description for logs/events: presence, never value.
    pub fn describe(&self, name: &str) -> String {
        match self.resolve(name) {
            Ok(Some(v)) => format!("secret:{name} present ({} bytes)", v.len()),
            Ok(None) => format!("secret:{name} absent"),
            Err(e) => format!("secret:{name} error={e}"),
        }
    }
}

#[cfg(test)]
#[path = "broker_tests.rs"]
mod tests;
