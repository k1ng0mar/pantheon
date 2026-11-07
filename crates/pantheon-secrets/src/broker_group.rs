//! broker_group: broker, vault, value, error, filevault, keychain, grants, redaction in one file.

pub mod broker {
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
    use crate::redaction::RedactionRegistry;
    use crate::value::SecretValue;
    use crate::vault::SecretVault;
    use std::path::Path;
    use std::sync::Arc;

    /// Resolves and injects secrets for a run.
    #[derive(Debug)]
    pub struct SecretsBroker {
        /// Durable backends behind `Arc`: every vault guards its state with a
        /// `Mutex` and takes `&self`, so sharing is sound - and `clone()` keeps
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
        /// Exact-value redaction registry. `inject` records every handed-out
        /// value here; the session scrubs model-facing tool output against it.
        /// Shared by `Arc`, so clones register into the same registry instead
        /// of silently splitting it.
        redaction: Arc<RedactionRegistry>,
        /// When Some, only these names resolve. Used for delegated child grants.
        allowed_names: Option<std::collections::HashSet<String>>,
    }

    impl Default for SecretsBroker {
        fn default() -> Self {
            Self {
                durable: Vec::new(),
                env: EnvVault::default(),
                plugin_env_allowlist: Vec::new(),
                redaction: RedactionRegistry::for_scope("default"),
                allowed_names: None,
            }
        }
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
                redaction: Arc::clone(&self.redaction),
                allowed_names: self.allowed_names.clone(),
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
                redaction: RedactionRegistry::for_scope("default"),
                allowed_names: None,
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
            Self::from_system_env()
                .with_vault_front(Box::new(crate::vault::ReadOnlyVault::new(mem)))
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
        ///
        /// "The platform has a usable credential store" is checked with a real
        /// probe, not just `store_status`. A D-Bus session can exist on a
        /// headless host while the `login` keyring collection does not, and
        /// `store_status` reports Ok for that case. Trusting it put a broker in
        /// front of a keychain that can only ever fail, so every `set` returned
        /// a backend error and the file vault that would have worked was never
        /// opened.
        pub fn durable(data_dir: &Path) -> Result<Self, SecretsError> {
            let keychain_usable = crate::keychain::KeychainVault::platform_available().is_ok()
                && crate::keychain::KeychainVault::usable().is_ok();
            Self::durable_with(data_dir, keychain_usable)
        }

        /// The selection half of [`Self::durable`], with the environment probe
        /// split out. Tests pass `keychain_usable: false` so the file-vault
        /// fallback is exercised without touching the developer's real keyring
        /// (a locked keyring would otherwise prompt for an unlock, bounded by
        /// the keychain operation timeout).
        ///
        /// Public because a host that builds a broker per request needs an
        /// enumerable backend: the OS keychain cannot list its entries, so under
        /// the keychain backend a secret stored by one request reads back as
        /// `SECRET_NOT_FOUND` on the next. Headless hosts and tests pin `false`.
        pub fn durable_with(data_dir: &Path, keychain_usable: bool) -> Result<Self, SecretsError> {
            let mut durable: Vec<Arc<dyn SecretVault>> = Vec::new();
            if keychain_usable {
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
                redaction: RedactionRegistry::for_scope("default"),
                allowed_names: None,
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

        /// Restrict resolution to exactly these names (delegated child grants).
        /// Names outside the set refuse instead of falling through to env.
        pub fn with_allowed_names(mut self, names: std::collections::HashSet<String>) -> Self {
            self.allowed_names = Some(names);
            self
        }

        /// Scope the redaction registry to `scope` (a data dir or profile home),
        /// so multiplexed scopes never cross-scrub: scrubbing scope B's output
        /// with scope A's values would confirm to B that those bytes exist.
        /// The default scope is shared process-wide and is correct for
        /// single-scope processes.
        pub fn with_redaction_scope(mut self, scope: &str) -> Self {
            self.redaction = RedactionRegistry::for_scope(scope);
            self
        }

        /// The exact-value redaction registry this broker registers into.
        /// The session scrubs model-facing tool output against it.
        pub fn redaction_registry(&self) -> Arc<RedactionRegistry> {
            Arc::clone(&self.redaction)
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
        /// is skipped - a broken keyring degrades to the next vault instead of
        /// failing the run. Any other error (tampered encrypted vault, bad
        /// request) propagates immediately.
        pub fn resolve(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
            if let Some(allowed) = &self.allowed_names {
                if !allowed.contains(name) {
                    return Err(SecretsError::Unsupported(format!(
                        "SECRET_NOT_GRANTED: {name} is not granted to this run"
                    )));
                }
            }
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
        ///
        /// Every handed-out value is registered with the redaction registry
        /// (fill-time registration): the exact bytes are recorded before they
        /// can appear in any tool result, so a later echo is scrubbed before it
        /// reaches the model. Registration never fails the injection.
        pub fn inject(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
            if let Some(allowed) = &self.allowed_names {
                if !allowed.contains(name) {
                    return Ok(None);
                }
            }
            let result = self.resolve(name);
            if let Ok(Some(ref value)) = result {
                if !value.is_empty() {
                    self.redaction.register(value.expose());
                }
            }
            result
        }

        /// Store into the first durable vault that can accept it (error when
        /// none is configured).
        ///
        /// A higher-ranked vault whose platform store cannot answer falls
        /// through to the next one - a broken keyring must not block a write
        /// the encrypted file could take - while every other failure
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

        /// All secret names across durable vaults and the environment, sorted,
        /// plus one warning per vault whose listing failed. Never returns
        /// values - only names, so a caller can list what exists without
        /// exposing any secret material.
        ///
        /// The list is NOT complete when warnings are non-empty: a vault
        /// error is swallowed by no one here, it is reported. In
        /// particular the OS keychain vault can never enumerate
        /// ([`crate::keychain::KeychainVault::names`] always returns
        /// `Unsupported` - the platform APIs expose lookup-by-name, not
        /// listing), so any broker backed by the keychain omits every
        /// keychain entry from this list by design. Callers that must not
        /// present a partial list as complete (the dashboard env page, for
        /// example) use the warnings, not just the names.
        pub fn names_with_warnings(&self) -> (Vec<String>, Vec<String>) {
            let mut names: Vec<String> = Vec::new();
            let mut warnings: Vec<String> = Vec::new();
            for (i, vault) in self.durable.iter().enumerate() {
                match vault.names() {
                    Ok(vault_names) => names.extend(vault_names),
                    Err(e) => warnings.push(format!(
                        "durable vault {i} could not list names ({e}); entries held there are missing from this list"
                    )),
                }
            }
            match self.env.names() {
                Ok(env_names) => names.extend(env_names),
                Err(e) => warnings.push(format!(
                    "environment vault could not list names ({e}); entries held there are missing from this list"
                )),
            }
            names.sort();
            names.dedup();
            (names, warnings)
        }

        /// All secret names across durable vaults and the environment, sorted.
        /// Never returns values - only names, so a caller can list what exists
        /// without exposing any secret material.
        ///
        /// Convenience over [`names_with_warnings`](Self::names_with_warnings):
        /// warnings are printed to stderr, because a silently partial list
        /// used to be the behaviour here and it hid keychain entries and
        /// broken vaults alike. Callers that render the list to a user
        /// should use `names_with_warnings` and show the warnings alongside.
        pub fn names(&self) -> Vec<String> {
            let (names, warnings) = self.names_with_warnings();
            for w in warnings {
                eprintln!("secrets: {w}");
            }
            names
        }

        /// Delete a secret from every vault that holds it. A name no vault
        /// knows is a no-op (not an error): the caller asked for it to be gone,
        /// and it is.
        ///
        /// A vault whose platform store cannot answer
        /// ([`SecretsError::Backend`]) is skipped, not fatal - the same
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
        /// full drop-and-recreate from the same data dir. The fallback for a
        /// host with no usable OS keychain is the EncryptedFileVault - a
        /// memory-only fallback would lose the secret at the second broker.
        ///
        /// Forces the fallback instead of probing the real keychain: a test
        /// must not write to the developer's credential store, and a locked
        /// keyring would otherwise prompt for an unlock (now bounded by the
        /// keychain operation timeout).
        #[test]
        fn durable_broker_secret_survives_restart() {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-secrets-durable-test-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            {
                let broker =
                    SecretsBroker::durable_with(&dir, false).expect("durable broker opens");
                broker
                    .set("TEST_RESTART_SECRET", SecretValue::new("s3cr3t-value"))
                    .expect("set through the durable broker");
            }
            let broker2 = SecretsBroker::durable_with(&dir, false).expect("durable broker reopens");
            let got = broker2
                .resolve("TEST_RESTART_SECRET")
                .expect("resolve works")
                .expect("secret survived the restart");
            assert_eq!(got.expose(), "s3cr3t-value");
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// Item 5: redaction tests - broker descriptions and name listings must
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

    /// Exact-value registry wiring: `inject` registers every handed-out value
    /// so the session's model-facing scrub can catch later echoes.
    #[cfg(test)]
    mod broker_registry_tests {
        use super::*;

        #[test]
        fn inject_registers_value_for_scrubbing() {
            // EnvVault normalizes lookups through PANTHEON_SECRET_<NAME>;
            // seed the map under the normalized key (no process-env races).
            let broker = SecretsBroker::new()
                .with_env(EnvVault::from_map(vec![(
                    "PANTHEON_SECRET_TEST_REGISTRY_SECRET",
                    "registry-s3cr3t-789",
                )]))
                .with_redaction_scope("test-broker-registry");
            let value = broker
                .inject("TEST_REGISTRY_SECRET")
                .expect("inject resolves")
                .expect("secret is present");
            assert!(!value.is_empty());
            let scrubbed = broker
                .redaction_registry()
                .scrub("echoed: registry-s3cr3t-789!");
            assert_eq!(scrubbed, "echoed: [redacted-secret]!");
        }

        #[test]
        fn clone_shares_the_registry() {
            let broker = SecretsBroker::new()
                .with_env(EnvVault::from_map(vec![(
                    "PANTHEON_SECRET_TEST_REGISTRY_CLONE",
                    "clone-s3cr3t-456",
                )]))
                .with_redaction_scope("test-broker-registry-clone");
            let clone = broker.clone();
            clone
                .inject("TEST_REGISTRY_CLONE")
                .expect("inject resolves")
                .expect("secret is present");
            // Registered through the clone, visible through the original.
            let scrubbed = broker.redaction_registry().scrub("clone-s3cr3t-456");
            assert_eq!(scrubbed, "[redacted-secret]");
        }

        #[test]
        fn absent_secret_registers_nothing() {
            let broker = SecretsBroker::new().with_redaction_scope("test-broker-registry-absent");
            let missing = broker.inject("TEST_REGISTRY_DEFINITELY_ABSENT").unwrap();
            assert!(missing.is_none());
            assert_eq!(
                broker.redaction_registry().scrub("clean text"),
                "clean text"
            );
        }
    }

    #[cfg(test)]
    mod broker_names_warnings_tests {
        use super::*;
        use crate::vault::{MemoryVault, SecretVault};

        /// A vault that answers everything except listing: the keychain
        /// shape (lookup works, enumeration does not).
        #[derive(Debug)]
        struct UnlistableVault;

        impl SecretVault for UnlistableVault {
            fn get(&self, _name: &str) -> Result<Option<SecretValue>, SecretsError> {
                Ok(None)
            }
            fn set(&self, _name: &str, _value: SecretValue) -> Result<(), SecretsError> {
                Ok(())
            }
            fn delete(&self, _name: &str) -> Result<(), SecretsError> {
                Ok(())
            }
            fn names(&self) -> Result<Vec<String>, SecretsError> {
                Err(SecretsError::Backend("cannot enumerate".into()))
            }
        }

        /// A listing failure surfaces as a warning, not a silently partial
        /// list: the caller learns the names are incomplete.
        #[test]
        fn names_with_warnings_reports_unlistable_vault() {
            let memory = MemoryVault::new();
            memory.set("LISTED", SecretValue::new("v")).unwrap();
            let broker = SecretsBroker::new()
                .with_vault(Box::new(memory))
                .with_vault(Box::new(UnlistableVault))
                .with_env(crate::env::EnvVault::from_map(Vec::<(&str, &str)>::new()));
            let (names, warnings) = broker.names_with_warnings();
            assert_eq!(names, vec!["LISTED".to_string()]);
            assert_eq!(warnings.len(), 1);
            assert!(
                warnings[0].contains("missing"),
                "warning must say entries are missing: {}",
                warnings[0]
            );
        }
    }
}

pub mod vault {
    //! Vault abstraction.
    //!
    //! One `SecretVault` = one storage backend. Backends in the skeleton:
    //! - [`crate::env::EnvVault`] - process environment compat (always present).
    //! - [`crate::filevault::EncryptedFileVault`] - AES-256-GCM sealed JSON file.
    //! - [`MemoryVault`] - tests / ephemeral runtime.
    //!
    //! - [`crate::keychain::KeychainVault`] - macOS Keychain, Windows CredMan,
    //!   Linux Secret Service (the preferred durable backend).
    //!
    //! All of these implement [`SecretVault`].

    use crate::error::SecretsError;
    use crate::value::SecretValue;
    use std::collections::HashMap;
    use std::sync::{Mutex, MutexGuard};

    /// Storage backend for secrets.
    ///
    /// Implementations guard their state with a [`Mutex`] and take `&self`, so
    /// a broker may share vaults across the runtime. `get` must never block on
    /// network or prompt - it is called on hot paths.
    pub trait SecretVault: Send + Sync + std::fmt::Debug {
        /// Fetch a secret, or `None` when this backend has no such name.
        fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError>;
        /// Store or overwrite a secret.
        fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError>;
        /// Delete a secret. Removing an unknown name is a no-op.
        fn delete(&self, name: &str) -> Result<(), SecretsError>;
        /// All names this backend holds.
        fn names(&self) -> Result<Vec<String>, SecretsError>;
    }

    /// In-memory vault - for tests and ephemeral runtime use. Contents are lost
    /// on drop and are never persisted.
    #[derive(Debug, Default)]
    pub struct MemoryVault {
        secrets: Mutex<HashMap<String, SecretValue>>,
    }

    impl MemoryVault {
        pub fn new() -> Self {
            Self::default()
        }

        fn state(&self) -> MutexGuard<'_, HashMap<String, SecretValue>> {
            self.secrets.lock().expect("MemoryVault mutex poisoned")
        }
    }

    impl SecretVault for MemoryVault {
        fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
            crate::error::validate_name(name)?;
            Ok(self.state().get(name).cloned())
        }

        fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
            crate::error::validate_name(name)?;
            self.state().insert(name.to_string(), value);
            Ok(())
        }

        fn delete(&self, name: &str) -> Result<(), SecretsError> {
            crate::error::validate_name(name)?;
            self.state().remove(name);
            Ok(())
        }

        fn names(&self) -> Result<Vec<String>, SecretsError> {
            let mut names: Vec<String> = self.state().keys().cloned().collect();
            names.sort();
            Ok(names)
        }
    }

    /// Read-only view over another vault.
    ///
    /// An env-var mirror (see
    /// [`crate::broker::SecretsBroker::from_system_env_with_api_key`]) must win
    /// reads - an exported rotation beats a stale stored value - but must never
    /// absorb writes: a write landing in a process-memory mirror dies with the
    /// process instead of reaching durable storage. `set`/`delete` fail with
    /// [`SecretsError::Backend`] so the broker treats this vault like a
    /// degraded platform store and falls through to the next durable vault,
    /// exactly as it does for a locked keychain.
    #[derive(Debug)]
    pub struct ReadOnlyVault<V: SecretVault> {
        inner: V,
    }

    impl<V: SecretVault> ReadOnlyVault<V> {
        /// Wrap `inner`; reads and listings pass through, writes fail closed.
        pub fn new(inner: V) -> Self {
            Self { inner }
        }

        /// Unwrap back to the inner vault.
        pub fn into_inner(self) -> V {
            self.inner
        }
    }

    impl<V: SecretVault> SecretVault for ReadOnlyVault<V> {
        fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
            self.inner.get(name)
        }

        fn set(&self, _name: &str, _value: SecretValue) -> Result<(), SecretsError> {
            Err(SecretsError::Backend(
                "read-only vault: writes fall through to the next durable vault".into(),
            ))
        }

        fn delete(&self, _name: &str) -> Result<(), SecretsError> {
            Err(SecretsError::Backend(
                "read-only vault: deletes fall through to the next durable vault".into(),
            ))
        }

        fn names(&self) -> Result<Vec<String>, SecretsError> {
            self.inner.names()
        }
    }
}

pub mod value {
    //! Zeroizing secret value.
    //!
    //! `SecretValue` is the only type a secret travels as. It zeroizes its
    //! backing buffer on drop, never appears verbatim in its own `Debug`
    //! output, and exposes its contents only through [`SecretValue::expose`],
    //! which is reserved for the execution boundary.

    use std::fmt;
    use zeroize::Zeroizing;

    /// A secret value that scrubs its contents on drop.
    ///
    /// Cloning is allowed but discouraged; prefer moving. Logs/errors/events
    /// must only ever carry `Debug` or [`SecretValue::len`], never raw bytes.
    #[derive(Clone, Default)]
    pub struct SecretValue(Zeroizing<String>);

    impl SecretValue {
        pub fn new(value: impl Into<String>) -> Self {
            Self(Zeroizing::new(value.into()))
        }

        /// Borrow the secret for the execution boundary.
        ///
        /// Do not persist, log, or embed this into model context.
        pub fn expose(&self) -> &str {
            &self.0
        }

        pub fn len(&self) -> usize {
            self.0.len()
        }

        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }

    impl fmt::Debug for SecretValue {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "SecretValue(***)")
        }
    }

    impl PartialEq for SecretValue {
        fn eq(&self, other: &Self) -> bool {
            self.0 == other.0
        }
    }

    impl Eq for SecretValue {}

    /// Item 5: redaction tests - a secret's value must never surface through
    /// formatting or description paths, only through [`SecretValue::expose`]
    /// at the execution boundary.
    #[cfg(test)]
    mod value_redaction_tests {
        use super::*;

        #[test]
        fn debug_never_shows_the_value() {
            let v = SecretValue::new("s3cr3t-value");
            let d = format!("{v:?}");
            assert_eq!(d, "SecretValue(***)");
            assert!(
                !d.contains("s3cr3t"),
                "Debug output must not contain secret material: {d}"
            );
        }

        #[test]
        fn expose_still_returns_the_value_at_the_boundary() {
            // The redaction is a display contract, not data loss: the
            // execution boundary still gets the real bytes via expose().
            assert_eq!(SecretValue::new("s3cr3t-value").expose(), "s3cr3t-value");
        }
    }
}

pub mod error {
    //! Structured secrets failure information.

    use std::fmt;

    /// Errors produced by vaults and the broker.
    #[derive(Debug)]
    pub enum SecretsError {
        /// Filesystem-level failure (key file, encrypted vault file, perms).
        Io(std::io::Error),
        /// Encryption/decryption failure (tampered file, wrong key, corrupt data).
        Crypto(String),
        /// The operation is not available for this vault/backend.
        Unsupported(String),
        /// The request itself was invalid (empty name, bad reference).
        Invalid(String),
        /// The platform backend failed (OS keychain locked, D-Bus error,
        /// credential store missing). The secret itself may well exist
        /// the store just could not answer.
        Backend(String),
    }

    impl fmt::Display for SecretsError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                SecretsError::Io(e) => write!(f, "secrets I/O error: {e}"),
                SecretsError::Crypto(m) => write!(f, "secrets crypto failure: {m}"),
                SecretsError::Unsupported(m) => write!(f, "secrets unsupported: {m}"),
                SecretsError::Invalid(m) => write!(f, "invalid secrets request: {m}"),
                SecretsError::Backend(m) => write!(f, "secrets backend failure: {m}"),
            }
        }
    }

    impl std::error::Error for SecretsError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                SecretsError::Io(e) => Some(e),
                _ => None,
            }
        }
    }

    impl From<std::io::Error> for SecretsError {
        fn from(e: std::io::Error) -> Self {
            SecretsError::Io(e)
        }
    }

    impl From<serde_json::Error> for SecretsError {
        fn from(e: serde_json::Error) -> Self {
            SecretsError::Crypto(format!("json: {e}"))
        }
    }

    /// Name validation shared by vaults and the broker.
    pub(crate) fn validate_name(name: &str) -> Result<(), SecretsError> {
        if name.is_empty() {
            return Err(SecretsError::Invalid(
                "secret name must not be empty".into(),
            ));
        }
        if name.len() > 256 {
            return Err(SecretsError::Invalid(
                "secret name exceeds 256 chars".into(),
            ));
        }
        Ok(())
    }
}

pub mod filevault {
    //! Encrypted local vault (the fallback when no OS keychain is available).
    //!
    //! Everything is sealed with AES-256-GCM under a 32-byte random key stored
    //! in a separate 0600 key file. The payload is written atomically (temp +
    //! rename) so a crash can't leave a torn vault, and any bit flip in the file
    //! fails decryption loudly instead of returning wrong secrets.
    //!
    //! Two limitations are by design and documented, not hidden:
    //!
    //! - **The key sits beside the vault.** The key file lives in the same
    //!   data dir as `secrets.json` (that is the only location the vault is
    //!   ever told about), so anyone who can read the data dir can read
    //!   both the ciphertext and its key. The vault protects secrets at
    //!   rest from casual reads, not from a data-dir compromise. Moving the
    //!   key elsewhere (OS keychain, separate mount) needs a migration plan
    //!   for existing installs and is deliberately not done silently here.
    //! - **Permissions are ratcheted, not just set on creation.** An
    //!   existing key file that is group/world-readable is tightened to
    //!   0600 on load; a file whose mode cannot be tightened fails the open
    //!   rather than proceeding with a readable key.
    //!
    //! OS keychains are the preferred backend; this is the self-hosted fallback.

    use crate::error::SecretsError;
    use crate::value::SecretValue;
    use crate::vault::SecretVault;
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use rand::Rng;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};
    use zeroize::Zeroizing;

    #[derive(Debug, Serialize, Deserialize)]
    struct Envelope {
        v: u32,
        nonce: String,      // hex
        ciphertext: String, // hex
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Payload {
        secrets: HashMap<String, String>,
    }

    /// Local-file vault sealed with AES-256-GCM.
    #[derive(Debug)]
    pub struct EncryptedFileVault {
        data_path: PathBuf,
        key: Zeroizing<[u8; 32]>,
        /// In-memory cache mirroring the decrypted file; persisted on mutation.
        cache: Mutex<HashMap<String, SecretValue>>,
    }

    const ENVELOPE_VERSION: u32 = 1;

    impl EncryptedFileVault {
        /// Open (creating if missing) the vault at `data_path`, using/creating
        /// a 0600 key file at `key_path`.
        pub fn open(
            data_path: impl AsRef<Path>,
            key_path: impl AsRef<Path>,
        ) -> Result<Self, SecretsError> {
            let data_path = data_path.as_ref().to_path_buf();
            let key = load_or_create_key(key_path.as_ref())?;
            let cache = Mutex::new(decrypt_file(&data_path, &key)?);
            Ok(Self {
                data_path,
                key,
                cache,
            })
        }

        fn state(&self) -> MutexGuard<'_, HashMap<String, SecretValue>> {
            self.cache
                .lock()
                .expect("EncryptedFileVault mutex poisoned")
        }

        fn persist(&self, map: &HashMap<String, SecretValue>) -> Result<(), SecretsError> {
            let payload = Payload {
                secrets: map
                    .iter()
                    .map(|(k, v)| (k.clone(), v.expose().to_string()))
                    .collect(),
            };
            let plaintext = serde_json::to_vec(&payload)
                .map_err(|e| SecretsError::Crypto(format!("serialize: {e}")))?;
            encrypt_and_write(&self.data_path, &self.key, &plaintext)
        }
    }

    impl SecretVault for EncryptedFileVault {
        fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
            crate::error::validate_name(name)?;
            Ok(self.state().get(name).cloned())
        }

        fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
            crate::error::validate_name(name)?;
            // Persist first, then swap the cache: the old code mutated the
            // in-memory cache before persisting, so a persist failure
            // returned Err while the process-local cache already reflected
            // the change until restart - a lie to every later `get` in this
            // process. The mutex is not held across the write.
            let new_map = {
                let map = self.state();
                let mut new_map = map.clone();
                new_map.insert(name.to_string(), value);
                new_map
            };
            self.persist(&new_map)?;
            let mut map = self.state();
            *map = new_map;
            Ok(())
        }

        fn delete(&self, name: &str) -> Result<(), SecretsError> {
            crate::error::validate_name(name)?;
            // Same persist-first rule as `set`: a failed delete must leave
            // the cached secret readable, not silently gone.
            let new_map = {
                let map = self.state();
                let mut new_map = map.clone();
                new_map.remove(name);
                new_map
            };
            self.persist(&new_map)?;
            let mut map = self.state();
            *map = new_map;
            Ok(())
        }

        fn names(&self) -> Result<Vec<String>, SecretsError> {
            let mut names: Vec<String> = self.state().keys().cloned().collect();
            names.sort();
            Ok(names)
        }
    }

    fn load_or_create_key(path: &Path) -> Result<Zeroizing<[u8; 32]>, SecretsError> {
        if path.exists() {
            let bytes = fs::read(path)?;
            if bytes.len() != 32 {
                return Err(SecretsError::Crypto(format!(
                    "key file {} has {} bytes, want 32",
                    path.display(),
                    bytes.len()
                )));
            }
            // 0600 was previously set only on creation: an existing key file
            // left group/world-readable (umask, restore, copy) stayed that
            // way forever. Ratchet it on every load; failing to tighten
            // fails the open rather than proceeding with a readable key.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(path)?.permissions().mode() & 0o777;
                if mode != 0o600 {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                }
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            Ok(Zeroizing::new(key))
        } else {
            let mut key = Zeroizing::new([0u8; 32]);
            rand::rng().fill_bytes(key.as_mut());
            write_key_file(path, &key)?;
            Ok(key)
        }
    }

    fn write_key_file(path: &Path, key: &[u8; 32]) -> Result<(), SecretsError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        set_private_perms(&file)?;
        file.write_all(key)?;
        file.sync_all()?;
        Ok(())
    }

    #[cfg(unix)]
    fn set_private_perms(file: &fs::File) -> Result<(), SecretsError> {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(0o600);
        file.set_permissions(perms)?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn set_private_perms(_file: &fs::File) -> Result<(), SecretsError> {
        Ok(())
    }

    /// Decrypt the vault file; a missing file decrypts to an empty vault.
    pub(crate) fn decrypt_file(
        path: &Path,
        key: &[u8; 32],
    ) -> Result<HashMap<String, SecretValue>, SecretsError> {
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let raw = fs::read(path)?;
        let envelope: Envelope = serde_json::from_slice(&raw)
            .map_err(|e| SecretsError::Crypto(format!("bad envelope: {e}")))?;
        if envelope.v != ENVELOPE_VERSION {
            return Err(SecretsError::Crypto(format!(
                "unsupported vault version {}",
                envelope.v
            )));
        }
        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|e| SecretsError::Crypto(format!("cipher init: {e}")))?;
        let nonce = hex_decode(&envelope.nonce)?;
        let ct = hex_decode(&envelope.ciphertext)?;
        if nonce.len() != 12 {
            return Err(SecretsError::Crypto("bad nonce length".into()));
        }
        let plaintext = cipher
            .decrypt(
                &Nonce::try_from(nonce.as_slice()).expect("nonce length checked above"),
                ct.as_ref(),
            )
            .map_err(|_| {
                SecretsError::Crypto("decryption failed (tampered or wrong key)".into())
            })?;
        let payload: Payload = serde_json::from_slice(&plaintext)
            .map_err(|e| SecretsError::Crypto(format!("bad payload: {e}")))?;
        Ok(payload
            .secrets
            .into_iter()
            .map(|(k, v)| (k, SecretValue::new(v)))
            .collect())
    }

    fn encrypt_and_write(
        path: &Path,
        key: &[u8; 32],
        plaintext: &[u8],
    ) -> Result<(), SecretsError> {
        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|e| SecretsError::Crypto(format!("cipher init: {e}")))?;
        let mut nonce_bytes = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let ct = cipher
            .encrypt(
                &Nonce::try_from(nonce_bytes.as_slice()).expect("nonce is 12 bytes"),
                plaintext,
            )
            .map_err(|_| SecretsError::Crypto("encryption failed".into()))?;
        let envelope = Envelope {
            v: ENVELOPE_VERSION,
            nonce: hex_encode(&nonce_bytes),
            ciphertext: hex_encode(&ct),
        };
        atomic_write(path, &serde_json::to_vec(&envelope)?)
    }

    fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SecretsError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let tmp = path.with_extension("tmp");
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            set_private_perms(&file)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn hex_decode(s: &str) -> Result<Vec<u8>, SecretsError> {
        if !s.len().is_multiple_of(2) {
            return Err(SecretsError::Crypto("odd-length hex".into()));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&s[i..i + 2], 16)
                    .map_err(|e| SecretsError::Crypto(format!("bad hex: {e}")))
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::vault::SecretVault;

        fn scratch(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-filevault-test-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn open_vault(dir: &Path) -> EncryptedFileVault {
            EncryptedFileVault::open(dir.join("secrets.json"), dir.join(".secrets.key")).unwrap()
        }

        /// A persist failure must not pollute the process-local cache: the
        /// old code inserted into the cache first, so `get` reported the
        /// change even though the call returned `Err`.
        #[test]
        fn failed_set_and_delete_leave_cache_unchanged() {
            let dir = scratch("atomic");
            let vault = open_vault(&dir);
            vault.set("KEEP", SecretValue::new("keep-value")).unwrap();
            // Block the atomic write: the tmp file path is a directory, so
            // the persist fails while the cache must stay untouched.
            std::fs::create_dir_all(dir.join("secrets.tmp")).unwrap();
            assert!(vault.set("NEW", SecretValue::new("new-value")).is_err());
            assert!(
                vault.get("NEW").unwrap().is_none(),
                "failed set must not cache"
            );
            assert!(
                vault.get("KEEP").unwrap().is_some(),
                "failed set must not disturb other entries"
            );
            assert!(vault.delete("KEEP").is_err());
            assert!(
                vault.get("KEEP").unwrap().is_some(),
                "failed delete must not drop the cached secret"
            );
            // Unblock: the vault is usable again, nothing stuck half-applied.
            std::fs::remove_dir_all(dir.join("secrets.tmp")).unwrap();
            vault.set("NEW", SecretValue::new("new-value")).unwrap();
            assert!(vault.get("NEW").unwrap().is_some());
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// An existing key file left group/world-readable is tightened to
        /// 0600 on load, not just when first created.
        #[test]
        #[cfg(unix)]
        fn existing_key_file_permissions_ratcheted_to_0600() {
            use std::os::unix::fs::PermissionsExt;
            let dir = scratch("keyperms");
            let key_path = dir.join(".secrets.key");
            std::fs::write(&key_path, [7u8; 32]).unwrap();
            std::fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
            let _vault = open_vault(&dir);
            let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "existing key file must be tightened to 0600");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod keychain {
    //! OS keychain vault (spec §13): the preferred durable backend.
    //!
    //! macOS Keychain Services, Windows Credential Manager, and - on Linux
    //! the freedesktop Secret Service (gnome-keyring, KWallet) all sit behind
    //! the `keyring` crate's platform-default store. This is the "OS
    //! keychain" the spec ranks above the encrypted local file; insert it
    //! into a broker ahead of [`crate::filevault::EncryptedFileVault`] and
    //! the environment.
    //!
    //! Contract notes:
    //! - `get`/`set` are one local round-trip to the store (a D-Bus call on
    //!   Linux, ~ms). A *locked* keyring is the one case that can block: the
    //!   Secret Service store prompts to unlock, and that prompt's
    //!   `Completed`-signal wait has no timeout of its own. Every operation is
    //!   therefore bounded by `KEYCHAIN_TIMEOUT`; on timeout the operation is
    //!   abandoned and the store surfaces as [`SecretsError::Backend`] - which
    //!   the broker skips - so the runtime never hangs on a modal prompt.
    //! - `names` is unsupported: the platform APIs store and look up
    //!   credentials but offer no enumeration through this interface. Keep
    //!   the authoritative name list wherever the references live.
    //! - Secrets cross into the store as one plain copy at the platform
    //!   boundary (unavoidable at the FFI/D-Bus edge); everything on this
    //!   side stays a zeroizing [`SecretValue`].
    //!
    //! Availability contract (the broker relies on it):
    //! - Host with **no store at all**: `get` reads as absence (`Ok(None)`),
    //!   `delete` as the no-op it would be, so a broker chain falls through
    //!   to the next vault; `set` errors - there is nowhere to write.
    //! - Host **with a store that cannot answer** (locked, D-Bus broken):
    //!   every op surfaces [`SecretsError::Backend`] - the secret may exist,
    //!   the store just couldn't reply. Callers such as
    //!   [`crate::SecretsBroker`] decide whether to fall through or fail.
    //!
    //! Headless machines without a credential store keep working: the broker
    //! falls through to the next vault, and [`KeychainVault::platform_available`]
    //! lets callers check first.

    use crate::error::{validate_name, SecretsError};
    use crate::value::SecretValue;
    use crate::vault::SecretVault;
    use keyring_core::{Entry as KrEntry, Error as KrError};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::Arc;
    use std::time::Duration;

    /// Probe names for [`KeychainVault::usable`]. Never hold a real secret:
    /// the probe only builds an entry handle, which is what forces the
    /// platform store to open the collection.
    const PROBE_SERVICE: &str = "pantheon-probe";
    const PROBE_USER: &str = "probe";
    /// Throwaway value written and immediately deleted by the probe. Not a
    /// secret; it exists only to prove the credential store answers.
    const PROBE_VALUE: &str = "pantheon-probe-value";

    /// How long a keychain operation may block before we give up and report the
    /// store as unavailable.
    ///
    /// The Secret Service store prompts to unlock a locked keyring, and
    /// `secret-service`'s `exec_prompt` waits for the unlock dialog's
    /// `Completed` signal with no timeout of its own - so an unanswered prompt
    /// would block the runtime forever (the `SecretVault::get` contract forbids
    /// that). This bound turns an unbounded wait into a [`SecretsError::Backend`],
    /// which the broker skips.
    const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

    /// Set once a keychain operation has timed out. The platform store is a
    /// process-wide singleton and its mutex is still held by the abandoned thread
    /// while the prompt is up, so every later operation would block on that
    /// mutex. Latching here makes later calls fail fast instead of hanging: the
    /// keychain is treated as unusable until the process restarts, and the broker
    /// falls through to the next durable vault.
    static KEYCHAIN_TIMED_OUT: AtomicBool = AtomicBool::new(false);

    /// The error a timed-out (or latched-off) keychain operation reports.
    fn timeout_err() -> SecretsError {
        SecretsError::Backend("os keychain: operation timed out".into())
    }

    /// Run one blocking keychain operation under [`KEYCHAIN_TIMEOUT`].
    ///
    /// The operation runs on a throwaway thread so the wait can be bounded; the
    /// outer `Err` is a timeout/latch, the inner `Result` is the platform's own
    /// answer (so callers can still distinguish `NoEntry` from a real failure).
    fn run_bounded<T>(
        op: impl FnOnce() -> Result<T, KrError> + Send + 'static,
    ) -> Result<Result<T, KrError>, SecretsError>
    where
        T: Send + 'static,
    {
        run_bounded_inner(KEYCHAIN_TIMEOUT, op)
    }

    /// [`run_bounded`] with an explicit timeout, for tests.
    fn run_bounded_inner<T>(
        timeout: Duration,
        op: impl FnOnce() -> Result<T, KrError> + Send + 'static,
    ) -> Result<Result<T, KrError>, SecretsError>
    where
        T: Send + 'static,
    {
        if KEYCHAIN_TIMED_OUT.load(Ordering::SeqCst) {
            return Err(timeout_err());
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(op());
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => Ok(result),
            Err(RecvTimeoutError::Timeout) => {
                KEYCHAIN_TIMED_OUT.store(true, Ordering::SeqCst);
                Err(timeout_err())
            }
            // The operation thread died without answering (panic). Treat it like
            // a timeout so the store is not retried into a poisoned mutex.
            Err(RecvTimeoutError::Disconnected) => {
                KEYCHAIN_TIMED_OUT.store(true, Ordering::SeqCst);
                Err(timeout_err())
            }
        }
    }

    /// Durable vault backed by the OS credential store.
    #[derive(Debug)]
    pub struct KeychainVault {
        service: String,
    }

    impl KeychainVault {
        /// Vault under the default `pantheon` service name.
        pub fn new() -> Self {
            Self::with_service("pantheon")
        }

        /// Vault under a custom service name (tests isolate under their own
        /// so cleanup never touches real credentials).
        pub fn with_service(service: impl Into<String>) -> Self {
            Self {
                service: service.into(),
            }
        }

        /// Is the platform credential store usable in this process?
        ///
        /// Initializes the store on first call (once per process, cached by
        /// the keyring crate). `Err` = no store here: headless box, container
        /// without a Secret Service, unsupported platform.
        pub fn platform_available() -> Result<(), SecretsError> {
            ensure_store()
        }

        /// Can the keychain actually store a credential right now?
        ///
        /// [`Self::platform_available`] only proves a store object could be
        /// constructed. On a headless Linux host a D-Bus session is often
        /// present while the `login` collection is not, so a real entry probe
        /// fails where `store_status` succeeded. Callers that are about to
        /// commit to the keychain (rather than merely report availability)
        /// must use this one.
        pub fn usable() -> Result<(), SecretsError> {
            ensure_store()?;
            // Building an entry is lazy on the Secret Service backend: the
            // handle is created but no D-Bus call happens until a credential
            // is read or written. So the probe has to actually round-trip a
            // value, otherwise it reports a usable keychain on a host whose
            // `login` collection does not exist. Each round-trip is bounded
            // (see `run_bounded`) so a locked keyring cannot hang the probe.
            let entry =
                Arc::new(KrEntry::new(PROBE_SERVICE, PROBE_USER).map_err(|e| backend_err(&e))?);
            run_bounded({
                let entry = Arc::clone(&entry);
                move || entry.set_password(PROBE_VALUE)
            })?
            .map_err(|e| backend_err(&e))?;
            let read_back = run_bounded({
                let entry = Arc::clone(&entry);
                move || entry.get_password()
            })?
            .map_err(|e| backend_err(&e))?;
            // Best-effort cleanup: a failed delete must not fail the probe.
            let _ = run_bounded({
                let entry = Arc::clone(&entry);
                move || entry.delete_credential()
            });
            if read_back != PROBE_VALUE {
                return Err(SecretsError::Backend(
                    "os keychain: probe value did not round-trip".into(),
                ));
            }
            Ok(())
        }

        fn entry(&self, name: &str) -> Result<KrEntry, SecretsError> {
            validate_name(name)?;
            ensure_store()?;
            KrEntry::new(&self.service, name).map_err(|e| backend_err(&e))
        }
    }

    /// Make sure some credential store is installed, then report the status.
    ///
    /// The first use in a process asks the `keyring` crate to build the
    /// platform-default store (a Secret Service connection on Linux, the
    /// Keychain on macOS, CredMan on Windows) and installs it. Once a store
    /// is already installed - including a test-injected one - this is a
    /// no-op, which is what makes the sample-store lib tests deterministic
    /// in the same process.
    fn ensure_store() -> Result<(), SecretsError> {
        if keyring_core::get_default_store().is_some() {
            return Ok(());
        }
        match keyring::Entry::store_status() {
            Ok(()) => Ok(()),
            Err(e) => Err(backend_err(e)),
        }
    }

    impl Default for KeychainVault {
        fn default() -> Self {
            Self::new()
        }
    }

    fn backend_err(e: &KrError) -> SecretsError {
        SecretsError::Backend(format!("os keychain: {e}"))
    }

    impl SecretVault for KeychainVault {
        fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
            validate_name(name)?;
            // No store on this host: this backend holds nothing under any
            // name - absence, so the broker chain falls through.
            // (A store that exists but cannot answer stays a loud Backend
            // error below: the secret may well be in there.)
            if ensure_store().is_err() {
                return Ok(None);
            }
            let entry = Arc::new(self.entry(name)?);
            match run_bounded(move || entry.get_password())? {
                Ok(v) => Ok(Some(SecretValue::new(v))),
                Err(KrError::NoEntry) => Ok(None),
                Err(e) => Err(backend_err(&e)),
            }
        }

        fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
            // One plain copy crosses into the store; our side stays zeroized.
            // (No store → `entry` reports Backend: nowhere to write.)
            let entry = Arc::new(self.entry(name)?);
            run_bounded(move || entry.set_password(value.expose()))?.map_err(|e| backend_err(&e))
        }

        fn delete(&self, name: &str) -> Result<(), SecretsError> {
            // Validate before touching the platform, exactly like get/set. A
            // store-less host returned Ok(()) for every name, empty ones
            // included, so `delete("")` only failed where a keyring happened to
            // exist - the test passed locally and failed on a CI runner with
            // one. A rejected name is rejected everywhere.
            validate_name(name)?;
            // Nothing can be stored where there is no store: deleting every
            // conceivable name is a chain of no-ops, contract and all.
            if ensure_store().is_err() {
                return Ok(());
            }
            let entry = Arc::new(self.entry(name)?);
            match run_bounded(move || entry.delete_credential())? {
                Ok(()) => Ok(()),
                // Trait contract: removing an unknown name is a no-op.
                Err(KrError::NoEntry) => Ok(()),
                Err(e) => Err(backend_err(&e)),
            }
        }

        fn names(&self) -> Result<Vec<String>, SecretsError> {
            // Enumeration gap, by platform design: the OS keychain APIs
            // expose lookup-by-name, not listing, so no broker list that
            // includes this vault is ever complete. The broker surfaces
            // this as a warning via `names_with_warnings` instead of
            // letting callers present a partial list as the whole store;
            // the only complete record of keychain-held names is the
            // operator's own tracking alongside the references.
            Err(SecretsError::Unsupported(
                "the OS keychain API cannot enumerate entries; track names \
                 alongside their references"
                    .into(),
            ))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The bound must turn the Secret Service's unbounded unlock-prompt wait
        /// into a `Backend` error (which the broker skips) and latch the store
        /// off, so a later call fails fast instead of blocking on the abandoned
        /// thread's process-wide mutex.
        #[test]
        fn a_blocking_operation_is_bounded_and_latches_the_store_off() {
            KEYCHAIN_TIMED_OUT.store(false, Ordering::SeqCst);

            // A fast operation passes the platform answer straight through.
            let fast = run_bounded_inner(Duration::from_secs(5), || {
                Ok::<_, KrError>("value".to_string())
            })
            .expect("fast op is not a timeout")
            .expect("op succeeded");
            assert_eq!(fast, "value");
            assert!(!KEYCHAIN_TIMED_OUT.load(Ordering::SeqCst));

            // An operation that never returns (the unanswered prompt) is cut off.
            let blocked = run_bounded_inner(Duration::from_millis(50), || {
                std::thread::sleep(Duration::from_secs(3600));
                Ok(())
            });
            assert!(
                matches!(blocked, Err(SecretsError::Backend(_))),
                "a blocked operation must report Backend, got {blocked:?}"
            );
            assert!(KEYCHAIN_TIMED_OUT.load(Ordering::SeqCst));

            // Latched: the next call fails fast rather than blocking.
            let latched = run_bounded_inner(Duration::from_secs(5), || Ok(()));
            assert!(matches!(latched, Err(SecretsError::Backend(_))));

            KEYCHAIN_TIMED_OUT.store(false, Ordering::SeqCst);
        }
    }
}

pub mod grants {
    //! Per-run credential grants for delegated child sessions.
    //!
    //! A `CredentialGrant` names exactly which secrets a child run may
    //! resolve and for which purpose. The scoped broker checks the grant
    //! before resolving and refuses ungranted names instead of falling
    //! through to unrestricted environment lookup. Grants are computed as
    //! intersections at each delegation level, never unions, and expire
    //! with the child run. Broker scoping alone does not stop a child with
    //! shell access from reading secret files directly; filesystem
    //! confinement is the separate half of the boundary.

    use crate::{SecretsBroker, SecretsError};

    /// Why a child run needs a credential.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum CredentialPurpose {
        Provider,
        Mcp,
        BrowserFill,
        Gateway,
    }

    /// One explicit grant for one child run.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CredentialGrant {
        pub run_id: String,
        pub name: String,
        pub purpose: CredentialPurpose,
    }

    impl CredentialGrant {
        pub fn new(
            run_id: impl Into<String>,
            name: impl Into<String>,
            purpose: CredentialPurpose,
        ) -> Self {
            Self {
                run_id: run_id.into(),
                name: name.into(),
                purpose,
            }
        }
    }

    /// Intersection of parent effective grants and child requested grants.
    /// Grandchildren therefore can only narrow, never widen.
    pub fn intersect_grants(
        parent: &[CredentialGrant],
        requested: &[CredentialGrant],
    ) -> Vec<CredentialGrant> {
        let mut out = Vec::new();
        for r in requested {
            if parent
                .iter()
                .any(|p| p.name == r.name && p.purpose == r.purpose)
                && !out
                    .iter()
                    .any(|g: &CredentialGrant| g.name == r.name && g.purpose == r.purpose)
            {
                out.push(r.clone());
            }
        }
        out
    }

    /// Broker wrapper that enforces a grant set. Un-granted names return
    /// `None` from `inject` and `SECRET_NOT_GRANTED` from `resolve`, never
    /// a fall through to the inner broker's environment.
    #[derive(Debug)]
    pub struct ScopedBroker {
        inner: SecretsBroker,
        grants: Vec<CredentialGrant>,
        run_id: String,
    }

    impl ScopedBroker {
        pub fn new(
            inner: SecretsBroker,
            run_id: impl Into<String>,
            grants: Vec<CredentialGrant>,
        ) -> Self {
            Self {
                inner,
                run_id: run_id.into(),
                grants,
            }
        }

        pub fn run_id(&self) -> &str {
            &self.run_id
        }

        pub fn grants(&self) -> &[CredentialGrant] {
            &self.grants
        }

        fn granted(&self, name: &str) -> bool {
            self.grants
                .iter()
                .any(|g| g.name == name && g.run_id == self.run_id)
        }

        pub fn inject(&self, name: &str) -> Result<Option<crate::SecretValue>, SecretsError> {
            if !self.granted(name) {
                return Ok(None);
            }
            self.inner.inject(name)
        }

        pub fn resolve(&self, name: &str) -> Result<Option<crate::SecretValue>, SecretsError> {
            if !self.granted(name) {
                return Err(SecretsError::Unsupported(format!(
                    "SECRET_NOT_GRANTED: {name} is not granted to run {}",
                    self.run_id
                )));
            }
            self.inner.resolve(name)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::SecretValue;

        fn broker_with(name: &str, value: &str) -> SecretsBroker {
            let mut b = SecretsBroker::new();
            let mem = crate::MemoryVault::new();
            let _ = mem.set(name, SecretValue::new(value));
            b = b.with_vault(Box::new(mem));
            b
        }

        #[test]
        fn scoped_broker_refuses_ungranted_name() {
            let inner = broker_with("PROVIDER_KEY", "secret");
            let scoped = ScopedBroker::new(inner, "run-1", vec![]);
            assert!(scoped.inject("PROVIDER_KEY").unwrap().is_none());
            let err = scoped.resolve("PROVIDER_KEY").unwrap_err();
            assert!(err.to_string().contains("SECRET_NOT_GRANTED"));
        }

        #[test]
        fn scoped_broker_allows_granted_name() {
            let inner = broker_with("PROVIDER_KEY", "secret");
            let scoped = ScopedBroker::new(
                inner,
                "run-1",
                vec![CredentialGrant::new(
                    "run-1",
                    "PROVIDER_KEY",
                    CredentialPurpose::Provider,
                )],
            );
            assert_eq!(
                scoped.inject("PROVIDER_KEY").unwrap().unwrap().expose(),
                "secret"
            );
        }

        #[test]
        fn intersect_narrows_not_widens() {
            let parent = vec![
                CredentialGrant::new("p", "A", CredentialPurpose::Provider),
                CredentialGrant::new("p", "B", CredentialPurpose::Provider),
            ];
            let requested = vec![
                CredentialGrant::new("c", "B", CredentialPurpose::Provider),
                CredentialGrant::new("c", "C", CredentialPurpose::Provider),
            ];
            let out = intersect_grants(&parent, &requested);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].name, "B");
        }
    }
}

pub mod redaction {
    //! Exact-value redaction registry.
    //!
    //! Complements pattern-based redaction (see `pantheon-api` logging): instead
    //! of guessing what a secret looks like from its shape, the registry records
    //! the exact bytes the broker handed out and scrubs those substrings from
    //! model-facing text. Registration happens at fill time, before the value can
    //! appear in any tool result, so a later echo can never reach the model.
    //!
    //! Registries are keyed by scope (a data dir or profile home) and shared
    //! process-wide per scope: a broker that registers and a session that scrubs
    //! stay in sync without plumbing handles between them. Scopes never share
    //! entries - scrubbing scope B's output with scope A's values would confirm
    //! to B that those bytes exist.
    //!
    //! Fail-open on a poisoned lock is a deliberate tradeoff, documented
    //! here rather than changed: `register` skips and `scrub` returns text
    //! unchanged when the per-scope lock is poisoned (a thread panicked
    //! while holding it). Failing the caller instead would break unrelated
    //! model-facing flows over a best-effort defence-in-depth layer -
    //! pattern-based redaction in `pantheon-api` logging still applies -
    //! and there is no safe state to recover: the guarded deque may be
    //! half-mutated. Poisoning is rare (it needs a panic mid-register /
    //! mid-scrub) and loud in logs via the panic itself.

    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, LazyLock, Mutex};

    /// Upper bound on registered values per scope. A fill-heavy session evicts
    /// its oldest entries instead of growing without limit. Eviction is
    /// loud (stderr): silently losing scrubbing for an early secret would
    /// leave that secret unscrubbed with no trace.
    const MAX_VALUES_PER_SCOPE: usize = 64;

    /// Minimum length of a registered secret. A 1-char "secret" would scrub
    /// benign text everywhere it appears; below this length the value is
    /// not distinctive enough to scrub safely.
    const MIN_SECRET_LEN: usize = 4;

    /// Placeholder substituted for a registered secret value.
    const REDACTED: &str = "[redacted-secret]";

    /// Process-wide scope-keyed registries. Insertions into the map are
    /// infallible, so a poisoned map lock is recovered rather than treated as
    /// fatal; the per-scope value lock below is where fail-open applies.
    static REGISTRIES: LazyLock<Mutex<HashMap<String, Arc<RedactionRegistry>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Thread-safe registry of exact secret values for one scope.
    #[derive(Debug)]
    pub struct RedactionRegistry {
        scope: String,
        values: Mutex<VecDeque<String>>,
    }

    impl RedactionRegistry {
        /// Registry for `scope`, shared process-wide: every caller asking for
        /// the same scope sees the same entries.
        pub fn for_scope(scope: &str) -> Arc<Self> {
            let mut map = REGISTRIES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.entry(scope.to_string())
                .or_insert_with(|| {
                    Arc::new(Self {
                        scope: scope.to_string(),
                        values: Mutex::new(VecDeque::new()),
                    })
                })
                .clone()
        }

        /// The scope this registry was created for.
        pub fn scope(&self) -> &str {
            &self.scope
        }

        /// Record an exact secret value for later scrubbing. Empty values
        /// are ignored: matching the empty string would hit everywhere.
        /// Values shorter than [`MIN_SECRET_LEN`] are ignored for the same
        /// reason at a smaller scale: a 1-char secret would scrub benign
        /// text wherever that character appears. Re-registering a value
        /// refreshes its recency so live secrets are evicted last. Fails
        /// open: a poisoned lock skips registration instead of failing the
        /// caller (see the module docs for the tradeoff).
        pub fn register(&self, value: &str) {
            if value.len() < MIN_SECRET_LEN {
                return;
            }
            let Ok(mut values) = self.values.lock() else {
                return; // poisoned: fail open, skip registration
            };
            if let Some(pos) = values.iter().position(|v| v == value) {
                values.remove(pos);
            }
            values.push_back(value.to_string());
            while values.len() > MAX_VALUES_PER_SCOPE {
                values.pop_front();
                // Loud, but never with the value: eviction means an early
                // secret is no longer scrubbed, and the operator should know.
                eprintln!(
                    "redaction: scope {} evicted its oldest registered secret (cap {MAX_VALUES_PER_SCOPE})",
                    self.scope
                );
            }
        }

        /// Replace every registered value in `text` with the placeholder.
        /// Longest values go first so a registered substring never shadows its
        /// registered superstring. Fails open: a poisoned lock returns `text`
        /// unchanged rather than failing the caller.
        pub fn scrub(&self, text: &str) -> String {
            let Ok(values) = self.values.lock() else {
                return text.to_string(); // poisoned: fail open, no scrub
            };
            if values.is_empty() {
                return text.to_string();
            }
            let mut ordered: Vec<&str> = values.iter().map(String::as_str).collect();
            ordered.sort_by_key(|a| std::cmp::Reverse(a.len()));
            // Substitute once, in a single left-to-right pass, never re-scanning
            // emitted output. `str::replace` cascades: with a value that overlaps
            // the placeholder (a registered "[redacted-secret]", or a short value
            // like "redact"), a later iteration rewrites bytes an earlier one just
            // inserted, producing `[redacted-[redacted-secret]]`. A single pass is
            // a fixed point by construction: scrubbing scrubbed text is a no-op.
            let mut out = String::with_capacity(text.len());
            let mut rest = text;
            'outer: while !rest.is_empty() {
                for value in &ordered {
                    if rest.starts_with(*value) {
                        out.push_str(REDACTED);
                        rest = &rest[value.len()..];
                        continue 'outer;
                    }
                }
                // No registered value starts here: copy one char (UTF-8 safe) and
                // advance.
                let ch = rest.chars().next().expect("non-empty");
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
            out
        }

        /// Number of values currently registered.
        #[cfg(test)]
        fn len(&self) -> usize {
            self.values.lock().map(|v| v.len()).unwrap_or(0)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn exact_value_is_scrubbed() {
            let reg = RedactionRegistry::for_scope("test-redact-exact");
            reg.register("alpha-bravo-123");
            assert_eq!(
                reg.scrub("leaked: alpha-bravo-123 end"),
                "leaked: [redacted-secret] end"
            );
            assert_eq!(
                reg.scrub("nothing sensitive here"),
                "nothing sensitive here"
            );
        }

        #[test]
        fn longest_value_wins_over_substring() {
            let reg = RedactionRegistry::for_scope("test-redact-longest");
            reg.register("abcd");
            reg.register("abcdef");
            // One placeholder for the whole superstring, not a nested mess.
            assert_eq!(reg.scrub("x abcdef y"), "x [redacted-secret] y");
            // The short value still scrubs on its own.
            assert_eq!(reg.scrub("x abcd y"), "x [redacted-secret] y");
        }

        /// Values below the minimum length are not registered at all: a
        /// 1-char secret would otherwise scrub benign text everywhere.
        #[test]
        fn short_values_are_not_registered() {
            let reg = RedactionRegistry::for_scope("test-redact-minlen");
            reg.register("x");
            reg.register("ab");
            reg.register("abc");
            assert_eq!(reg.len(), 0);
            assert_eq!(reg.scrub("x ab abc"), "x ab abc");
            reg.register("abcd");
            assert_eq!(reg.len(), 1);
            assert_eq!(reg.scrub("x abcd y"), "x [redacted-secret] y");
        }

        #[test]
        fn oldest_entries_evict_past_bound() {
            let reg = RedactionRegistry::for_scope("test-redact-evict");
            for i in 0..70 {
                reg.register(&format!("token-{i:03}"));
            }
            assert_eq!(reg.len(), MAX_VALUES_PER_SCOPE);
            // The first six registrations fell off the front.
            assert!(
                !reg.scrub("token-000").contains(REDACTED),
                "evicted value must not scrub"
            );
            assert!(
                reg.scrub("token-069").contains(REDACTED),
                "newest value must scrub"
            );
        }

        #[test]
        fn scopes_do_not_cross_scrub() {
            let a = RedactionRegistry::for_scope("test-redact-scope-a");
            let b = RedactionRegistry::for_scope("test-redact-scope-b");
            a.register("scope-a-only-secret");
            assert_eq!(
                b.scrub("scope-a-only-secret"),
                "scope-a-only-secret",
                "scope B must not scrub scope A's values"
            );
            assert!(
                a.scrub("scope-a-only-secret").contains(REDACTED),
                "scope A still scrubs its own values"
            );
        }

        #[test]
        fn empty_value_is_ignored() {
            let reg = RedactionRegistry::for_scope("test-redact-empty");
            reg.register("");
            assert_eq!(reg.len(), 0);
            assert_eq!(reg.scrub("anything"), "anything");
        }

        #[test]
        fn poisoned_lock_fails_open() {
            let reg = RedactionRegistry::for_scope("test-redact-poison");
            reg.register("s3cr3t-value");
            // Poison the value lock.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = reg.values.lock().unwrap();
                panic!("intentional poison for test");
            }));
            // Scrub degrades to a no-op instead of failing or panicking.
            assert_eq!(
                reg.scrub("has s3cr3t-value inside"),
                "has s3cr3t-value inside"
            );
            // Registration is a silent no-op too.
            reg.register("another-value");
        }
    }

    #[cfg(test)]
    mod fixed_point_tests {
        //! Scrubbing must be a fixed point: scrubbing an already-scrubbed string
        //! is a no-op. The old implementation chained `str::replace` per value, so
        //! a registered value overlapping the placeholder rewrote the placeholder
        //! into `[redacted-[redacted-secret]]`. That surfaced as a flaky
        //! assertion in `oauth::tests::redaction_registered_on_handout`, because
        //! every broker shares one process-wide registry and sibling tests were
        //! registering an overlapping value.
        use super::*;

        #[test]
        fn scrubbing_twice_equals_scrubbing_once() {
            let reg = RedactionRegistry::for_scope("test-fixed-point-twice");
            reg.register("super-secret-value");
            let once = reg.scrub("echo super-secret-value done");
            assert_eq!(once, "echo [redacted-secret] done");
            assert_eq!(
                reg.scrub(&once),
                once,
                "a second scrub must not rewrite the placeholder"
            );
        }

        /// A registered value that overlaps the placeholder itself is the exact
        /// case that produced the nested placeholder.
        #[test]
        fn a_value_overlapping_the_placeholder_does_not_nest() {
            let reg = RedactionRegistry::for_scope("test-fixed-point-overlap");
            reg.register("[redacted-secret]");
            let out = reg.scrub("[redacted-secret]");
            assert!(
                !out.contains("[redacted-[redacted-secret]]"),
                "placeholder must not nest: {out}"
            );
        }

        /// A short registered value that happens to be a substring of the
        /// placeholder ("redact") must not chew into it either.
        #[test]
        fn a_short_value_inside_the_placeholder_does_not_nest() {
            let reg = RedactionRegistry::for_scope("test-fixed-point-short");
            reg.register("redact");
            let out = reg.scrub("[redacted-secret]");
            assert!(
                !out.contains("[redacted-[redacted-secret]]"),
                "placeholder must not nest: {out}"
            );
        }

        #[test]
        fn ordinary_values_still_scrub_longest_first() {
            // Both values must exceed MIN_SECRET_LEN (4): anything shorter is
            // deliberately ignored by `register`, which would make this test pass
            // vacuously.
            let reg = RedactionRegistry::for_scope("test-fixed-point-longest");
            reg.register("abcdefgh");
            reg.register("abcd");
            // Longest-first: the whole "abcdefgh" becomes one placeholder, not two.
            assert_eq!(reg.scrub("abcdefgh"), "[redacted-secret]");
            assert_eq!(reg.scrub("abcd x"), "[redacted-secret] x");
        }
    }
}
