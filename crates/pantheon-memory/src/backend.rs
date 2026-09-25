//! Memory backend registry. External backends (GalaxyMem, Mnemosyne,
//! Honcho, Hindsight, OpenViking, ...) implement `MemoryBackend` and
//! register here. The runtime session asks the registry for the active
//! backend by name, so the same memory tool surface works against the
//! native store or any plugin backend.
//!
//! Policy and provenance never leave Pantheon. Backends store and
//! recall data; callers gate through `recall_via` / `write_via` /
//! `confirm_via` BEFORE the backend sees anything. A backend that
//! silently overwrites records is a hostile backend.
//!
//! Selection is persisted as TOML at `<data_dir>/memory-backend.toml`
//! (`BackendSelection { name, options }`); `open_selected` reads it and
//! instantiates the chosen backend.
use crate::{MemoryBackend, MemoryStore};
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Memory,
        false,
        cause,
        "check backend registration",
        "",
    )
}

/// Catalog metadata for one backend. The registry builds the catalog
/// dynamically from the constructors passed at construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub name: String,
    pub label: String,
    pub kind: BackendKind,
    pub capabilities: Vec<String>,
}

/// Backend kind: what the runtime expects when it instantiates the
/// backend. The native store is built-in; HTTP backends talk to a
/// subprocess or remote service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendKind {
    Native,
    Http,
    Subprocess,
}

/// Selection of the active backend. Persisted as TOML so a session can
/// load it on startup. `native` is always present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSelection {
    pub name: String,
    #[serde(default)]
    pub options: HashMap<String, String>,
}

impl Default for BackendSelection {
    fn default() -> Self {
        Self {
            name: "native".into(),
            options: HashMap::new(),
        }
    }
}

/// Where the selection file lives inside a data dir.
pub fn selection_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("memory-backend.toml")
}

/// Load the persisted selection; missing/unreadable file => default
/// (native). Never fails: a broken selection must not brick a session,
/// `open_selected` falls back to native with an error line instead.
pub fn load_selection(data_dir: &Path) -> BackendSelection {
    std::fs::read_to_string(selection_path(data_dir))
        .ok()
        .and_then(|s| toml::from_str::<BackendSelection>(&s).ok())
        .unwrap_or_default()
}

/// Persist the selection (create dir as needed).
pub fn save_selection(data_dir: &Path, sel: &BackendSelection) -> Result<(), PantheonError> {
    std::fs::create_dir_all(data_dir)
        .and_then(|_| {
            std::fs::write(
                selection_path(data_dir),
                toml::to_string(sel).unwrap_or_default(),
            )
        })
        .map_err(|e| {
            merr(
                "MEM_SELECTION_WRITE",
                format!("writing {}: {e}", selection_path(data_dir).display()),
            )
        })
}

/// Instantiate the selected backend for a data dir.
///
/// - `native` (default): the persistent SQLite store at
///   `<data_dir>/memory.db` — same file sessions have always used.
/// - anything else: resolved through `BackendRegistry::with_defaults()`.
///   If instantiation fails (missing url, unreachable config), the error
///   is returned so callers can decide; `Session::new` logs and degrades
///   to no memory rather than pretending a backend is active.
pub fn open_selected(data_dir: &Path) -> Result<Arc<dyn MemoryBackend>, PantheonError> {
    let sel = load_selection(data_dir);
    if sel.name == "native" {
        let store = MemoryStore::open(&data_dir.join("memory.db")).map_err(|e| {
            merr(
                "MEM_NATIVE_OPEN",
                format!("opening {}: {e}", data_dir.join("memory.db").display()),
            )
        })?;
        return Ok(Arc::new(store));
    }
    let registry = BackendRegistry::with_plugins(data_dir);
    registry.instantiate_selected(&sel)
}

/// Registry: maps backend name to factory + info. Native and the HTTP
/// plugin backends are registered by default at construction.
pub struct BackendRegistry {
    entries: HashMap<String, BackendEntry>,
}

type Factory =
    Arc<dyn Fn(&BackendSelection) -> Result<Arc<dyn MemoryBackend>, PantheonError> + Send + Sync>;

struct BackendEntry {
    info: BackendInfo,
    factory: Factory,
}

impl std::fmt::Debug for BackendEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendEntry")
            .field("info", &self.info)
            .finish()
    }
}

/// Registered HTTP-bridge plugin backends: each talks to a service that
/// exposes (or is bridged to) Pantheon's small `/v1/memory` JSON API.
/// URL/key come from selection options (`url`, `key`) or env
/// `PANTHEON_MEMORY_<NAME>_URL` / `PANTHEON_MEMORY_<NAME>_KEY`.
const PLUGIN_BACKENDS: &[(&str, &str)] = &[
    ("galaxymem", "GalaxyMem via HTTP bridge (Pantheon /v1/memory protocol)"),
    ("mnemosyne", "Mnemosyne via HTTP bridge (Pantheon /v1/memory protocol)"),
    ("honcho", "Honcho via HTTP bridge (Pantheon /v1/memory protocol)"),
    ("hindsight", "Hindsight via HTTP bridge (Pantheon /v1/memory protocol)"),
    ("openviking", "OpenViking via HTTP bridge (Pantheon /v1/memory protocol)"),
];

fn bridge_factory(
    name: &'static str,
) -> impl Fn(&BackendSelection) -> Result<Arc<dyn MemoryBackend>, PantheonError> + Send + Sync + 'static
{
    move |sel: &BackendSelection| {
        let opt_url = sel.options.get("url").cloned();
        let env_url = format!("PANTHEON_MEMORY_{}_URL", name.to_uppercase());
        let base = match opt_url.or_else(|| std::env::var(&env_url).ok()) {
            Some(u) if !u.trim().is_empty() => u,
            _ => {
                return Err(merr(
                    "MEM_BACKEND_CONFIG",
                    format!(
                        "{name}: no url configured (set options.url in memory-backend.toml or ${env_url})"
                    ),
                ))
            }
        };
        let env_key = format!("PANTHEON_MEMORY_{}_KEY", name.to_uppercase());
        let key = sel.options.get("key").cloned().or_else(|| std::env::var(&env_key).ok());
        Ok(Arc::new(crate::http_backend::HttpBackend::new(base, key))
            as Arc<dyn MemoryBackend>)
    }
}

impl BackendRegistry {
    /// Build a registry with the native + http + plugin factories
    /// pre-registered. The native factory opens a fresh in-memory store
    /// on every call (tests); real sessions use `open_selected`, which
    /// opens the persistent store directly for `native`.
    pub fn with_defaults() -> Self {
        let mut r = Self {
            entries: HashMap::new(),
        };
        r.register_with(
            BackendInfo {
                name: "native".into(),
                label: "Pantheon native memory (SQLite + FTS5)".into(),
                kind: BackendKind::Native,
                capabilities: vec![
                    "memory.read".into(),
                    "memory.write".into(),
                    "memory.forget".into(),
                    "memory.md".into(),
                ],
            },
            |_| {
                Ok(Arc::new(MemoryStore::open_in_memory()?) as Arc<dyn MemoryBackend>)
            },
        );
        r.register_with(
            BackendInfo {
                name: "http".into(),
                label: "External memory backend over HTTP (Pantheon /v1/memory protocol)"
                    .into(),
                kind: BackendKind::Http,
                capabilities: vec!["memory.read".into(), "memory.write".into()],
            },
            |_| {
                let base = std::env::var("PANTHEON_MEMORY_HTTP_URL").map_err(|_| {
                    merr(
                        "MEM_HTTP_NO_URL",
                        "PANTHEON_MEMORY_HTTP_URL is not set".to_string(),
                    )
                })?;
                let key = std::env::var("PANTHEON_MEMORY_HTTP_KEY").ok();
                Ok(Arc::new(crate::http_backend::HttpBackend::new(base, key))
                    as Arc<dyn MemoryBackend>)
            },
        );
        for (name, label) in PLUGIN_BACKENDS {
            r.register_with(
                BackendInfo {
                    name: (*name).into(),
                    label: (*label).into(),
                    kind: BackendKind::Http,
                    capabilities: vec!["memory.read".into(), "memory.write".into()],
                },
                bridge_factory(name),
            );
        }
        r
    }

    /// Built-ins plus every user manifest in `<data_dir>/memory-plugins`.
    /// This is what sessions and the CLI use: dropping a TOML file is the
    /// whole installation story for a custom memory backend.
    pub fn with_plugins(data_dir: &Path) -> Self {
        let mut r = Self::with_defaults();
        let _loaded = crate::plugins::load_dir(&mut r, data_dir);
        r
    }

    /// Register a backend with an options-blind factory (legacy shape).
    pub fn register<F>(&mut self, info: BackendInfo, factory: F)
    where
        F: Fn() -> Result<Arc<dyn MemoryBackend>, PantheonError> + Send + Sync + 'static,
    {
        self.register_with(info, move |_| factory())
    }

    /// Register a backend whose factory receives the persisted selection
    /// (name + options). This is how a plugin reads `url`/`key` from
    /// `memory-backend.toml` without the crate owning config parsing.
    pub fn register_with<F>(&mut self, info: BackendInfo, factory: F)
    where
        F: Fn(&BackendSelection) -> Result<Arc<dyn MemoryBackend>, PantheonError>
            + Send
            + Sync
            + 'static,
    {
        let name = info.name.clone();
        self.entries.insert(
            name,
            BackendEntry {
                info,
                factory: Arc::new(factory),
            },
        );
    }

    /// All registered backends, sorted by name.
    pub fn list(&self) -> Vec<BackendInfo> {
        let mut v: Vec<BackendInfo> = self.entries.values().map(|e| e.info.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// One backend by name.
    pub fn info(&self, name: &str) -> Option<&BackendInfo> {
        self.entries.get(name).map(|e| &e.info)
    }

    /// Instantiate by name with no options (legacy shape; http reads env).
    pub fn instantiate(&self, name: &str) -> Result<Arc<dyn MemoryBackend>, PantheonError> {
        self.instantiate_selected(&BackendSelection {
            name: name.to_string(),
            options: HashMap::new(),
        })
    }

    /// Instantiate the backend named in `sel`, passing its options to the
    /// factory. The factory is responsible for any I/O.
    pub fn instantiate_selected(
        &self,
        sel: &BackendSelection,
    ) -> Result<Arc<dyn MemoryBackend>, PantheonError> {
        let e = self.entries.get(&sel.name).ok_or_else(|| {
            merr(
                "MEM_BACKEND_UNKNOWN",
                format!("no backend named '{}'", sel.name),
            )
        })?;
        (e.factory)(sel)
    }

    /// Has a backend with this name.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_native_http_and_plugin_backends() {
        let r = BackendRegistry::with_defaults();
        let names: Vec<String> = r.list().iter().map(|b| b.name.clone()).collect();
        for want in [
            "native",
            "http",
            "galaxymem",
            "mnemosyne",
            "honcho",
            "hindsight",
            "openviking",
        ] {
            assert!(names.contains(&want.to_string()), "missing {want}: {names:?}");
        }
        assert!(r.contains("native"));
        assert!(r.contains("http"));
    }

    #[test]
    fn register_and_instantiate_round_trips() {
        let mut r = BackendRegistry::with_defaults();
        r.register(
            BackendInfo {
                name: "fake".into(),
                label: "Fake backend".into(),
                kind: BackendKind::Http,
                capabilities: vec!["memory.read".into()],
            },
            || Ok(Arc::new(MemoryStore::open_in_memory()?) as Arc<dyn MemoryBackend>),
        );
        let info = r.info("fake").unwrap();
        assert_eq!(info.label, "Fake backend");
        let backend = r.instantiate("fake").unwrap();
        // It really is a working memory store.
        let _ = backend.list_agent("nyx").unwrap();
    }

    #[test]
    fn unknown_backend_returns_structured_error() {
        let r = BackendRegistry::with_defaults();
        let err = r.instantiate("nope").unwrap_err();
        assert_eq!(err.code, "MEM_BACKEND_UNKNOWN");
    }

    #[test]
    fn plugin_factory_reads_url_from_selection_options() {
        let r = BackendRegistry::with_defaults();
        let sel = BackendSelection {
            name: "honcho".into(),
            options: [("url".to_string(), "http://127.0.0.1:9/v1".to_string())]
                .into_iter()
                .collect(),
        };
        // Construction performs no I/O; only the URL check runs here.
        let backend = r.instantiate_selected(&sel).unwrap();
        // list_agent would need the service; expect a structured conn error,
        // not a panic — proves the adapter is wired.
        let err = backend.list_agent("nyx").unwrap_err();
        assert_eq!(err.code, "MEM_HTTP_CONN");
    }

    #[test]
    fn plugin_factory_without_url_is_a_config_error() {
        let r = BackendRegistry::with_defaults();
        let sel = BackendSelection {
            name: "hindsight".into(),
            options: HashMap::new(),
        };
        let err = r.instantiate_selected(&sel).unwrap_err();
        assert_eq!(err.code, "MEM_BACKEND_CONFIG");
        assert!(err.cause.contains("hindsight"), "{}", err.cause);
    }

    #[test]
    fn selection_round_trips_through_toml() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-sel-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_selection(&dir), BackendSelection::default());
        let sel = BackendSelection {
            name: "honcho".into(),
            options: [("url".to_string(), "http://x".to_string())]
                .into_iter()
                .collect(),
        };
        save_selection(&dir, &sel).unwrap();
        assert_eq!(load_selection(&dir), sel);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_selected_defaults_to_persistent_native() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-opensel-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let backend = open_selected(&dir).unwrap();
        // Persistent file exists after first write.
        let policy = pantheon_core::capability::Policy::coder_with_memory();
        let prov = crate::Provenance {
            source: "test".into(),
            origin: "user".into(),
            trust: pantheon_core::provenance::TrustTier::User,
            recorded_at_ms: 0,
        };
        crate::write_via(
            backend.as_ref(),
            &policy,
            crate::Proposal {
                layer: crate::LayerKind::Agent,
                namespace: "nyx".into(),
                key: "k".into(),
                value: "v".into(),
                provenance: prov,
            },
            4096,
        )
        .unwrap();
        assert!(dir.join("memory.db").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
