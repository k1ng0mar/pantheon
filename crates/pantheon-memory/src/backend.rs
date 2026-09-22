//! Memory backend registry. External backends (GalaxyMem, Mnemosyne,
//! Honcho, Hindsight, ...) implement `MemoryBackend` and register here.
//! The runtime session asks the registry for the active backend by
//! name, so the same memory tool surface works against the native store
//! or any plugin backend.
//!
//! Policy and provenance never leave Pantheon. Backends store and
//! recall data; the `MemoryBackend::write` impl must call back through
//! `propose_write` semantics (validation + provenance + capability
//! gate) or otherwise surface a structured refusal. A backend that
//! silently overwrites records is a hostile backend.
//!
//! The registry is in-memory. The runtime reads the active backend from
//! a small config file at session construction; this crate does not
//! own config.
use crate::{MemoryBackend, MemoryStore};
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// Registry: maps backend name to factory + info. Native is registered
/// by default at construction.
pub struct BackendRegistry {
    entries: HashMap<String, BackendEntry>,
}

struct BackendEntry {
    info: BackendInfo,
    factory: Arc<dyn Fn() -> Result<Arc<dyn MemoryBackend>, PantheonError> + Send + Sync>,
}

impl std::fmt::Debug for BackendEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendEntry")
            .field("info", &self.info)
            .finish()
    }
}

impl BackendRegistry {
    /// Build a registry with the native store factory pre-registered.
    /// The factory opens a fresh in-memory store on every call. For
    /// persistent native use, call `instantiate("native", path)`.
    pub fn with_defaults() -> Self {
        let mut r = Self {
            entries: HashMap::new(),
        };
        let info = BackendInfo {
            name: "native".into(),
            label: "Pantheon native memory (SQLite + FTS5)".into(),
            kind: BackendKind::Native,
            capabilities: vec![
                "memory.read".into(),
                "memory.write".into(),
                "memory.forget".into(),
                "memory.md".into(),
            ],
        };
        r.entries.insert(
            "native".into(),
            BackendEntry {
                info,
                factory: Arc::new(|| {
                    Ok(Arc::new(MemoryStore::open_in_memory()?) as Arc<dyn MemoryBackend>)
                }),
            },
        );
        let http_info = BackendInfo {
            name: "http".into(),
            label: "External memory backend over HTTP (GalaxyMem, Mnemosyne, Honcho, Hindsight)"
                .into(),
            kind: BackendKind::Http,
            capabilities: vec!["memory.read".into(), "memory.write".into()],
        };
        r.entries.insert(
            "http".into(),
            BackendEntry {
                info: http_info,
                factory: Arc::new(|| {
                    let base = std::env::var("PANTHEON_MEMORY_HTTP_URL").map_err(|_| {
                        merr(
                            "MEM_HTTP_NO_URL",
                            "PANTHEON_MEMORY_HTTP_URL is not set".to_string(),
                        )
                    })?;
                    let key = std::env::var("PANTHEON_MEMORY_HTTP_KEY").ok();
                    Ok(Arc::new(crate::http_backend::HttpBackend::new(base, key))
                        as Arc<dyn MemoryBackend>)
                }),
            },
        );
        r
    }

    /// Register a backend. Overwrites any existing entry with the same
    /// name; this is how a plugin system installs itself at runtime.
    pub fn register<F>(&mut self, info: BackendInfo, factory: F)
    where
        F: Fn() -> Result<Arc<dyn MemoryBackend>, PantheonError> + Send + Sync + 'static,
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

    /// Instantiate a backend by name. The factory is responsible for
    /// any I/O. For the native store, the default factory gives an
    /// in-memory store; callers that need a persistent store should
    /// call `MemoryStore::open` themselves and wrap with
    /// `MemoryBackendRegistry`.
    pub fn instantiate(&self, name: &str) -> Result<Arc<dyn MemoryBackend>, PantheonError> {
        let e = self
            .entries
            .get(name)
            .ok_or_else(|| merr("MEM_BACKEND_UNKNOWN", format!("no backend named '{name}'")))?;
        (e.factory)()
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
    fn defaults_include_native_and_http() {
        let r = BackendRegistry::with_defaults();
        let names: Vec<String> = r.list().iter().map(|b| b.name.clone()).collect();
        assert!(names.contains(&"native".to_string()), "names: {names:?}");
        assert!(names.contains(&"http".to_string()), "names: {names:?}");
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
}
