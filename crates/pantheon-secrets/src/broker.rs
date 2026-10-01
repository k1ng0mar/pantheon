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
//!   contents, so events, `pantheon logs`, and logs stay clean.
//! - `inject` returns `None` for absent secrets rather than erroring, so
//!   callers degrade gracefully when a secret isn't configured.

use crate::env::EnvVault;
use crate::error::SecretsError;
use crate::value::SecretValue;
use crate::vault::SecretVault;
use std::path::Path;
use std::sync::Arc;

/// Resolves and injects secrets for a run.
#[derive(Debug, Default)]
pub struct SecretsBroker {
    /// Durable backends behind `Arc`: every vault guards its state with a
    /// `Mutex` and takes `&self`, so sharing is sound — and `clone()` keeps
    /// resolving against the same platform stores instead of silently
    /// dropping to memory-only.
    durable: Vec<Arc<dyn SecretVault>>,
    env: EnvVault,
    /// Host env names the plugin supervisor may copy into plugin
    /// subprocesses. Carried on the broker because the broker owns the
    /// run's secrets-boundary policy. Empty (default) = plugins receive no
    /// host vars beyond the curated minimum (PATH); manifest-declared names
    /// only cross the boundary on an explicit operator allowlist hit.
    plugin_env_allowlist: Vec<String>,
}

impl Clone for SecretsBroker {
    fn clone(&self) -> Self {
        // Vaults are shared, not dropped: the `Arc`s clone cheaply and the
        // clone resolves against the same platform stores. (An earlier
        // version rebuilt from the system environment here and silently
        // fell back to memory-only, losing every durable backend.)
        Self {
            durable: self.durable.clone(),
            env: self.env.clone(),
            plugin_env_allowlist: self.plugin_env_allowlist.clone(),
        }
    }
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
    ///
    /// The `env:` literal form resolves nothing unless an allowlist is
    /// installed via [`Self::with_env_allowlist`]: fail closed by default.
    pub fn from_system_env() -> Self {
        let mut durable: Vec<Arc<dyn SecretVault>> = Vec::new();
        if crate::keychain::KeychainVault::platform_available().is_ok() {
            durable.push(Arc::new(crate::keychain::KeychainVault::new()));
        }
        Self {
            durable,
            env: EnvVault::system(),
            plugin_env_allowlist: Vec::new(),
        }
    }

    /// Like `from_system_env`, but also picks up the legacy `PANTHEON_API_KEY`
    /// env var (and any config-named env var passed via `key_env`) by injecting
    /// it into a memory vault. The mirror is ranked *with the environment*
    /// (in front of the keychain) for reads, so exporting a rotated key
    /// always beats a stale stored one; an explicit `--key` still beats both
    /// via [`Self::with_vault_front`].
    ///
    /// The mirror is read-only: it is wrapped in
    /// [`crate::vault::ReadOnlyVault`] so a `set` falls through to the next
    /// durable vault (the keychain) instead of dying in process memory.
    pub fn from_system_env_with_api_key(key_env: Option<&str>) -> Self {
        let mem = crate::vault::MemoryVault::new();
        let found = key_env
            .and_then(|env| std::env::var(env).ok())
            .or_else(|| std::env::var("PANTHEON_API_KEY").ok());
        if let Some(k) = found {
            let _ = mem.set("PANTHEON_API_KEY", SecretValue::new(k));
        }
        Self::from_system_env().with_vault_front(Box::new(crate::vault::ReadOnlyVault::new(mem)))
    }

    /// Broker with a durable vault that survives restarts, for hosts where
    /// the OS keychain may be absent (headless Linux): the OS keychain
    /// when the platform has a usable credential store, otherwise an
    /// [`crate::filevault::EncryptedFileVault`] under `data_dir`
    /// (`secrets.json` sealed under `.secrets.key`, 0600).
    ///
    /// There is deliberately no in-memory fallback in the chain: a `set`
    /// that only reaches process memory silently loses the secret on
    /// restart, which is the failure this constructor exists to prevent.
    /// Fails loudly when the file vault cannot be opened (I/O, tampered
    /// envelope) rather than degrading to memory.
    pub fn durable(data_dir: &Path) -> Result<Self, SecretsError> {
        let mut durable: Vec<Arc<dyn SecretVault>> = Vec::new();
        if crate::keychain::KeychainVault::platform_available().is_ok() {
            durable.push(Arc::new(crate::keychain::KeychainVault::new()));
        } else {
            durable.push(Arc::new(crate::filevault::EncryptedFileVault::open(
                data_dir.join("secrets.json"),
                data_dir.join(".secrets.key"),
            )?));
        }
        Ok(Self {
            durable,
            env: EnvVault::system(),
            plugin_env_allowlist: Vec::new(),
        })
    }

    /// Add a durable vault (OS keychain, encrypted local, memory). Durable
    /// vaults are consulted in insertion order before the environment.
    pub fn with_vault(mut self, vault: Box<dyn SecretVault>) -> Self {
        self.durable.push(Arc::from(vault));
        self
    }

    /// Add a vault consulted before every existing one. Explicit overrides
    /// (a `--key` flag, a per-call credential) must beat config- and
    /// env-seeded vaults, and durable vaults resolve in insertion order.
    pub fn with_vault_front(mut self, vault: Box<dyn SecretVault>) -> Self {
        self.durable.insert(0, Arc::from(vault));
        self
    }

    /// Replace the environment source (tests use this).
    pub fn with_env(mut self, env: EnvVault) -> Self {
        self.env = env;
        self
    }

    /// Restrict the `env:` literal secret form to the allowlist (exact
    /// names or `PREFIX_*`; see [`crate::env::env_var_allowed`]). Default
    /// is empty: `env:` reads fail closed. Applies to the broker's own
    /// environment vault.
    pub fn with_env_allowlist(mut self, allowlist: Vec<String>) -> Self {
        self.env = self.env.with_env_allowlist(allowlist);
        self
    }

    /// Host env names the plugin supervisor may copy into plugin
    /// subprocesses (same entry syntax as the `env:` allowlist). Default is
    /// empty: plugins receive no host vars beyond the curated minimum.
    pub fn with_plugin_env_allowlist(mut self, allowlist: Vec<String>) -> Self {
        self.plugin_env_allowlist = allowlist;
        self
    }

    /// The plugin env allowlist (see [`Self::with_plugin_env_allowlist`]).
    pub fn plugin_env_allowlist(&self) -> &[String] {
        &self.plugin_env_allowlist
    }

    /// The `env:` literal-lookup allowlist on this broker's environment vault.
    pub fn env_allowlist(&self) -> &[String] {
        self.env.env_allowlist()
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

    /// All secret names across durable vaults and the environment, sorted.
    /// Never returns values — only names, so a caller can list what exists
    /// without exposing any secret material.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for vault in &self.durable {
            if let Ok(vault_names) = vault.names() {
                names.extend(vault_names);
            }
        }
        if let Ok(env_names) = self.env.names() {
            names.extend(env_names);
        }
        names.sort();
        names.dedup();
        names
    }

    /// Delete a secret from every vault that holds it. A name no vault
    /// knows is a no-op (not an error): the caller asked for it to be gone,
    /// and it is.
    ///
    /// A vault whose platform store cannot answer
    /// ([`SecretsError::Backend`]) is skipped, not fatal — the same
    /// degraded-store rule as [`Self::resolve`] and [`Self::set`]. Every
    /// other failure propagates: a delete that reports `Ok` really deleted.
    pub fn delete(&self, name: &str) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        for vault in &self.durable {
            match vault.delete(name) {
                Ok(()) => {}
                Err(SecretsError::Backend(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Item 3: a secret `set` through the durable broker survives a
    /// full drop-and-recreate from the same data dir. On a host with no
    /// OS keychain (this environment), the fallback is the
    /// EncryptedFileVault — a memory-only fallback would lose the
    /// secret at the second `durable()` call.
    #[test]
    fn durable_broker_secret_survives_restart() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-secrets-durable-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        {
            let broker = SecretsBroker::durable(&dir).expect("durable broker opens");
            broker
                .set("TEST_RESTART_SECRET", SecretValue::new("s3cr3t-value"))
                .expect("set through the durable broker");
        }
        let broker2 = SecretsBroker::durable(&dir).expect("durable broker reopens");
        let got = broker2
            .resolve("TEST_RESTART_SECRET")
            .expect("resolve works")
            .expect("secret survived the restart");
        assert_eq!(got.expose(), "s3cr3t-value");
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Item 5: redaction tests — broker descriptions and name listings must
/// never carry secret material.
#[cfg(test)]
mod broker_redaction_tests {
    use super::*;
    use crate::dotenv::DotenvVault;
    use crate::vault::SecretVault;

    #[test]
    fn describe_reports_presence_never_value() {
        // EnvVault normalizes lookups through PANTHEON_SECRET_<NAME>;
        // seed the map under the normalized key.
        let broker = SecretsBroker::new().with_env(EnvVault::from_map(vec![(
            "PANTHEON_SECRET_TEST_DESCRIBE_SECRET",
            "s3cr3t-value",
        )]));
        let d = broker.describe("TEST_DESCRIBE_SECRET");
        assert!(
            !d.contains("s3cr3t-value"),
            "describe() must not leak the value: {d}"
        );
        assert!(d.contains("present"), "presence is reported: {d}");
        let missing = broker.describe("TEST_DESCRIBE_MISSING");
        assert!(
            missing.contains("absent") && !missing.contains("s3cr3t"),
            "absent secrets describe cleanly: {missing}"
        );
    }

    #[test]
    fn dotenv_vault_roundtrip_and_names_are_names_only() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-secrets-redact-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let vault = DotenvVault::new(&dir);
        vault
            .set("TEST_REDACT_KEY", SecretValue::new("s3cr3t-value"))
            .expect("set works");
        let got = vault
            .get("TEST_REDACT_KEY")
            .expect("get works")
            .expect("round-trips");
        assert_eq!(got.expose(), "s3cr3t-value");
        let names = vault.names().expect("names works");
        assert!(
            names.iter().any(|n| n == "TEST_REDACT_KEY"),
            "names lists the key: {names:?}"
        );
        for n in &names {
            assert!(
                !n.contains("s3cr3t"),
                "names() must carry names only, never values: {n}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
