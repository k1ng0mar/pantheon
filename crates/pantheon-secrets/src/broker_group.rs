pub mod broker {
    //! Resolve order: durable vaults in insertion order, then env.
    //! A keyring that won't answer gets skipped, deeper failures propagate.
    //! `inject` returns None for an absent secret rather than erroring.

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
        // Arc so clone() keeps hitting the same store.
        durable: Vec<Arc<dyn SecretVault>>,
        env: EnvVault,
        // Empty = PATH only for plugin children.
        plugin_env_allowlist: Vec<String>,
        // inject() registers each value it hands out; the session scrubs
        // tool output against this.
        redaction: Arc<RedactionRegistry>,
        // Some = only these names resolve. Delegated child grants.
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
            // Clone shares the Arcs, so a clone resolves against the same stores.
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

        /// Broker over the real process env (`PANTHEON_SECRET_*`), with the OS
        /// keychain in front when this host has a usable one. `env:` resolves
        /// nothing until [`Self::with_env_allowlist`].
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

        /// `from_system_env` plus the legacy `PANTHEON_API_KEY` (and any
        /// `key_env`), injected as a memory vault ranked with the environment, so
        /// an exported rotation beats a stored one. Read-only mirror: `set` falls
        /// through to the keychain.
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

        /// Durable vault for hosts that may have no keychain (headless Linux):
        /// the OS keychain when usable, else an [`crate::filevault::EncryptedFileVault`]
        /// under `data_dir` (`secrets.json` sealed under `.secrets.key`, 0600).
        ///
        /// No memory fallback in the chain - that loses the secret on restart,
        /// which is the failure this constructor exists to prevent. Errors when
        /// the file vault won't open rather than degrading.
        ///
        /// Usability is a real entry probe, not `store_status`: a headless box can
        /// have a D-Bus session and no `login` collection, and trusting
        /// `store_status` there gave a broker a keychain that could only fail.
        pub fn durable(data_dir: &Path) -> Result<Self, SecretsError> {
            let keychain_usable = crate::keychain::KeychainVault::platform_available().is_ok()
                && crate::keychain::KeychainVault::usable().is_ok();
            Self::durable_with(data_dir, keychain_usable)
        }

        /// The selection half of [`Self::durable`], env probe split out. Tests
        /// pass `keychain_usable: false` so they don't touch a real keyring.
        ///
        /// Public because a per-request host needs an enumerable backend, and the
        /// keychain can't enumerate.
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

        /// Restrict `env:` to the allowlist (exact names or `PREFIX_*`). Empty by
        /// default, so `env:` reads fail closed.
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

        /// Scope the redaction registry - a data dir or profile home. Scopes
        /// don't cross-scrub: doing so would confirm to one scope that another's
        /// bytes exist.
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

        /// Durable vaults first, then the env. A vault whose store can't answer
        /// ([`SecretsError::Backend`]) is skipped; anything else propagates.
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

        /// [`resolve`], named for the point where a run's credentials are handed
        /// over. The value goes straight to the boundary - not into a log or model
        /// context.
        ///
        /// Each value is registered for redaction at handout, before it can appear
        /// in a tool result. Registration can't fail the injection.
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

        /// First durable vault that takes it. Errors when there isn't one.
        ///
        /// A vault that can't answer falls through - a locked keyring shouldn't
        /// block a write the file vault could take.
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

        /// Names across the durable vaults and the env, sorted, plus one warning
        /// per vault that failed to list. No values.
        ///
        /// Non-empty warnings mean the list is incomplete. The keychain can't
        /// enumerate at all (the platform APIs only look up by name), so any chain
        /// with a keychain in it omits those entries by design. The dashboard env
        /// page shows the warnings.
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

        /// [`names_with_warnings`](Self::names_with_warnings) with the warnings on
        /// stderr. Rendering a list to a user should use that one so they see them.
        pub fn names(&self) -> Vec<String> {
            let (names, warnings) = self.names_with_warnings();
            for w in warnings {
                eprintln!("secrets: {w}");
            }
            names
        }

        /// Delete from every vault that holds it. An unknown name is a no-op - the
        /// caller wanted it gone and it is. A vault that can't answer is skipped;
        /// anything else propagates, so an `Ok` really did delete.
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

        /// A `set` through the durable broker survives a full drop-and-recreate
        /// from the same data dir. Forced onto the file vault so the test doesn't
        /// write to the developer's keyring.
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
    //! One `SecretVault` = one backend: [`crate::env::EnvVault`],
    //! [`crate::filevault::EncryptedFileVault`], [`MemoryVault`],
    //! [`crate::keychain::KeychainVault`].

    use crate::error::SecretsError;
    use crate::value::SecretValue;
    use std::collections::HashMap;
    use std::sync::{Mutex, MutexGuard};

    /// Backend for secrets. Implementations guard state with a [`Mutex`] and take
    /// `&self`. `get` doesn't block on network or a prompt - it's on hot paths.
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
    /// The env mirror wins reads but doesn't absorb writes - a write into process
    /// memory dies with the process. `set`/`delete` return `Backend` so the broker
    /// falls through to the next durable vault, the same way it does for a locked
    /// keychain.
    #[derive(Debug)]
    pub struct ReadOnlyVault<V: SecretVault> {
        inner: V,
    }

    impl<V: SecretVault> ReadOnlyVault<V> {
        /// Wrap `inner`; reads and listings pass through, writes fail closed.
        pub fn new(inner: V) -> Self {
            Self { inner }
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
    //! Zeroizing secret value. The only type a secret travels as. Zeroizes on
    //! drop, `Debug` prints nothing, and `expose` is for the execution boundary.

    use std::fmt;
    use zeroize::Zeroizing;

    /// Scrubs its contents on drop. Cloning works but prefer moving. Logs and
    /// errors carry `Debug` or [`SecretValue::len`], not bytes.
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
    //! Encrypted local vault, the fallback when there's no OS keychain.
    //!
    //! AES-256-GCM under a 32-byte random key in a separate 0600 file. Payload is
    //! written temp+rename so a crash can't tear it, and a bit flip fails
    //! decryption instead of returning a wrong secret.
    //!
    //! The key sits beside the vault, so this protects against casual reads, not a
    //! compromised data dir. Moving the key elsewhere (keychain, separate mount)
    //! needs a migration for existing installs. Key file permissions are ratcheted
    //! to 0600 on every load, and a file that can't be tightened fails the open.
    //!
    //! Keychain first where there is one; this is the self-hosted fallback.

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
            // Persist, then swap the cache, so a persist failure leaves `get`
            // serving the previous value. The mutex isn't held across the write.
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
            // 0600 was creation-only before, so a key file left readable by
            // umask, a copy or a restore stayed that way. Ratchet on every load.
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
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(path)?;
        // Ratchet again for an existing file on platforms where the creation
        // mode is not honoured; the file is never group/world-readable before
        // the key bytes are written.
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
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp)?;
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

        /// A persist failure leaves the process-local cache alone, so `get` never
        /// reports a change the call returned `Err` for.
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
    //! OS keychain vault: the preferred durable backend. Keychain Services on
    //! macOS, CredMan on Windows, Secret Service on Linux (gnome-keyring,
    //! KWallet), all through the `keyring` crate.
    //!
    //! A locked keyring prompts to unlock, and that prompt's wait has no timeout
    //! of its own, so every operation is bounded by `KEYCHAIN_TIMEOUT`. On timeout
    //! the op is abandoned and surfaces as [`SecretsError::Backend`], which the
    //! broker skips rather than hanging the runtime on a modal dialog.
    //!
    //! `names` is unsupported - the platform APIs store and look up by name but
    //! don't enumerate. Keep the authoritative list wherever the references live.
    //!
    //! No store on the host: `get` is `Ok(None)` and `delete` is the no-op it would
    //! be, so the chain falls through; `set` errors since there's nowhere to write.
    //! A store that exists but can't answer is `Backend` on every op.
    //!
    //! [`KeychainVault::platform_available`] lets a caller check before committing.

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

    /// How long a keychain op may block. The Secret Service unlock prompt waits on
    /// its `Completed` signal with no timeout of its own, and `SecretVault::get`
    /// can't block forever. On expiry the op is `Backend`, which the broker skips.
    const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

    /// Set once a keychain op has timed out. The platform store is a process-wide
    /// singleton and the abandoned thread still holds its mutex while the prompt
    /// is up, so later calls would queue behind it. Latched, they fail fast: the
    /// keychain is unusable until the process restarts.
    static KEYCHAIN_TIMED_OUT: AtomicBool = AtomicBool::new(false);

    /// The error a timed-out (or latched-off) keychain operation reports.
    fn timeout_err() -> SecretsError {
        SecretsError::Backend("os keychain: operation timed out".into())
    }

    /// One blocking keychain op under [`KEYCHAIN_TIMEOUT`], on a throwaway thread.
    /// Outer `Err` is the timeout/latch; inner `Result` is the platform's own
    /// answer, so callers can still tell `NoEntry` from a real failure.
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

        /// Whether a store object can be built here. `Err` = headless box,
        /// container without a Secret Service, unsupported platform.
        pub fn platform_available() -> Result<(), SecretsError> {
            ensure_store()
        }

        /// Whether the keychain can actually store something right now.
        ///
        /// [`Self::platform_available`] only proves a store could be constructed.
        /// A headless Linux box often has a D-Bus session and no `login`
        /// collection. Anything about to commit to the keychain uses this one.
        pub fn usable() -> Result<(), SecretsError> {
            ensure_store()?;
            // Building an entry is lazy on Secret Service: the handle exists but
            // no D-Bus call happens until a value is read or written. So the probe
            // round-trips a value, otherwise it reports a working keychain on a
            // host whose collection doesn't exist. Each trip is bounded.
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

    /// Install the platform-default store if it isn't already, then report status.
    /// A no-op once a store is installed - including a test-injected one, which is
    /// why the sample-store tests are deterministic in one process.
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
            // No store here, so this backend holds nothing under any name -
            // absence, and the chain falls through. (A store that exists but
            // can't answer stays a loud Backend error below.)
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
            // Validate before touching the platform, like get/set: a rejected name
            // is rejected everywhere, store or no store.
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
            // The platform APIs look up by name and don't enumerate, so no list
            // that includes this vault is complete. The broker turns that into a
            // warning via `names_with_warnings`. The only complete record of
            // keychain names is the operator's own tracking.
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

        /// The bound has to turn the unlock-prompt wait into a `Backend` error
        /// (which the broker skips) and latch the store off, so a later call
        /// doesn't queue behind the abandoned thread's process-wide mutex.
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

pub mod redaction {
    //! Exact-value redaction. Pattern-based redaction guesses a secret's shape;
    //! this records the exact bytes the broker handed out and scrubs those
    //! substrings from model-facing text. Registration happens at fill time, so an
    //! echo later can't reach the model.
    //!
    //! Registries are per scope (a data dir or profile home), shared process-wide so
    //! a registering broker and a scrubbing session stay in sync without plumbing.
    //! Scopes don't share entries - that would confirm to one scope that another's
    //! bytes exist.
    //!
    //! A poisoned lock fails open: `register` skips and `scrub` returns the text
    //! unchanged. Poisoning needs a panic mid-register or mid-scrub, and the guarded
    //! deque may be half-mutated, so there's no state to recover to. Pattern-based
    //! redaction in `pantheon-api` logging still applies.

    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, LazyLock, Mutex};

    /// Cap on registered values per scope. A fill-heavy session evicts oldest
    /// rather than growing. Eviction goes to stderr: silently losing scrubbing for
    /// a secret would leave it unscrubbed with nothing in the log.
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

        /// Record a value for later scrubbing. Empty values are ignored (matching
        /// "" hits everywhere), and so is anything under [`MIN_SECRET_LEN`] - a
        /// 1-char secret would scrub that character out of benign text. Re-
        /// registering refreshes recency so live secrets evict last. A poisoned
        /// lock skips rather than failing the caller.
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

        /// Replace registered values with the placeholder, longest first so a
        /// registered substring can't shadow its superstring. A poisoned lock
        /// returns the text unchanged.
        pub fn scrub(&self, text: &str) -> String {
            let Ok(values) = self.values.lock() else {
                return text.to_string(); // poisoned: fail open, no scrub
            };
            if values.is_empty() {
                return text.to_string();
            }
            let mut ordered: Vec<&str> = values.iter().map(String::as_str).collect();
            ordered.sort_by_key(|a| std::cmp::Reverse(a.len()));
            // One left-to-right pass, never re-scanning output. `str::replace`
            // cascades: a value overlapping the placeholder (a registered
            // "[redacted-secret]", or a short one like "redact") rewrites bytes an
            // earlier iteration inserted, giving `[redacted-[redacted-secret]]`.
            // One pass is a fixed point by construction.
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
        //! Scrubbing a scrubbed string is a no-op. Chained `str::replace` broke that
        //! when a registered value overlapped the placeholder, which surfaced as a
        //! flaky assertion in `oauth::tests::redaction_registered_on_handout` since
        //! every broker shares one process-wide registry.
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
            // ignored by `register`, which would make this test pass vacuously.
            let reg = RedactionRegistry::for_scope("test-fixed-point-longest");
            reg.register("abcdefgh");
            reg.register("abcd");
            // Longest-first: the whole "abcdefgh" becomes one placeholder, not two.
            assert_eq!(reg.scrub("abcdefgh"), "[redacted-secret]");
            assert_eq!(reg.scrub("abcd x"), "[redacted-secret] x");
        }
    }
}
