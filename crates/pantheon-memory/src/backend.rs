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
use pantheon_api::error::{Layer, PantheonError};
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
/// dynamically from the constructors passed at construction. The setup
/// wizard consumes `BackendRegistry::catalog()`; `auth` names the
/// credential the entry needs (env var or keyless), `deployment` says
/// where it runs, and `recommended` is reserved for a future named
/// default (false everywhere - native is the implicit default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub name: String,
    pub label: String,
    pub kind: BackendKind,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub recommended: bool,
    #[serde(default)]
    pub auth: String,
    #[serde(default)]
    pub deployment: DeploymentKind,
}

/// Where the backend runs. Orthogonal to [`BackendKind`], which says how
/// the runtime instantiates it (native store vs HTTP bridge vs
/// subprocess); this says where the service lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentKind {
    #[default]
    Local,
    Cloud,
    SelfHosted,
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
///   `<data_dir>/memory.db` - same file sessions have always used.
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

/// One HTTP-bridge plugin backend. `default_url` is `None` for every
/// registered entry: no vendor ships a service that speaks Pantheon's
/// `/v1/memory` protocol, so any vendor URL as a default would point the
/// protocol client at a native API that returns 404s or foreign JSON.
/// Users supply `options.url` (or the env var) pointing at their own
/// thin bridge (a user-operated shim translating the vendor API to
/// Pantheon's `/v1/memory` protocol). Keeping the default empty makes a
/// missing bridge a clear `MEM_BACKEND_CONFIG` error instead of a
/// confusing protocol failure.
struct PluginSpec {
    name: &'static str,
    label: &'static str,
    default_url: Option<&'static str>,
    auth: &'static str,
    deployment: DeploymentKind,
}

/// Registered HTTP-bridge plugin backends: each talks to a service that
/// exposes (or is bridged to) Pantheon's small `/v1/memory` JSON API.
/// URL/key come from selection options (`url`, `key`) or env
/// `PANTHEON_MEMORY_<NAME>_URL` / `PANTHEON_MEMORY_<NAME>_KEY` (dashes in
/// the name become underscores). Nothing ships a working bridge: no
/// entry carries a default URL, so a backend with no configured `url`
/// fails closed with `MEM_BACKEND_CONFIG` naming the missing piece and
/// what a working setup needs (a thin bridge exposing Pantheon's
/// `/v1/memory` protocol, or a future native adapter).
///
/// Accuracy notes (docs-verified 2026-09-29, browser search against each
/// vendor's current official docs; none of these backends has ever been
/// live-tested against the real service, so every entry is
/// still-unverifiable at the wire level):
/// - Hindsight Cloud is real (api.hindsight.vectorize.io, `hsk_` tokens),
///   as is `pip install hindsight-api` serving :8888; neither speaks
///   Pantheon's `/v1/memory` protocol. Old defaults pointing the
///   protocol client at those hosts were broken-by-design and removed.
/// - Honcho Cloud is real (api.honcho.dev, `HONCHO_API_KEY`, Bearer) and
///   Honcho self-hosts from a Docker Compose stack on :8000 (often
///   keyless unless a config key is set); neither speaks Pantheon's
///   `/v1/memory` protocol. Old defaults removed for the same reason.
/// - Supermemory Cloud is real (api.supermemory.ai, Bearer, `sm_` keys)
///   and `npx supermemory local` is real (:6767, prints an `sm_...` API
///   key on first boot - local access is NOT keyless). Neither speaks
///   Pantheon's `/v1/memory` protocol. Old defaults removed.
/// - Mem0 Cloud is real (api.mem0.ai, `m0-` keys; Mem0's own API uses
///   `Authorization: Token <key>`, not Bearer). Old default removed.
/// - OpenViking self-hosts on :1933 with `root_api_key` in `ov.conf`
///   (sent as `X-API-Key` by its own clients, not Bearer); its REST does
///   not speak Pantheon's `/v1/memory` protocol. Old default removed.
/// - GalaxyMem (k1ng0mar/galaxymem) is a local-first SQLite memory
///   engine (Hermes plugin or standalone library); it has no plain REST
///   surface at all - a thin bridge is the only path.
/// - ByteRover is `brv` CLI/MCP-shaped (no plain REST); Mnemosyne is
///   MCP-shaped (stdio/SSE/streamable HTTP, no REST); mnemopi is omp's
///   local SQLite engine (stdio MCP); sharpshooter is omp's
///   file-based project decision memory (no HTTP surface at all).
const PLUGIN_SPECS: &[PluginSpec] = &[
    PluginSpec {
        name: "byterover",
        label: "ByteRover via local thin bridge (brv is CLI/MCP-shaped: no plain REST API - point `url` at a bridge that fronts `brv query`/`brv curate`)",
        default_url: None,
        auth: "via the bridge; local brv is keyless (`brv login` is only for cloud sync)",
        deployment: DeploymentKind::Local,
    },
    PluginSpec {
        name: "galaxymem",
        label: "GalaxyMem via HTTP bridge (needs a thin bridge exposing Pantheon's /v1/memory protocol; GalaxyMem itself is a local SQLite engine with no plain REST)",
        default_url: None,
        auth: "PANTHEON_MEMORY_GALAXYMEM_KEY if the bridge requires auth",
        deployment: DeploymentKind::Local,
    },
    PluginSpec {
        name: "hindsight-cloud",
        label: "Hindsight Cloud via a thin bridge - point `url` at a bridge exposing Pantheon's /v1/memory protocol; Hindsight's own REST (api.hindsight.vectorize.io) does not speak it",
        default_url: None,
        auth: "hsk_ API token via PANTHEON_MEMORY_HINDSIGHT_CLOUD_KEY",
        deployment: DeploymentKind::Cloud,
    },
    PluginSpec {
        name: "hindsight-local",
        label: "Hindsight (local) via a thin bridge - point `url` at a bridge exposing Pantheon's /v1/memory protocol; the `hindsight-api` server's own REST (:8888) does not speak it",
        default_url: None,
        auth: "none on loopback; an LLM provider key or local model is needed for extraction",
        deployment: DeploymentKind::Local,
    },
    PluginSpec {
        name: "honcho-cloud",
        label: "Honcho Cloud via a thin bridge - point `url` at a bridge exposing Pantheon's /v1/memory protocol; Honcho's own API (api.honcho.dev) does not speak it",
        default_url: None,
        auth: "HONCHO_API_KEY via PANTHEON_MEMORY_HONCHO_CLOUD_KEY (Bearer)",
        deployment: DeploymentKind::Cloud,
    },
    PluginSpec {
        name: "honcho-local",
        label: "Honcho (self-host) via a thin bridge - point `url` at a bridge exposing Pantheon's /v1/memory protocol; the Docker Compose stack's own API (:8000) does not speak it",
        default_url: None,
        auth: "optional: self-set config key via PANTHEON_MEMORY_HONCHO_LOCAL_KEY (self-hosted Honcho is often keyless)",
        deployment: DeploymentKind::SelfHosted,
    },
    PluginSpec {
        name: "mem0",
        label: "Mem0 via HTTP bridge - cloud memory API (needs a thin bridge to Pantheon's /v1/memory protocol; api.mem0.ai does not speak it)",
        default_url: None,
        auth: "m0- API key via PANTHEON_MEMORY_MEM0_KEY (Mem0's own API uses `Authorization: Token <key>`)",
        deployment: DeploymentKind::Cloud,
    },
    PluginSpec {
        name: "mnemopi",
        label: "OMP local SQLite memory via HTTP bridge (needs a thin bridge)",
        default_url: None,
        auth: "as the OMP sidecar requires",
        deployment: DeploymentKind::Local,
    },
    PluginSpec {
        name: "mnemosyne",
        label: "Mnemosyne (local-only) via HTTP bridge - target its MCP streamable-HTTP endpoint or a thin bridge; no plain REST API",
        default_url: None,
        auth: "none (local loopback; embeddings may need OPENAI_API_KEY or a local profile)",
        deployment: DeploymentKind::Local,
    },
    PluginSpec {
        name: "openviking",
        label: "OpenViking (self-host only) via a thin bridge - point `url` at a bridge exposing Pantheon's /v1/memory protocol; OpenViking's own REST (:1933) does not speak it",
        default_url: None,
        auth: "root_api_key from ov.conf via PANTHEON_MEMORY_OPENVKING_KEY (sent to the bridge as Bearer; OpenViking itself uses X-API-Key)",
        deployment: DeploymentKind::SelfHosted,
    },
    PluginSpec {
        name: "sharpshooter",
        label: "OMP project decision memory via HTTP bridge (needs a thin bridge)",
        default_url: None,
        auth: "as the OMP sidecar requires",
        deployment: DeploymentKind::Local,
    },
    PluginSpec {
        name: "supermemory-cloud",
        label: "Supermemory Cloud via a thin bridge - point `url` at a bridge exposing Pantheon's /v1/memory protocol; Supermemory's own API (api.supermemory.ai) does not speak it",
        default_url: None,
        auth: "API key (sm_...) via PANTHEON_MEMORY_SUPERMEMORY_CLOUD_KEY (Bearer)",
        deployment: DeploymentKind::Cloud,
    },
    PluginSpec {
        name: "supermemory-local",
        label: "Supermemory (local binary) via a thin bridge - point `url` at a bridge exposing Pantheon's /v1/memory protocol; the local server's own API (:6767) does not speak it",
        default_url: None,
        auth: "API key printed on first boot (sm_...) via PANTHEON_MEMORY_SUPERMEMORY_LOCAL_KEY",
        deployment: DeploymentKind::Local,
    },
];

fn bridge_factory(
    spec: &'static PluginSpec,
) -> impl Fn(&BackendSelection) -> Result<Arc<dyn MemoryBackend>, PantheonError> + Send + Sync + 'static
{
    move |sel: &BackendSelection| {
        let prefix = spec.name.to_uppercase().replace('-', "_");
        let env_url = format!("PANTHEON_MEMORY_{prefix}_URL");
        let base = sel
            .options
            .get("url")
            .cloned()
            .filter(|u| !u.trim().is_empty())
            .or_else(|| {
                std::env::var(&env_url)
                    .ok()
                    .filter(|u| !u.trim().is_empty())
            })
            .or_else(|| spec.default_url.map(str::to_string));
        let base = match base {
            Some(u) => u,
            _ => {
                return Err(merr(
                    "MEM_BACKEND_CONFIG",
                    format!(
                        "{}: no url configured - point it at a thin bridge exposing Pantheon's /v1/memory protocol (or a future native adapter); set options.url in memory-backend.toml or ${env_url}",
                        spec.name,
                    ),
                ))
            }
        };
        let env_key = format!("PANTHEON_MEMORY_{prefix}_KEY");
        let key = sel
            .options
            .get("key")
            .cloned()
            .or_else(|| std::env::var(&env_key).ok());
        Ok(Arc::new(crate::http_backend::HttpBackend::new(base, key)) as Arc<dyn MemoryBackend>)
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
                recommended: false,
                auth: "none (local file)".into(),
                deployment: DeploymentKind::Local,
            },
            |_| Ok(Arc::new(MemoryStore::open_in_memory()?) as Arc<dyn MemoryBackend>),
        );
        r.register_with(
            BackendInfo {
                name: "http".into(),
                label: "External memory backend over HTTP (Pantheon /v1/memory protocol)".into(),
                kind: BackendKind::Http,
                capabilities: vec!["memory.read".into(), "memory.write".into()],
                recommended: false,
                auth: "PANTHEON_MEMORY_HTTP_KEY if the service requires auth".into(),
                deployment: DeploymentKind::SelfHosted,
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
        for spec in PLUGIN_SPECS {
            r.register_with(
                BackendInfo {
                    name: spec.name.into(),
                    label: spec.label.into(),
                    kind: BackendKind::Http,
                    capabilities: vec!["memory.read".into(), "memory.write".into()],
                    recommended: false,
                    auth: spec.auth.into(),
                    deployment: spec.deployment,
                },
                bridge_factory(spec),
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

    /// The setup-wizard catalog: every registered backend, native first,
    /// then alphabetical by name. Native stays the implicit default
    /// no entry is marked `recommended`.
    pub fn catalog(&self) -> Vec<BackendInfo> {
        let mut v = self.list();
        v.sort_by(|a, b| {
            (a.name != "native")
                .cmp(&(b.name != "native"))
                .then_with(|| a.name.cmp(&b.name))
        });
        v
    }

    /// Old backend ids that were split into local/cloud variants.
    /// `hindsight` and `honcho` resolve to their cloud variants, the
    /// defaults the old ids pointed at.
    pub fn resolve_alias(name: &str) -> &str {
        match name {
            "hindsight" => "hindsight-cloud",
            "honcho" => "honcho-cloud",
            other => other,
        }
    }

    /// One backend by name (aliases resolve first).
    pub fn info(&self, name: &str) -> Option<&BackendInfo> {
        self.entries.get(Self::resolve_alias(name)).map(|e| &e.info)
    }

    /// Instantiate by name with no options (legacy shape; http reads env).
    pub fn instantiate(&self, name: &str) -> Result<Arc<dyn MemoryBackend>, PantheonError> {
        self.instantiate_selected(&BackendSelection {
            name: Self::resolve_alias(name).to_string(),
            options: HashMap::new(),
        })
    }

    /// Instantiate the backend named in `sel`, passing its options to the
    /// factory. The factory is responsible for any I/O.
    pub fn instantiate_selected(
        &self,
        sel: &BackendSelection,
    ) -> Result<Arc<dyn MemoryBackend>, PantheonError> {
        let name = Self::resolve_alias(&sel.name);
        let e = self.entries.get(name).ok_or_else(|| {
            merr(
                "MEM_BACKEND_UNKNOWN",
                format!("no backend named '{}'", sel.name),
            )
        })?;
        let sel = BackendSelection {
            name: name.to_string(),
            options: sel.options.clone(),
        };
        (e.factory)(&sel)
    }

    /// Has a backend with this name (aliases resolve first).
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(Self::resolve_alias(name))
    }
}
