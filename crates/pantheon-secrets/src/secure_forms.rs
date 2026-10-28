//! Secure forms: masked secret entry and management on top of the broker.
//!
//! This is Pantheon's equivalent of a secure vault entry form. It adds no
//! second secret store: values live only in the broker's durable vaults,
//! exactly as before. What it adds is:
//!
//! * A metadata sidecar (`secure-forms.json` under the data dir) holding
//!   only names, scopes and last-updated timestamps. It never holds values,
//!   and it is written with 0600 permissions.
//! * A caller gate: only a live user session may save, rotate or delete.
//!   Delegated child agents and plugins are refused outright, so a
//!   compromised or over-eager delegate cannot harvest or replace keys.
//! * Verification on save: a harmless check (provider models list, bot
//!   `getMe`, or a local shape check) that reports success or failure
//!   without ever echoing the value, not even in an error string.
//! * Config references: which config paths name a secret, so rotation
//!   never requires hand-editing `config.toml` — config keeps naming the
//!   secret, and the broker resolves the current value.
//!
//! Failure is always closed: when the broker has no durable vault, or the
//! metadata file cannot be written, `save` returns an error and nothing
//! is written anywhere else. There is no fallback to `config.toml`, to a
//! log, or to the transcript.

use crate::broker::SecretsBroker;
use crate::error::SecretsError;
use crate::value::SecretValue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// What a secret is for. The scope picks the default verification check
/// and is shown in listings; it never affects where the value is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretScope {
    /// A model/provider API key (built-in or custom provider).
    Provider,
    /// A secret an MCP server needs (header token, env value).
    Mcp,
    /// The local browser vault key (`GSD_BROWSER_VAULT_KEY`).
    BrowserVault,
    /// A chat bot token (Telegram-style `getMe` verification).
    BotToken,
    /// Anything else the user wants to keep in the broker.
    Custom,
}

impl SecretScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            SecretScope::Provider => "provider",
            SecretScope::Mcp => "mcp",
            SecretScope::BrowserVault => "browser_vault",
            SecretScope::BotToken => "bot_token",
            SecretScope::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "provider" | "model" | "api_key" => Some(SecretScope::Provider),
            "mcp" => Some(SecretScope::Mcp),
            "browser_vault" | "browser" | "gsd" => Some(SecretScope::BrowserVault),
            "bot_token" | "telegram" | "bot" => Some(SecretScope::BotToken),
            "custom" | "other" => Some(SecretScope::Custom),
            _ => None,
        }
    }

    /// Scopes in the order a form should offer them.
    pub fn all() -> &'static [SecretScope] {
        &[
            SecretScope::Provider,
            SecretScope::Mcp,
            SecretScope::BrowserVault,
            SecretScope::BotToken,
            SecretScope::Custom,
        ]
    }
}

/// Who is asking. Only a live user session may manage secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretCaller {
    /// The user, in their own TUI or dashboard session.
    UserSession,
    /// A delegated child agent. Refused, always.
    Delegate,
    /// A plugin or extension. Refused, always.
    Plugin,
}

impl SecretCaller {
    fn check(self) -> Result<(), SecretsError> {
        match self {
            SecretCaller::UserSession => Ok(()),
            SecretCaller::Delegate => Err(SecretsError::Unsupported(
                "secret management is refused for delegated agents; only the user session may manage secrets".into(),
            )),
            SecretCaller::Plugin => Err(SecretsError::Unsupported(
                "secret management is refused for plugins; only the user session may manage secrets".into(),
            )),
        }
    }
}

/// Outcome of a verification check. Reasons never contain the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// A live or local check passed.
    Verified,
    /// A check ran and the provider rejected the value (status only).
    Failed(String),
    /// No check exists for this secret, or it could not run.
    Unavailable(String),
}

/// A verification check. Implementations must never put the value, or any
/// part of it, into an outcome, an error, or a log line.
pub trait Verifier {
    fn verify(&self, value: &SecretValue) -> VerifyOutcome;
}

/// Verifier for OpenAI-compatible provider keys: `GET {base_url}/models`
/// with a bearer token. Only the HTTP status is reported on failure.
pub struct ProviderModelsVerifier {
    pub base_url: String,
}

impl Verifier for ProviderModelsVerifier {
    fn verify(&self, value: &SecretValue) -> VerifyOutcome {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(15)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        match agent
            .get(&url)
            .header("Authorization", format!("Bearer {}", value.expose()))
            .call()
        {
            Ok(resp) if (200..300).contains(&resp.status().as_u16()) => VerifyOutcome::Verified,
            Ok(resp) => {
                VerifyOutcome::Failed(format!("provider returned HTTP {}", resp.status().as_u16()))
            }
            Err(_) => VerifyOutcome::Failed("provider could not be reached".to_string()),
        }
    }
}

/// Verifier for bot tokens: Telegram-style `getMe`. The token travels only
/// in the request path at the execution boundary and is never logged or
/// included in any outcome.
pub struct BotTokenVerifier;

impl Verifier for BotTokenVerifier {
    fn verify(&self, value: &SecretValue) -> VerifyOutcome {
        let url = format!("https://api.telegram.org/bot{}/getMe", value.expose());
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(15)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        match agent.get(&url).call() {
            Ok(resp) if (200..300).contains(&resp.status().as_u16()) => VerifyOutcome::Verified,
            Ok(resp) => {
                VerifyOutcome::Failed(format!("bot API returned HTTP {}", resp.status().as_u16()))
            }
            Err(_) => VerifyOutcome::Failed("bot API could not be reached".to_string()),
        }
    }
}

/// Local shape check only: non-empty, and at least `min_len` bytes.
/// Used for browser vault keys and MCP secrets, where no harmless live
/// endpoint exists. This proves shape, not validity.
pub struct LocalVerifier {
    pub min_len: usize,
}

impl Verifier for LocalVerifier {
    fn verify(&self, value: &SecretValue) -> VerifyOutcome {
        if value.is_empty() {
            VerifyOutcome::Failed("secret is empty".to_string())
        } else if value.len() < self.min_len {
            VerifyOutcome::Failed(format!(
                "secret is shorter than {} characters",
                self.min_len
            ))
        } else {
            VerifyOutcome::Verified
        }
    }
}

/// Default verifier for a scope. Provider and bot-token scopes need a
/// caller-supplied endpoint, so this returns `None` for them; callers pass
/// an explicit verifier instead.
pub fn default_verifier(scope: SecretScope) -> Option<Box<dyn Verifier>> {
    match scope {
        SecretScope::BrowserVault => Some(Box::new(LocalVerifier { min_len: 16 })),
        SecretScope::Mcp => Some(Box::new(LocalVerifier { min_len: 1 })),
        SecretScope::BotToken => Some(Box::new(BotTokenVerifier)),
        SecretScope::Provider | SecretScope::Custom => None,
    }
}

/// One listed secret: name, scope and last update. Never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretEntry {
    pub name: String,
    pub scope: SecretScope,
    /// Unix seconds of the last save/rotation, or 0 when unknown.
    pub updated_at: u64,
    /// Whether the broker currently resolves the name.
    pub present: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MetadataFile {
    entries: BTreeMap<String, MetadataEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MetadataEntry {
    scope: SecretScope,
    updated_at: u64,
}

/// Result of saving a secret: the value is stored, and the verification
/// outcome is reported separately so a failed check never blocks the
/// user's ability to retry or clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveOutcome {
    pub verification: VerifyOutcome,
}

/// The secure-forms store: broker for values, sidecar for metadata.
pub struct SecureFormStore<'a> {
    broker: &'a SecretsBroker,
    metadata_path: PathBuf,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_metadata(path: &Path) -> MetadataFile {
    let Ok(text) = std::fs::read_to_string(path) else {
        return MetadataFile::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn store_metadata(path: &Path, meta: &MetadataFile) -> Result<(), SecretsError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            SecretsError::Backend(format!(
                "secure-forms metadata dir could not be created: {e}"
            ))
        })?;
    }
    let text = serde_json::to_string_pretty(meta).map_err(|e| {
        SecretsError::Backend(format!(
            "secure-forms metadata could not be serialised: {e}"
        ))
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| {
        SecretsError::Backend(format!("secure-forms metadata could not be written: {e}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        SecretsError::Backend(format!("secure-forms metadata could not be stored: {e}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

impl<'a> SecureFormStore<'a> {
    /// Open against a broker and a data dir. The metadata file is created
    /// lazily on first save; opening never fails on a missing file.
    pub fn new(broker: &'a SecretsBroker, data_dir: &Path) -> Self {
        Self {
            broker,
            metadata_path: data_dir.join("secure-forms.json"),
        }
    }

    pub fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }

    /// Save a secret via the broker, then verify it. The value is stored
    /// before verification runs, so a failed check leaves the user with
    /// retry and clear options instead of losing the entry. On a broker
    /// failure nothing is recorded and the metadata file is untouched:
    /// fail closed, never fall back to config or a log.
    pub fn save(
        &self,
        caller: SecretCaller,
        name: &str,
        scope: SecretScope,
        value: SecretValue,
        verifier: Option<&dyn Verifier>,
    ) -> Result<SaveOutcome, SecretsError> {
        caller.check()?;
        if value.is_empty() {
            return Err(SecretsError::Unsupported(
                "secret value must not be empty".into(),
            ));
        }
        self.broker.set(name, value.clone())?;
        // Register for redaction so any later echo is scrubbed. Best
        // effort: a registration failure must not un-save the secret.
        let _ = self.broker.inject(name);
        let mut meta = load_metadata(&self.metadata_path);
        meta.entries.insert(
            name.to_string(),
            MetadataEntry {
                scope,
                updated_at: now_unix(),
            },
        );
        // Metadata failure after a broker write is still an error, and the
        // caller is told; the value itself is in the broker, not lost.
        store_metadata(&self.metadata_path, &meta)?;
        let verification = match verifier {
            Some(v) => v.verify(&value),
            None => match default_verifier(scope) {
                Some(v) => v.verify(&value),
                None => VerifyOutcome::Unavailable(
                    "no verification check for this secret type".to_string(),
                ),
            },
        };
        Ok(SaveOutcome { verification })
    }

    /// Replace the value of an existing secret. Rotating a name that does
    /// not exist is an error: use `save` for a new secret. Config entries
    /// keep naming the secret, so no config edit is needed.
    pub fn rotate(
        &self,
        caller: SecretCaller,
        name: &str,
        value: SecretValue,
        verifier: Option<&dyn Verifier>,
    ) -> Result<SaveOutcome, SecretsError> {
        caller.check()?;
        let exists = self.broker.resolve(name)?.is_some()
            || load_metadata(&self.metadata_path)
                .entries
                .contains_key(name);
        if !exists {
            return Err(SecretsError::Unsupported(format!(
                "no secret named {name}; save it first"
            )));
        }
        let scope = load_metadata(&self.metadata_path)
            .entries
            .get(name)
            .map(|e| e.scope)
            .unwrap_or(SecretScope::Custom);
        self.save(caller, name, scope, value, verifier)
    }

    /// Delete a secret from the broker and drop its metadata.
    pub fn delete(&self, caller: SecretCaller, name: &str) -> Result<(), SecretsError> {
        caller.check()?;
        self.broker.delete(name)?;
        let mut meta = load_metadata(&self.metadata_path);
        if meta.entries.remove(name).is_some() {
            store_metadata(&self.metadata_path, &meta)?;
        }
        Ok(())
    }

    /// List secrets by name, scope and last update. Values are never
    /// included. Names the broker holds but the sidecar has never seen
    /// (saved before secure forms, or by another path) appear with scope
    /// `custom` and unknown update time, so the list is never silently
    /// partial.
    pub fn list(&self) -> Vec<SecretEntry> {
        let meta = load_metadata(&self.metadata_path);
        let mut entries: BTreeMap<String, SecretEntry> = BTreeMap::new();
        for (name, m) in &meta.entries {
            let present = self.broker.resolve(name).ok().flatten().is_some();
            entries.insert(
                name.clone(),
                SecretEntry {
                    name: name.clone(),
                    scope: m.scope,
                    updated_at: m.updated_at,
                    present,
                },
            );
        }
        for name in self.broker.names() {
            entries.entry(name.clone()).or_insert(SecretEntry {
                name,
                scope: SecretScope::Custom,
                updated_at: 0,
                present: true,
            });
        }
        entries.into_values().collect()
    }

    /// Config paths that reference each secret name, from parsed TOML
    /// text. A path is recorded when a `key_env` / `api_key_env` leaf
    /// names the secret, or when a `{source, name}` table names it.
    /// Other string leaves (provider ids, model ids, titles) are not
    /// references. Unparsable config yields no references, never an error.
    pub fn references(config_text: &str) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let Ok(doc) = toml::from_str::<toml::Value>(config_text) else {
            return out;
        };
        fn walk(prefix: &str, v: &toml::Value, out: &mut BTreeMap<String, Vec<String>>) {
            match v {
                toml::Value::Table(t) => {
                    if let Some(name) = t
                        .get("name")
                        .and_then(|n| n.as_str())
                        .filter(|_| t.contains_key("source"))
                    {
                        out.entry(name.to_string())
                            .or_default()
                            .push(prefix.to_string());
                        return;
                    }
                    for (k, child) in t {
                        let path = if prefix.is_empty() {
                            k.clone()
                        } else {
                            format!("{prefix}.{k}")
                        };
                        walk(&path, child, out);
                    }
                }
                toml::Value::String(s) => {
                    // Only env-name leaves are references: key_env and
                    // api_key_env. A bare string leaf (provider id, model
                    // id, title) naming a word is not a secret reference.
                    let leaf = prefix.rsplit('.').next().unwrap_or(prefix);
                    if leaf == "key_env" || leaf == "api_key_env" {
                        out.entry(s.clone()).or_default().push(prefix.to_string());
                    }
                }
                toml::Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        walk(&format!("{prefix}[{i}]"), item, out);
                    }
                }
                _ => {}
            }
        }
        walk("", &doc, &mut out);
        // Only keep names that look like secret names; drop every other
        // string leaf (titles, model ids) so callers get references, not
        // a dump of the config.
        out.retain(|_, paths| !paths.is_empty());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::MemoryVault;

    struct OkVerifier;
    impl Verifier for OkVerifier {
        fn verify(&self, _value: &SecretValue) -> VerifyOutcome {
            VerifyOutcome::Verified
        }
    }

    struct FailVerifier;
    impl Verifier for FailVerifier {
        fn verify(&self, _value: &SecretValue) -> VerifyOutcome {
            VerifyOutcome::Failed("provider returned HTTP 401".to_string())
        }
    }

    fn broker() -> SecretsBroker {
        SecretsBroker::new().with_vault(Box::new(MemoryVault::new()))
    }

    fn store<'a>(broker: &'a SecretsBroker, dir: &Path) -> SecureFormStore<'a> {
        SecureFormStore::new(broker, dir)
    }

    #[test]
    fn save_round_trips_through_broker_and_lists_metadata_only() {
        let dir = std::env::temp_dir().join("pantheon-secure-forms-test-1");
        let _ = std::fs::remove_dir_all(&dir);
        let b = broker();
        let s = store(&b, &dir);
        let out = s
            .save(
                SecretCaller::UserSession,
                "TEST_PROVIDER_KEY",
                SecretScope::Provider,
                SecretValue::new("test-value-123"),
                Some(&OkVerifier),
            )
            .expect("save");
        assert_eq!(out.verification, VerifyOutcome::Verified);
        assert_eq!(
            b.resolve("TEST_PROVIDER_KEY").unwrap().unwrap().expose(),
            "test-value-123"
        );
        let listed = s.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "TEST_PROVIDER_KEY");
        assert_eq!(listed[0].scope, SecretScope::Provider);
        assert!(listed[0].updated_at > 0);
        assert!(listed[0].present);
        // Metadata sidecar holds names and scopes, never the value.
        let meta = std::fs::read_to_string(dir.join("secure-forms.json")).unwrap();
        assert!(!meta.contains("test-value-123"));
        assert!(meta.contains("TEST_PROVIDER_KEY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delegate_and_plugin_callers_are_refused() {
        let dir = std::env::temp_dir().join("pantheon-secure-forms-test-2");
        let _ = std::fs::remove_dir_all(&dir);
        let b = broker();
        let s = store(&b, &dir);
        for caller in [SecretCaller::Delegate, SecretCaller::Plugin] {
            let err = s
                .save(
                    caller,
                    "X",
                    SecretScope::Custom,
                    SecretValue::new("v"),
                    Some(&OkVerifier),
                )
                .unwrap_err();
            assert!(err.to_string().contains("refused"));
            assert!(s.delete(caller, "X").is_err());
        }
        assert!(b.resolve("X").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broker_failure_is_fail_closed_and_leaves_no_metadata() {
        // A broker with no durable vault cannot store anything.
        let b = SecretsBroker::new();
        let dir = std::env::temp_dir().join("pantheon-secure-forms-test-3");
        let _ = std::fs::remove_dir_all(&dir);
        let s = store(&b, &dir);
        let err = s
            .save(
                SecretCaller::UserSession,
                "X",
                SecretScope::Custom,
                SecretValue::new("distinct-test-value-999"),
                Some(&OkVerifier),
            )
            .unwrap_err();
        assert!(!dir.join("secure-forms.json").exists());
        assert!(!err.to_string().contains("distinct-test-value-999"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_requires_existing_and_updates_value_and_timestamp() {
        let dir = std::env::temp_dir().join("pantheon-secure-forms-test-4");
        let _ = std::fs::remove_dir_all(&dir);
        let b = broker();
        let s = store(&b, &dir);
        assert!(s
            .rotate(
                SecretCaller::UserSession,
                "NOPE",
                SecretValue::new("v"),
                Some(&OkVerifier)
            )
            .is_err());
        s.save(
            SecretCaller::UserSession,
            "ROT",
            SecretScope::Mcp,
            SecretValue::new("old"),
            Some(&OkVerifier),
        )
        .unwrap();
        let first = s.list()[0].updated_at;
        std::thread::sleep(std::time::Duration::from_secs(1));
        s.rotate(
            SecretCaller::UserSession,
            "ROT",
            SecretValue::new("new"),
            Some(&OkVerifier),
        )
        .unwrap();
        assert_eq!(b.resolve("ROT").unwrap().unwrap().expose(), "new");
        assert!(s.list()[0].updated_at >= first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_value_and_metadata() {
        let dir = std::env::temp_dir().join("pantheon-secure-forms-test-5");
        let _ = std::fs::remove_dir_all(&dir);
        let b = broker();
        let s = store(&b, &dir);
        s.save(
            SecretCaller::UserSession,
            "DEL",
            SecretScope::Custom,
            SecretValue::new("v"),
            Some(&OkVerifier),
        )
        .unwrap();
        s.delete(SecretCaller::UserSession, "DEL").unwrap();
        assert!(b.resolve("DEL").unwrap().is_none());
        assert!(s.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verification_failure_still_saves_and_never_echoes_value() {
        let dir = std::env::temp_dir().join("pantheon-secure-forms-test-6");
        let _ = std::fs::remove_dir_all(&dir);
        let b = broker();
        let s = store(&b, &dir);
        let out = s
            .save(
                SecretCaller::UserSession,
                "BAD",
                SecretScope::Provider,
                SecretValue::new("do-not-echo-this-value"),
                Some(&FailVerifier),
            )
            .unwrap();
        assert!(matches!(out.verification, VerifyOutcome::Failed(_)));
        let dbg = format!("{out:?}");
        assert!(!dbg.contains("do-not-echo-this-value"));
        assert!(b.resolve("BAD").unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_verifier_checks_shape_only() {
        assert_eq!(
            LocalVerifier { min_len: 16 }.verify(&SecretValue::new("short")),
            VerifyOutcome::Failed("secret is shorter than 16 characters".to_string())
        );
        assert_eq!(
            LocalVerifier { min_len: 16 }.verify(&SecretValue::new("long-enough-vault-key")),
            VerifyOutcome::Verified
        );
    }

    #[test]
    fn references_find_key_env_paths() {
        let refs = SecureFormStore::references(
            "[model]\nprovider = \"exoseed\"\nkey_env = \"EXO_KEY\"\n\n[custom_providers.nvidia]\nbase_url = \"https://x\"\nkey_env = \"NVIDIA_KEY\"\n",
        );
        assert_eq!(
            refs.get("EXO_KEY").unwrap(),
            &vec!["model.key_env".to_string()]
        );
        assert_eq!(
            refs.get("NVIDIA_KEY").unwrap(),
            &vec!["custom_providers.nvidia.key_env".to_string()]
        );
        assert!(refs.get("exoseed").is_none());
    }

    #[test]
    fn scope_parsing_accepts_common_names() {
        assert_eq!(SecretScope::parse("provider"), Some(SecretScope::Provider));
        assert_eq!(SecretScope::parse("gsd"), Some(SecretScope::BrowserVault));
        assert_eq!(SecretScope::parse("telegram"), Some(SecretScope::BotToken));
        assert_eq!(SecretScope::parse("nope"), None);
    }
}
