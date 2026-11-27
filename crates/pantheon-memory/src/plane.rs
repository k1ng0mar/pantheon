pub mod backend {
    //! Memory backend registry. External backends (GalaxyMem, Mnemosyne, Honcho,
    //! Hindsight, OpenViking, ...) implement `MemoryBackend` and register here, so
    //! the memory tool surface works the same against native or a plugin.
    //!
    //! Policy and provenance stay in Pantheon: callers gate through `recall_via` /
    //! `write_via` / `confirm_via` before the backend sees anything.
    //!
    //! Selection persists as TOML at `<data_dir>/memory-backend.toml`.
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

    /// Catalog metadata for one backend, for the setup wizard's
    /// `BackendRegistry::catalog()`. `auth` names the credential the entry needs,
    /// `deployment` where it runs, `recommended` is reserved (native is the
    /// implicit default).
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

    /// Option names that carry credentials. Never persisted to
    /// `memory-backend.toml` and never read back from it - that file is plain TOML
    /// in the data dir, so a key written there is a plaintext leak. Keys come from
    /// `PANTHEON_MEMORY_<NAME>_KEY` or the Secure Vault.
    const CREDENTIAL_OPTION_KEYS: &[&str] = &[
        "key",
        "api_key",
        "apikey",
        "token",
        "auth_token",
        "secret",
        "password",
    ];

    /// A copy of `sel` with credential options removed.
    fn strip_credential_options(sel: &BackendSelection) -> BackendSelection {
        let mut out = sel.clone();
        for k in CREDENTIAL_OPTION_KEYS {
            out.options.remove(*k);
        }
        out
    }

    /// Warn (loudly, on stderr) when a selection still carries credential
    /// options, e.g. one written by an older build. The value is never
    /// printed; only the file path and the env-var alternative are named.
    fn warn_credential_options(sel: &BackendSelection, path: &Path) {
        let present: Vec<&str> = CREDENTIAL_OPTION_KEYS
            .iter()
            .copied()
            .filter(|k| sel.options.contains_key(*k))
            .collect();
        if !present.is_empty() {
            eprintln!(
                "pantheon: WARNING: {} contains credential option(s) ({}) - \
             keys are no longer read from this file and are ignored. \
             Move the key to the PANTHEON_MEMORY_<NAME>_KEY environment \
             variable (or the Secure Vault) and remove it from the file; \
             the next save will drop it.",
                path.display(),
                present.join(", "),
            );
        }
    }

    /// Load the persisted selection. Missing file is the default (native) and
    /// never fails - a broken selection shouldn't brick a session. A corrupt file
    /// also falls back, but `load_selection_result` is the form that surfaces it.
    pub fn load_selection(data_dir: &Path) -> BackendSelection {
        load_selection_result(data_dir).unwrap_or_default()
    }

    /// As above, but distinguishes "no file" (default) from "present and
    /// unparseable" (error naming the file). Callers that degrade must surface
    /// it - quietly reading a different store hides a corrupt config.
    pub fn load_selection_result(data_dir: &Path) -> Result<BackendSelection, PantheonError> {
        let path = selection_path(data_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Ok(BackendSelection::default()),
        };
        let sel: BackendSelection = toml::from_str(&text).map_err(|e| {
            merr(
                "MEM_SELECTION_PARSE",
                format!(
                    "{} is not valid TOML ({}); ignoring it and using native",
                    path.display(),
                    e
                ),
            )
        })?;
        warn_credential_options(&sel, &path);
        Ok(strip_credential_options(&sel))
    }

    /// Persist the selection (create dir as needed). Credential options
    /// are stripped before writing: a key passed in-memory is never written
    /// to the file, even if the caller put it in `options`.
    pub fn save_selection(data_dir: &Path, sel: &BackendSelection) -> Result<(), PantheonError> {
        let sel = strip_credential_options(sel);
        std::fs::create_dir_all(data_dir)
            .and_then(|_| {
                std::fs::write(
                    selection_path(data_dir),
                    toml::to_string(&sel).unwrap_or_default(),
                )
            })
            .map_err(|e| {
                merr(
                    "MEM_SELECTION_WRITE",
                    format!("writing {}: {e}", selection_path(data_dir).display()),
                )
            })
    }

    /// Sync the persisted selection from `config.toml [memory]`, which wins on
    /// load, and write the resolved value back so the two don't drift. Returns the
    /// warning to log when a conflict was resolved or the sync failed.
    ///
    /// Empty config backend means no opinion and the selection file wins
    /// untouched. Config options win when non-empty; when the section carries none,
    /// the file's options are kept, so a switch by name alone doesn't wipe the
    /// endpoint. A corrupt selection file is left alone - it still errors at open,
    /// and overwriting it destroys the evidence. Credential options are stripped.
    pub fn sync_selection_from_config(
        data_dir: &Path,
        backend: &str,
        options: &HashMap<String, String>,
    ) -> Option<String> {
        let backend = backend.trim();
        if backend.is_empty() {
            return None;
        }
        let current = match load_selection_result(data_dir) {
            Ok(sel) => sel,
            Err(_) => return None,
        };
        let resolved = BackendSelection {
            name: backend.to_string(),
            options: if options.is_empty() {
                current.options.clone()
            } else {
                options.clone()
            },
        };
        if resolved == current {
            return None;
        }
        let warning = format!(
        "pantheon: WARNING: [memory] backend {backend:?} overrides the memory-backend.toml selection {:?}; syncing the selection file to {backend:?} (options {})",
        current.name,
        if options.is_empty() {
            "kept from the selection file".to_string()
        } else {
            "taken from [memory]".to_string()
        },
    );
        if let Err(e) = save_selection(data_dir, &resolved) {
            return Some(format!(
                "{warning}; sync failed ({e}), the files still disagree"
            ));
        }
        Some(warning)
    }

    /// Instantiate the selected backend. `native` is the SQLite store at
    /// `<data_dir>/memory.db`; anything else resolves through
    /// `BackendRegistry::with_defaults()` and errors if it can't (missing url,
    /// unreachable config) so the caller decides. A corrupt selection file is
    /// `MEM_SELECTION_PARSE`, not a fallback to native - reading a different store
    /// than configured hides the corruption.
    pub fn open_selected(data_dir: &Path) -> Result<Arc<dyn MemoryBackend>, PantheonError> {
        let sel = load_selection_result(data_dir)?;
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

    type Factory = Arc<
        dyn Fn(&BackendSelection) -> Result<Arc<dyn MemoryBackend>, PantheonError> + Send + Sync,
    >;

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

    /// One HTTP-bridge plugin backend. `default_url` is `None` everywhere: no
    /// vendor speaks Pantheon's `/v1/memory`, so a vendor URL as a default would
    /// point the client at an API that returns 404s or foreign JSON. Users supply
    /// `options.url` or the env var pointing at their own bridge, so a missing one
    /// is a clear `MEM_BACKEND_CONFIG`.
    struct PluginSpec {
        name: &'static str,
        label: &'static str,
        default_url: Option<&'static str>,
        auth: &'static str,
        deployment: DeploymentKind,
    }

    /// Registered HTTP-bridge backends. URL from selection options (`url`) or
    /// `PANTHEON_MEMORY_<NAME>_URL`; key from `PANTHEON_MEMORY_<NAME>_KEY` only,
    /// never the selection file. No entry has a default URL, so an unconfigured
    /// backend fails with `MEM_BACKEND_CONFIG`.
    ///
    /// Checked against the vendors' own docs, never live-tested at the wire level,
    /// and none of them speak `/v1/memory` - a bridge is required either way.
    /// Hindsight Cloud (`hsk_`) and `hindsight-api` on :8888; Honcho Cloud and
    /// self-host on :8000; Supermemory Cloud and `npx supermemory local` on :6767
    /// (local is not keyless); Mem0 Cloud (`Token`, not Bearer); OpenViking on :1933
    /// (`X-API-Key`). GalaxyMem, ByteRover, Mnemosyne, mnemopi and sharpshooter have
    /// no plain REST at all.
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
        auth: "root_api_key from ov.conf via PANTHEON_MEMORY_OPENVIKING_KEY (sent to the bridge as Bearer; OpenViking itself uses X-API-Key)",
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
    ) -> impl Fn(&BackendSelection) -> Result<Arc<dyn MemoryBackend>, PantheonError>
           + Send
           + Sync
           + 'static {
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
            // A `key` here can only be an in-memory leftover or a hand-edited
            // file that skipped the loader. Ignore it loudly rather than honour a
            // plaintext key.
            if sel.options.contains_key("key") {
                eprintln!(
                    "pantheon: WARNING: ignoring a `key` in the {} backend selection - \
                 backend keys live in ${env_key} (or the Secure Vault) only, \
                 never in memory-backend.toml",
                    spec.name,
                );
            }
            let key = std::env::var(&env_key).ok();
            Ok(Arc::new(crate::http_backend::HttpBackend::new(base, key))
                as Arc<dyn MemoryBackend>)
        }
    }

    impl BackendRegistry {
        /// Native + http + plugin factories pre-registered. The native factory
        /// opens a fresh in-memory store per call (tests); sessions use
        /// `open_selected`.
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
                    label: "External memory backend over HTTP (Pantheon /v1/memory protocol)"
                        .into(),
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

        /// Register a backend whose factory receives the persisted selection.
        /// That's how a plugin reads `url`; keys never travel this way - see
        /// `CREDENTIAL_OPTION_KEYS`.
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

    #[cfg(test)]
    mod tests {
        use super::*;

        fn tmpdir(tag: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-backend-key-{tag}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn sel_with_key() -> BackendSelection {
            let mut options = HashMap::new();
            options.insert("url".to_string(), "http://127.0.0.1:9000".to_string());
            options.insert("key".to_string(), "sekrit-plaintext-key".to_string());
            BackendSelection {
                name: "mem0".to_string(),
                options,
            }
        }

        #[test]
        fn save_selection_never_writes_a_key_value() {
            let dir = tmpdir("save");
            save_selection(&dir, &sel_with_key()).unwrap();
            let text = std::fs::read_to_string(selection_path(&dir)).unwrap();
            assert!(
                !text.contains("sekrit-plaintext-key"),
                "selection file must not contain the key value: {text}"
            );
            assert!(
                text.contains("http://127.0.0.1:9000"),
                "non-secret options survive: {text}"
            );
        }

        #[test]
        fn load_selection_strips_a_legacy_plaintext_key() {
            let dir = tmpdir("load");
            // A file written by an older build, key included.
            std::fs::write(
            selection_path(&dir),
            "name = \"mem0\"\n[options]\nurl = \"http://127.0.0.1:9000\"\nkey = \"sekrit-plaintext-key\"\n",
        )
        .unwrap();
            let sel = load_selection(&dir);
            assert_eq!(sel.name, "mem0");
            assert_eq!(
                sel.options.get("url").map(String::as_str),
                Some("http://127.0.0.1:9000")
            );
            assert!(
                !sel.options.contains_key("key"),
                "loaded selection must not carry the key"
            );
        }

        #[test]
        fn corrupt_selection_is_an_error_not_a_silent_native_fallback() {
            let dir = tmpdir("corrupt");
            std::fs::write(selection_path(&dir), "name = [\nthis is not toml").unwrap();
            let err = load_selection_result(&dir).expect_err("corrupt file must error");
            assert_eq!(err.code, "MEM_SELECTION_PARSE");
            assert!(
                open_selected(&dir).is_err(),
                "open_selected must not silently fall back to native"
            );
        }

        #[test]
        fn sync_from_config_wins_and_writes_back_on_conflict() {
            let dir = tmpdir("sync-win");
            save_selection(
                &dir,
                &BackendSelection {
                    name: "mem0".to_string(),
                    options: HashMap::from([(
                        "url".to_string(),
                        "http://127.0.0.1:9000".to_string(),
                    )]),
                },
            )
            .unwrap();
            // Config names another backend: it wins, a warning is
            // returned, and the selection file is rewritten to match.
            let warning = sync_selection_from_config(&dir, "honcho", &HashMap::new())
                .expect("conflict must warn");
            assert!(
                warning.contains("honcho"),
                "warning names the winner: {warning}"
            );
            assert!(
                warning.contains("mem0"),
                "warning names the loser: {warning}"
            );
            let sel = load_selection(&dir);
            assert_eq!(sel.name, "honcho");
            // No config options: the selection file's options are kept.
            assert_eq!(
                sel.options.get("url").map(String::as_str),
                Some("http://127.0.0.1:9000")
            );
            // In agreement now: no warning, no rewrite.
            assert_eq!(
                sync_selection_from_config(&dir, "honcho", &HashMap::new()),
                None
            );
            // Config options win when present.
            let warning = sync_selection_from_config(
                &dir,
                "honcho",
                &HashMap::from([("url".to_string(), "http://127.0.0.1:9001".to_string())]),
            )
            .expect("option conflict must warn");
            assert!(warning.contains("honcho"), "warning: {warning}");
            assert_eq!(
                load_selection(&dir).options.get("url").map(String::as_str),
                Some("http://127.0.0.1:9001")
            );
        }

        #[test]
        fn sync_from_config_leaves_an_empty_backend_and_a_corrupt_file_alone() {
            let dir = tmpdir("sync-noop");
            save_selection(&dir, &BackendSelection::default()).unwrap();
            assert_eq!(sync_selection_from_config(&dir, "", &HashMap::new()), None);
            assert_eq!(
                sync_selection_from_config(&dir, "   ", &HashMap::new()),
                None
            );
            assert_eq!(load_selection(&dir).name, "native");
            // Corrupt file: sync does not overwrite it (the opener still
            // errors, as before), and does not pretend to have resolved
            // anything.
            std::fs::write(selection_path(&dir), "name = [\nthis is not toml").unwrap();
            assert_eq!(
                sync_selection_from_config(&dir, "honcho", &HashMap::new()),
                None
            );
            assert!(load_selection_result(&dir).is_err());
            assert_eq!(
                std::fs::read_to_string(selection_path(&dir)).unwrap(),
                "name = [\nthis is not toml"
            );
        }
    }
}

pub mod http_backend {
    //! HTTP memory backend adapter for services that implement this JSON API:
    //!
    //!   GET  /v1/memory/recall?query=...&limit=N&layers=...
    //!     -> 200 [{"key":..,"value":..,"rank":..,"provenance":{...}}]
    //!
    //!   POST /v1/memory/write
    //!     body: {"layer":"Agent","namespace":"nyx","key":"..","value":"..",
    //!            "provenance":{...},"max_bytes":N}
    //!     -> 200 {"key":..,"value":..,"provenance":{...}}
    //!
    //!   GET  /v1/memory/list_agent?namespace=...
    //!     -> 200 [["key","value"], ...]
    //!
    //! Errors are 4xx/5xx with {"error":"CODE","cause":"..."} or plain text,
    //! mapped back to PantheonError.
    use crate::{LayerKind, MemoryBackend, MemoryRecord, Proposal, Provenance, Recalled};
    use pantheon_api::capability::Policy;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::provenance::TrustTier;
    use serde::{Deserialize, Serialize};

    fn merr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Memory,
            false,
            cause,
            "check the backend service",
            "",
        )
    }

    #[derive(Debug, Clone)]
    pub struct HttpBackend {
        base: String,
        api_key: Option<String>,
        /// Path prefix the service mounts the protocol under. Default
        /// `/v1/memory`; custom plugins set their own (e.g. `/memory`).
        prefix: String,
        /// Cap on the trust tier the server may report. Default Untrusted: a
        /// compromised server shouldn't launder injected `user`-tier records into
        /// the recall stream.
        trust_ceiling: TrustTier,
    }

    impl HttpBackend {
        pub fn new(base: String, api_key: Option<String>) -> Self {
            Self {
                base,
                api_key,
                prefix: "/v1/memory".into(),
                trust_ceiling: TrustTier::Untrusted,
            }
        }

        /// Same adapter, custom protocol mount point. Trailing slashes are
        /// normalized so both `url` and `prefix` can be written either way.
        pub fn with_prefix(
            base: String,
            api_key: Option<String>,
            prefix: impl Into<String>,
        ) -> Self {
            let mut p = prefix.into();
            if !p.starts_with('/') {
                p.insert(0, '/');
            }
            while p.ends_with('/') {
                p.pop();
            }
            Self {
                base,
                api_key,
                prefix: p,
                trust_ceiling: TrustTier::Untrusted,
            }
        }

        /// Raise or lower the trust ceiling for tiers the server reports.
        /// Default is Untrusted; only raise this for a server the operator
        /// fully controls.
        pub fn with_trust_ceiling(mut self, ceiling: TrustTier) -> Self {
            self.trust_ceiling = ceiling;
            self
        }

        fn clamp_hit(&self, mut hit: Recalled) -> Recalled {
            hit.record.provenance.trust =
                crate::clamp_trust(hit.record.provenance.trust, self.trust_ceiling);
            hit
        }

        fn clamp_record(&self, mut record: MemoryRecord) -> MemoryRecord {
            record.provenance.trust =
                crate::clamp_trust(record.provenance.trust, self.trust_ceiling);
            record
        }

        /// `<base><prefix>/<suffix>` with the base trailing slash trimmed.
        fn endpoint(&self, suffix: &str) -> String {
            format!(
                "{}{}/{}",
                self.base.trim_end_matches('/'),
                self.prefix,
                suffix
            )
        }
    }

    #[derive(Serialize)]
    struct WriteRequest {
        layer: String,
        namespace: String,
        key: String,
        value: String,
        provenance: Provenance,
        max_bytes: usize,
    }

    #[derive(Deserialize, Default)]
    struct ErrorResponse {
        error: Option<String>,
        cause: Option<String>,
    }

    /// Server error to `PantheonError`. `error` is attacker-controlled, so only
    /// allowlisted codes pass; the rest become `MEM_HTTP_REMOTE`. `cause` is free
    /// text: control characters stripped, length capped.
    fn remote_error(parsed: &ErrorResponse, status: u16, endpoint: &str) -> PantheonError {
        let code = parsed
            .error
            .as_deref()
            .map(|c| crate::sanitize_remote_code(c, "MEM_HTTP_REMOTE"))
            .unwrap_or_else(|| format!("HTTP {status}"));
        let cause = parsed
            .cause
            .as_deref()
            .map(crate::sanitize_remote_text)
            .unwrap_or_else(|| format!("HTTP {status} from {endpoint}"));
        merr(&code, cause)
    }

    fn layer_str(layer: LayerKind) -> String {
        match layer {
            LayerKind::Global => "Global",
            LayerKind::Agent => "Agent",
            LayerKind::Project => "Project",
            LayerKind::TaskSession => "TaskSession",
            LayerKind::EphemeralTurn => "EphemeralTurn",
        }
        .to_string()
    }

    impl MemoryBackend for HttpBackend {
        fn recall(
            &self,
            _policy: &Policy,
            namespaces: &[&str],
            layers: &[LayerKind],
            query: &str,
            limit: usize,
        ) -> Result<Vec<Recalled>, PantheonError> {
            if namespaces.is_empty() {
                return Ok(Vec::new());
            }
            let layers_str = layers
                .iter()
                .copied()
                .map(layer_str)
                .collect::<Vec<_>>()
                .join(",");
            // Namespace goes on the wire, not just the signature. The remote
            // filters; if it ignores this, that is the operator's server, but
            // we must at least stop *not asking*.
            let ns_str = namespaces.join(",");
            let url = format!(
                "{}?query={}&limit={}&layers={}&namespaces={}",
                self.endpoint("recall"),
                pct(query),
                limit,
                pct(&layers_str),
                pct(&ns_str)
            );
            let resp = http_get(&url, self.api_key.as_deref())?;
            let hits: Vec<Recalled> = serde_json::from_slice(&resp.body).map_err(|e| {
                merr(
                    "MEM_HTTP_DECODE",
                    format!("recall response was not valid JSON: {e}"),
                )
            })?;
            // Trust laundering guard: the server's claimed tiers are capped at
            // the ceiling before anything downstream sees them.
            Ok(hits.into_iter().map(|h| self.clamp_hit(h)).collect())
        }

        fn write(
            &self,
            _policy: &Policy,
            proposal: Proposal,
            max_bytes: usize,
        ) -> Result<MemoryRecord, PantheonError> {
            let req = WriteRequest {
                layer: layer_str(proposal.layer),
                namespace: proposal.namespace,
                key: proposal.key,
                value: proposal.value,
                provenance: proposal.provenance,
                max_bytes,
            };
            let body = serde_json::to_vec(&req).map_err(|e| {
                merr(
                    "MEM_HTTP_ENCODE",
                    format!("failed to encode write request: {e}"),
                )
            })?;
            let endpoint = self.endpoint("write");
            let resp = http_post(&endpoint, &body, self.api_key.as_deref())?;
            if resp.status >= 400 {
                let parsed: ErrorResponse = serde_json::from_slice(&resp.body).unwrap_or_default();
                return Err(remote_error(&parsed, resp.status, &endpoint));
            }
            let record: MemoryRecord = serde_json::from_slice(&resp.body).map_err(|e| {
                merr(
                    "MEM_HTTP_DECODE",
                    format!("write response was not valid JSON: {e}"),
                )
            })?;
            Ok(self.clamp_record(record))
        }

        fn list_agent(&self, namespace: &str) -> Result<Vec<(String, String)>, PantheonError> {
            let url = format!(
                "{}?namespace={}",
                self.endpoint("list_agent"),
                pct(namespace)
            );
            let resp = http_get(&url, self.api_key.as_deref())?;
            let rows: Vec<(String, String)> = serde_json::from_slice(&resp.body).map_err(|e| {
                merr(
                    "MEM_HTTP_DECODE",
                    format!("list_agent response was not valid JSON: {e}"),
                )
            })?;
            Ok(rows)
        }
    }

    /// Percent-encode a string for use in a URL query parameter.
    fn pct(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }

    struct HttpResponse {
        status: u16,
        body: Vec<u8>,
    }

    fn http_get(url: &str, api_key: Option<&str>) -> Result<HttpResponse, PantheonError> {
        let agent = http_agent(url);
        let mut req = agent.get(url);
        if let Some(k) = api_key {
            req = req.header("Authorization", format!("Bearer {k}").as_str());
        }
        let resp = req.call().map_err(|e| http_conn_err("GET", url, e))?;
        read_response("GET", url, resp)
    }

    fn http_post(
        url: &str,
        payload: &[u8],
        api_key: Option<&str>,
    ) -> Result<HttpResponse, PantheonError> {
        let agent = http_agent(url);
        let mut req = agent.post(url);
        if let Some(k) = api_key {
            req = req.header("Authorization", format!("Bearer {k}").as_str());
        }
        let resp = req
            .header("Content-Type", "application/json")
            .send(payload)
            .map_err(|e| http_conn_err("POST", url, e))?;
        read_response("POST", url, resp)
    }

    /// Shared agent: bounded timeout, rustls, no async runtime.
    ///
    /// Statuses come back as responses so error bodies go through the same capped
    /// read. ureq 3 takes the env proxy by default; loopback and `NO_PROXY` hosts
    /// get an agent with it disabled.
    fn http_agent(url: &str) -> ureq::Agent {
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            );
        if bypasses_proxy(url) {
            builder = builder.proxy(None);
        }
        builder.build().new_agent()
    }

    /// Whether requests to `url` must skip the environment proxy.
    fn bypasses_proxy(url: &str) -> bool {
        let host = match url_host(url) {
            Some(h) => h,
            None => return false,
        };
        if host == "localhost"
            || host == "127.0.0.1"
            || host == "::1"
            || host.ends_with(".localhost")
        {
            return true;
        }
        let list = std::env::var("NO_PROXY")
            .or_else(|_| std::env::var("no_proxy"))
            .unwrap_or_default();
        host_matches_no_proxy(&host, &list)
    }

    /// `NO_PROXY` matching, split out so it's unit-testable: comma-separated, `*`
    /// matches everything, a leading dot or none matches the domain and its
    /// subdomains, an optional `:port` is ignored.
    fn host_matches_no_proxy(host: &str, no_proxy: &str) -> bool {
        no_proxy.split(',').any(|entry| {
            let entry = entry.trim().trim_start_matches('.').to_lowercase();
            if entry.is_empty() {
                return false;
            }
            if entry == "*" {
                return true;
            }
            let entry_host = entry.split(':').next().unwrap_or("");
            host == entry_host || host.ends_with(&format!(".{entry_host}"))
        })
    }

    /// The lowercased host of an `http(s)://host[:port]/...` URL.
    fn url_host(url: &str) -> Option<String> {
        let host_port = url.split("://").nth(1)?.split('/').next()?;
        let host = if let Some(inner) = host_port.strip_prefix('[') {
            inner.split(']').next()?
        } else {
            host_port.split(':').next()?
        };
        if host.is_empty() {
            return None;
        }
        Some(host.to_lowercase())
    }

    fn http_conn_err(method: &str, url: &str, e: ureq::Error) -> PantheonError {
        merr("MEM_HTTP_CONN", format!("{method} {url}: {e}"))
    }

    /// Read a response body capped at [`crate::MAX_RESPONSE_BYTES`]. A buggy
    /// or hostile bridge streaming an unlimited body is a bounded
    /// `MEM_HTTP_OVERSIZE` error, not an unbounded buffer.
    fn capped_body(
        endpoint: &str,
        mut resp: ureq::http::Response<ureq::Body>,
    ) -> Result<Vec<u8>, PantheonError> {
        use std::io::Read;
        let status = resp.status();
        let mut reader = resp
            .body_mut()
            .as_reader()
            .take(crate::MAX_RESPONSE_BYTES as u64 + 1);
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .map_err(|e| merr("MEM_HTTP_BODY", format!("{endpoint}: {e}")))?;
        if body.len() > crate::MAX_RESPONSE_BYTES {
            return Err(merr(
                "MEM_HTTP_OVERSIZE",
                format!(
                "{endpoint}: response exceeds the {}MB memory-backend response cap (HTTP {status})",
                crate::MAX_RESPONSE_BYTES / (1024 * 1024)
            ),
            ));
        }
        Ok(body)
    }

    fn read_response(
        method: &str,
        url: &str,
        resp: ureq::http::Response<ureq::Body>,
    ) -> Result<HttpResponse, PantheonError> {
        let endpoint = format!("{method} {url}");
        let status = resp.status().as_u16();
        let body = capped_body(&endpoint, resp)?;
        if status >= 400 {
            let parsed: ErrorResponse = serde_json::from_slice(&body).unwrap_or_default();
            return Err(remote_error(&parsed, status, &endpoint));
        }
        Ok(HttpResponse { status, body })
    }
}

pub mod markdown {
    //! MEMORY.md interface. Auto-syncs Agent-layer records with a markdown file at
    //! the project root: each record is a `# <key>` heading and its value as the
    //! body. The store is the source of truth, the file is a projection - git
    //! diffable, greppable, and importable on session start.

    use crate::{LayerKind, MemoryStore, Proposal, Provenance};
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::logging::now_ms;
    use std::path::Path;

    fn merr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Memory,
            false,
            cause,
            "check the memory.md path and permissions",
            "",
        )
    }

    fn fnv1a_hex(data: &[u8]) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in data {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    }

    fn file_hash(path: &Path) -> Result<String, PantheonError> {
        let bytes = std::fs::read(path).map_err(|e| merr("MEM_SYNC_READ", format!("{e}")))?;
        Ok(fnv1a_hex(&bytes))
    }

    /// Result of a sync pass: import/export decisions made and the next
    /// `MEMORY.md` snapshot written to disk.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SyncReport {
        /// True when the on-disk file differed from the last imported hash
        /// and the diff was applied to the store.
        pub imported: bool,
        /// True when the store changed after import (or had no prior file)
        /// and the export was written back to disk.
        pub exported: bool,
        /// Hash of the file that was imported (or the existing one if no
        /// import happened this round).
        pub file_hash: String,
    }

    /// Conflict: both the on-disk file and the store changed since the last
    /// sync. The caller must reconcile manually (import, export, or merge)
    /// before silent data loss becomes a problem.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SyncConflict {
        pub file_hash: String,
        pub store_hash: String,
    }

    fn store_hash(store: &MemoryStore, namespace: &str) -> Result<String, PantheonError> {
        let rows = store.list_agent_meta(namespace)?;
        let md = render_agent(&rows);
        Ok(fnv1a_hex(md.as_bytes()))
    }

    /// One sync pass: read `path` and the store, import file changes, then export
    /// if the store changed since `last_known_hash`. Pass `None` on a fresh
    /// install. Returns the new hash to store for the next pass.
    ///
    /// For a sentinel export file, a section missing from it is a deletion.
    /// Hand-written or legacy files never delete - they may be partial views. Run
    /// `detect_conflict` first when both sides may have changed.
    pub fn sync(
        store: &MemoryStore,
        policy: &pantheon_api::capability::Policy,
        namespace: &str,
        path: &Path,
        last_known_hash: Option<&str>,
    ) -> Result<SyncReport, PantheonError> {
        let file_exists = path.exists();
        let cur_hash = if file_exists {
            file_hash(path)?
        } else {
            "".into()
        };

        let mut imported = false;

        // Step 1: file changes since last sync?
        let file_changed = last_known_hash
            .map(|h| h != cur_hash)
            .unwrap_or(file_exists);
        if file_exists && file_changed {
            // Deletions only for sentinel files, which are full exports. A
            // hand-written file may be a partial view, and deleting rows it
            // doesn't mention would nuke records it never claimed to cover.
            let text =
                std::fs::read_to_string(path).map_err(|e| merr("MEM_SYNC_READ", format!("{e}")))?;
            let sentinel = text
                .lines()
                .next()
                .is_some_and(|l| l == "<!-- pantheon:agent-memory v2 -->");
            let parsed = parse_md_meta(&text);
            let parsed_keys: std::collections::HashSet<String> =
                parsed.iter().map(|(k, _, _)| k.clone()).collect();
            import_parsed(store, policy, namespace, parsed)?;
            imported = true;
            if sentinel {
                for (key, _) in store.list_agent(namespace)? {
                    if !parsed_keys.contains(&key) {
                        store.forget(LayerKind::Agent, namespace, &key)?;
                    }
                }
            }
        }

        // Step 2: store changed since last sync?
        let store_hash_now = store_hash(store, namespace)?;
        let store_changed = last_known_hash.map(|h| h != store_hash_now).unwrap_or(true);
        if !imported && !store_changed {
            return Ok(SyncReport {
                imported: false,
                exported: false,
                file_hash: cur_hash,
            });
        }

        // Step 3: write the projection back. Atomic via tmp+rename.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| merr("MEM_SYNC_MKDIR", format!("{e}")))?;
            }
        }
        let tmp = path.with_extension("md.sync.tmp");
        let rows = store.list_agent_meta(namespace)?;
        let md = render_agent(&rows);
        std::fs::write(&tmp, md).map_err(|e| merr("MEM_SYNC_WRITE", format!("{e}")))?;
        std::fs::rename(&tmp, path).map_err(|e| merr("MEM_SYNC_RENAME", format!("{e}")))?;
        let exported = true;

        Ok(SyncReport {
            imported,
            exported,
            file_hash: file_hash(path)?,
        })
    }

    /// Detect a conflict between the on-disk file and the store. The caller
    /// must reconcile before calling `sync`, otherwise the write-back clobbers
    /// one side.
    pub fn detect_conflict(
        store: &MemoryStore,
        namespace: &str,
        path: &Path,
        last_known_hash: Option<&str>,
    ) -> Result<Option<SyncConflict>, PantheonError> {
        let file_exists = path.exists();
        let cur_hash = if file_exists {
            file_hash(path)?
        } else {
            "".into()
        };
        let store_hash_now = store_hash(store, namespace)?;
        let file_changed = last_known_hash
            .map(|h| h != cur_hash)
            .unwrap_or(file_exists);
        let store_changed = last_known_hash
            .map(|h| h != store_hash_now)
            .unwrap_or(false);
        if file_changed && store_changed {
            Ok(Some(SyncConflict {
                file_hash: cur_hash,
                store_hash: store_hash_now,
            }))
        } else {
            Ok(None)
        }
    }

    /// Render Agent records for a namespace. Value lines that look like headings
    /// get a leading backslash so a value holding `# something` survives the
    /// round-trip. `trust` becomes a provenance footer that `parse_md` reads back;
    /// legacy 2-tuple callers get `user`.
    pub fn render_record(
        key: &str,
        value: &str,
        trust: pantheon_api::provenance::TrustTier,
    ) -> String {
        let escaped = value
            .lines()
            .map(|l| {
                if l.starts_with("# ") || l == "#" {
                    format!("\\{l}")
                } else if l.starts_with("{trust=") {
                    // A value line shaped like the footer would be misparsed as
                    // the record's footer on import. Escape it so it stays a value.
                    format!("\\{l}")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("# {key}\n\n{{trust={}}}\n{}\n\n", trust.as_str(), escaped)
    }

    pub fn render_agent(
        records: &[(String, String, pantheon_api::provenance::TrustTier)],
    ) -> String {
        let mut out = String::new();
        // Sentinel header: unambiguous, and a key named "Agent memory" still
        // round-trips because the header is not a `# ` heading.
        out.push_str("<!-- pantheon:agent-memory v2 -->\n\n# Agent memory\n\n");
        for (k, v, t) in records {
            out.push_str(&render_record(k, v, *t));
        }
        out
    }

    /// Export the Agent-layer records for one namespace to a markdown file.
    pub fn export_agent(
        store: &MemoryStore,
        namespace: &str,
        path: &Path,
    ) -> Result<(), PantheonError> {
        let rows = list_agent_records(store, namespace)?;
        let md = render_agent(&rows);
        std::fs::write(path, md)
            .map_err(|e| merr("MEM_EXPORT", format!("write {}: {e}", path.display())))
    }

    /// List Agent-layer records and export them as `MEMORY.md`.
    pub fn list_agent_records(
        store: &MemoryStore,
        namespace: &str,
    ) -> Result<Vec<(String, String, pantheon_api::provenance::TrustTier)>, PantheonError> {
        store.list_agent_meta(namespace)
    }

    /// Parse a markdown file into Agent records: split on `# <key>`, body until
    /// the next heading is the value, and unescape what render escaped. The
    /// sentinel header marks v1 exports; without it the first `# Agent memory`
    /// line is the document header and is skipped, so a v1 key of that name still
    /// round-trips.
    pub fn parse_md_meta(
        content: &str,
    ) -> Vec<(String, String, Option<pantheon_api::provenance::TrustTier>)> {
        let mut out = Vec::new();
        let mut current: Option<(String, String, Option<String>)> = None;
        let mut seen_header = false;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("\\# ") {
                // Escaped heading-like line inside a value.
                if let Some((_, body, _)) = current.as_mut() {
                    body.push_str(&format!("# {rest}\n"));
                }
                continue;
            }
            if line == "\\#" {
                if let Some((_, body, _)) = current.as_mut() {
                    body.push_str("#\n");
                }
                continue;
            }
            // Escaped footer-like line inside a value (see render_record):
            // the backslash keeps it value, not a provenance footer.
            if let Some(rest) = line.strip_prefix("\\{trust=") {
                if let Some((_, body, _)) = current.as_mut() {
                    body.push_str(&format!("{{trust={rest}\n"));
                }
                continue;
            }
            // Provenance footer: `{trust=tier}` as the first line of a body.
            // Captured and stripped so it never shows up in the value.
            if let Some(rest) = line.strip_prefix("{trust=") {
                if let Some((_, body, trust)) = current.as_mut() {
                    if body.trim().is_empty() && trust.is_none() {
                        if let Some(tier) = rest.strip_suffix('}') {
                            *trust = Some(tier.trim().to_string());
                            continue;
                        }
                    }
                }
            }
            if let Some(rest) = line.strip_prefix("# ") {
                let key = rest.trim().to_string();
                // Document header: always skipped, sentinel or legacy. In a
                // sentinel file only the very first occurrence is the header;
                // later `# Agent memory` lines are real records.
                if key == "Agent memory" && !seen_header {
                    seen_header = true;
                    continue;
                }
                if let Some((key, body, trust)) = current.take() {
                    let tier = trust.and_then(|t| pantheon_api::provenance::TrustTier::parse(&t));
                    out.push((key, body.trim().to_string(), tier));
                }
                current = Some((key, String::new(), None));
            } else if let Some((_, body, _)) = current.as_mut() {
                body.push_str(line);
                body.push('\n');
            }
        }
        if let Some((key, body, trust)) = current {
            let tier = trust.and_then(|t| pantheon_api::provenance::TrustTier::parse(&t));
            out.push((key, body.trim().to_string(), tier));
        }
        out
    }

    /// Legacy two-tuple view over parse_md_meta (tests, simple importers).
    pub fn parse_md(content: &str) -> Vec<(String, String)> {
        parse_md_meta(content)
            .into_iter()
            .map(|(k, v, _)| (k, v))
            .collect()
    }

    /// Import Agent records from a markdown file. Each section becomes a Proposal
    /// through the normal write path.
    ///
    /// Both v2 (per-record tier) and hand-edited files import at the file's tier,
    /// capped at `User` - `system` is harness-generated control, and a project file
    /// an agent or a cloned repo can write must not mint it. Then capped at the
    /// store's existing tier, so an import can keep or lower but never upgrade.
    /// The anti-clobber rule still applies: below the stored tier the row is held.
    ///
    /// All-or-nothing: every section is validated before any write.
    pub fn import_agent(
        store: &MemoryStore,
        policy: &pantheon_api::capability::Policy,
        namespace: &str,
        path: &Path,
    ) -> Result<usize, PantheonError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| merr("MEM_IMPORT_READ", format!("read {}: {e}", path.display())))?;
        import_parsed(store, policy, namespace, parse_md_meta(&text))
    }

    /// Import already-parsed sections. Shared by [`import_agent`] and by
    /// [`sync`], which needs the parsed keys for deletion propagation and
    /// must not re-read the file between parsing and importing.
    fn import_parsed(
        store: &MemoryStore,
        policy: &pantheon_api::capability::Policy,
        namespace: &str,
        parsed: Vec<(String, String, Option<pantheon_api::provenance::TrustTier>)>,
    ) -> Result<usize, PantheonError> {
        let mut proposals = Vec::with_capacity(parsed.len());
        for (key, value, trust) in parsed {
            // A v2 file carries each record's tier; a hand-edited one is
            // human-authored, so User. Untrusted stays Untrusted - the round-trip
            // must not launder trust. Capped at User: a project file must not mint
            // `system`.
            let tier = trust.unwrap_or(pantheon_api::provenance::TrustTier::User);
            let tier = if tier.rank() > pantheon_api::provenance::TrustTier::User.rank() {
                pantheon_api::provenance::TrustTier::User
            } else {
                tier
            };
            // Never upgrade on import: cap the file's tier at the store's
            // existing tier for this row. New rows keep the User-capped tier.
            let tier = match store.get(LayerKind::Agent, namespace, &key)? {
                Some(existing) if existing.provenance.trust.rank() < tier.rank() => {
                    existing.provenance.trust
                }
                _ => tier,
            };
            proposals.push(Proposal {
                layer: LayerKind::Agent,
                namespace: namespace.to_string(),
                key,
                value,
                provenance: Provenance {
                    source: "import".into(),
                    origin: "memory.md".into(),
                    trust: tier,
                    recorded_at_ms: now_ms(),
                },
            });
        }
        // Validate everything first: a single bad section must abort the
        // whole import, not leave the earlier sections stored.
        for p in &proposals {
            if let Err(r) = crate::validate(p, 4096) {
                return Err(merr(
                    r.code(),
                    format!(
                        "proposal failed validation: {r:?}; fix the section; nothing was imported"
                    ),
                ));
            }
        }
        let mut written = 0;
        for p in proposals {
            crate::propose_write(store, policy, p, 4096)?;
            written += 1;
        }
        Ok(written)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use pantheon_api::capability::{Capability, Policy};

        fn put(store: &MemoryStore, key: &str, value: &str) {
            store
                .put(&Proposal {
                    layer: LayerKind::Agent,
                    namespace: "test".to_string(),
                    key: key.to_string(),
                    value: value.to_string(),
                    provenance: Provenance {
                        source: "test".to_string(),
                        origin: "test".to_string(),
                        trust: pantheon_api::provenance::TrustTier::User,
                        recorded_at_ms: 1,
                    },
                })
                .unwrap();
        }

        fn policy() -> Policy {
            Policy::default().allow(Capability::MemoryWrite)
        }

        #[test]
        fn footer_like_value_line_survives_a_round_trip() {
            let rendered = render_record(
                "k",
                "{trust=user}\nsecond line",
                pantheon_api::provenance::TrustTier::User,
            );
            let parsed = parse_md_meta(&rendered);
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].0, "k");
            assert_eq!(parsed[0].1, "{trust=user}\nsecond line");
            assert_eq!(
                parsed[0].2,
                Some(pantheon_api::provenance::TrustTier::User),
                "the footer tier comes from the real footer, not the value line"
            );
        }

        #[test]
        fn import_is_all_or_nothing_when_a_section_is_empty() {
            let store = MemoryStore::open_in_memory().unwrap();
            let dir = std::env::temp_dir().join(format!(
                "pantheon-md-import-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("MEMORY.md");
            std::fs::write(&path, "# good\n\nsome value\n\n# empty\n\n").unwrap();
            let err = import_agent(&store, &policy(), "test", &path)
                .expect_err("an empty section must abort the import");
            assert_eq!(err.code, "MEM_EMPTY");
            assert!(
                store
                    .get(LayerKind::Agent, "test", "good")
                    .unwrap()
                    .is_none(),
                "the earlier good section must NOT be stored when a later section fails"
            );
        }

        #[test]
        fn sync_deletes_sections_missing_from_a_sentinel_file() {
            let store = MemoryStore::open_in_memory().unwrap();
            put(&store, "keep", "kept value");
            put(&store, "drop", "dropped value");
            let dir = std::env::temp_dir().join(format!(
                "pantheon-md-sync-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("MEMORY.md");
            // A full Pantheon export containing only "keep".
            let md = render_agent(&[(
                "keep".to_string(),
                "kept value".to_string(),
                pantheon_api::provenance::TrustTier::User,
            )]);
            std::fs::write(&path, md).unwrap();
            sync(&store, &policy(), "test", &path, None).unwrap();
            assert!(store
                .get(LayerKind::Agent, "test", "keep")
                .unwrap()
                .is_some());
            assert!(
                store
                    .get(LayerKind::Agent, "test", "drop")
                    .unwrap()
                    .is_none(),
                "a section deleted from the sentinel file must not resurrect on export"
            );
            let exported = std::fs::read_to_string(&path).unwrap();
            assert!(
                !exported.contains("dropped value"),
                "export must not resurrect it either"
            );
        }
    }
}

pub mod plugins {
    //! User-defined memory plugins. A TOML manifest at
    //! `<data_dir>/memory-plugins/<name>.toml` makes a backend selectable like any
    //! built-in:
    //!
    //! ```toml
    //! name = "mybrain"
    //! label = "MyBrain memory service"
    //! kind = "http"              # or "stdio"
    //! url = "http://127.0.0.1:9000"   # http
    //! key = "token"                  # http (optional)
    //! prefix = "/v1/memory"          # http (optional)
    //! command = "python3"            # stdio
    //! args = ["mybrain_bridge.py"]   # stdio
    //! timeout_ms = 5000              # stdio (optional)
    //! ```
    //!
    //! Selection options override manifest values, so one manifest can serve
    //! several deployments.
    //!
    //! stdio is one JSON request line in, one response line out. Ops: `recall`,
    //! `write`, `list_agent`, `get`, `forget`, `confirm`; `get` and `confirm` carry
    //! the record's `layer` (row identity is layer+namespace+key). Responses are
    //! `{"ok":true, ...}` or `{"ok":false,"code":..,"cause":..}`. Process per
    //! call, killed on timeout.
    //!
    //! A plugin is never asked whether a write is allowed - the gate runs first,
    //! and plugins only store or return what they're handed.
    use crate::backend::{BackendKind, DeploymentKind};
    use crate::{BackendInfo, BackendRegistry, BackendSelection, LayerKind};
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::provenance::TrustTier;
    use serde::Deserialize;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    fn perr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Memory,
            false,
            cause,
            "check the memory plugin manifest and its service",
            "",
        )
    }

    /// One user plugin manifest.
    #[derive(Debug, Clone, Deserialize)]
    pub struct MemoryPluginManifest {
        pub name: String,
        #[serde(default)]
        pub label: String,
        /// "http" (default) or "stdio".
        #[serde(default = "default_kind")]
        pub kind: String,
        #[serde(default)]
        pub url: Option<String>,
        #[serde(default)]
        pub key: Option<String>,
        #[serde(default)]
        pub prefix: Option<String>,
        #[serde(default)]
        pub command: Option<String>,
        #[serde(default)]
        pub args: Vec<String>,
        #[serde(default = "default_timeout_ms")]
        pub timeout_ms: u64,
        /// Explicit operator consent to run a `stdio` command.
        ///
        /// A stdio backend is a child process with the operator's privileges - the
        /// same trust class as an extension or an exec plugin, and those need an
        /// approval record. Without it, a TOML file in `memory-plugins/` is arbitrary
        /// code execution on the next memory call. `http` backends run nothing, so
        /// they don't need it. Defaults to false, so existing manifests keep working
        /// and stdio has to opt in.
        #[serde(default)]
        pub allow_exec: bool,
    }

    fn default_kind() -> String {
        "http".into()
    }
    fn default_timeout_ms() -> u64 {
        5_000
    }

    impl MemoryPluginManifest {
        pub fn label_or_default(&self) -> String {
            if self.label.trim().is_empty() {
                format!("{} (user memory plugin)", self.name)
            } else {
                self.label.clone()
            }
        }

        /// Validate required fields per kind. Returns a structured error so
        /// `backend list` / session startup can report *why* a plugin was skipped.
        pub fn validate(&self) -> Result<(), PantheonError> {
            if self.name.trim().is_empty() {
                return Err(perr("MEM_PLUGIN_MANIFEST", "manifest has no name".into()));
            }
            match self.kind.as_str() {
                "http" => {
                    if self.url.as_deref().unwrap_or("").trim().is_empty() {
                        return Err(perr(
                            "MEM_PLUGIN_MANIFEST",
                            format!("{}: kind=http requires `url`", self.name),
                        ));
                    }
                }
                "stdio" => {
                    if self.command.as_deref().unwrap_or("").trim().is_empty() {
                        return Err(perr(
                            "MEM_PLUGIN_MANIFEST",
                            format!("{}: kind=stdio requires `command`", self.name),
                        ));
                    }
                    // Fail closed on the consent gate, and name the fix in
                    // the error: an operator who wrote this manifest can
                    // answer the question without reading the source.
                    if !self.allow_exec {
                        return Err(perr(
                            "MEM_PLUGIN_MANIFEST",
                            format!(
                                "{}: kind=stdio runs `{}` as a child process with your user \
privileges; set `allow_exec = true` in this manifest to consent to that",
                                self.name,
                                self.command.as_deref().unwrap_or("")
                            ),
                        ));
                    }
                }
                other => {
                    return Err(perr(
                        "MEM_PLUGIN_MANIFEST",
                        format!("{}: unknown kind '{other}' (http|stdio)", self.name),
                    ))
                }
            }
            Ok(())
        }
    }

    /// Parse a manifest from TOML text.
    pub fn parse_manifest(text: &str) -> Result<MemoryPluginManifest, PantheonError> {
        let m: MemoryPluginManifest = toml::from_str(text)
            .map_err(|e| perr("MEM_PLUGIN_MANIFEST", format!("invalid manifest TOML: {e}")))?;
        m.validate()?;
        Ok(m)
    }

    /// Load every manifest in `<data_dir>/memory-plugins` and register it. Bad
    /// ones are skipped with a loud line, not a panic - one broken file shouldn't
    /// brick the registry. Returns the names that loaded.
    pub fn load_dir(reg: &mut BackendRegistry, data_dir: &Path) -> Vec<String> {
        let dir = data_dir.join("memory-plugins");
        let mut loaded = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return loaded, // no plugins dir: normal case
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("memory plugin {}: unreadable: {e}", path.display());
                    continue;
                }
            };
            match parse_manifest(&text) {
                Ok(m) => {
                    // File name wins over a mismatched manifest name so the
                    // selection can always be derived from `ls`.
                    let stem = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&m.name)
                        .to_string();
                    let name = stem;
                    register_manifest(reg, name.clone(), m);
                    loaded.push(name);
                }
                Err(e) => eprintln!("memory plugin {}: skipped: {e}", path.display()),
            }
        }
        loaded.sort();
        loaded
    }

    fn register_manifest(reg: &mut BackendRegistry, name: String, m: MemoryPluginManifest) {
        let info = BackendInfo {
            name: name.clone(),
            label: m.label_or_default(),
            kind: if m.kind == "stdio" {
                BackendKind::Subprocess
            } else {
                BackendKind::Http
            },
            capabilities: vec!["memory.read".into(), "memory.write".into()],
            recommended: false,
            auth: String::new(),
            // User manifests point at bridges the user operates themselves:
            // a stdio manifest is a local process; an http manifest is a
            // self-hosted bridge.
            deployment: if m.kind == "stdio" {
                DeploymentKind::Local
            } else {
                DeploymentKind::SelfHosted
            },
        };
        reg.register_with(info, move |sel: &BackendSelection| {
            // Selection options override manifest values, so one manifest can
            // serve several deployments (dev/staging pointers, rotated keys).
            let opt = |k: &str| sel.options.get(k).cloned();
            if m.kind == "stdio" {
                let command = opt("command")
                    .or_else(|| m.command.clone())
                    .unwrap_or_default();
                let args = if sel.options.contains_key("args") {
                    split_args(opt("args").unwrap_or_default())
                } else {
                    m.args.clone()
                };
                let timeout_ms = opt("timeout_ms")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(m.timeout_ms);
                return Ok(Arc::new(StdioBackend::new(command, args, timeout_ms))
                    as Arc<dyn crate::MemoryBackend>);
            }
            let base = opt("url")
                .or_else(|| m.url.clone())
                .ok_or_else(|| perr("MEM_PLUGIN_MANIFEST", format!("{name}: no url")))?;
            let key = opt("key").or_else(|| m.key.clone());
            let prefix = opt("prefix")
                .or_else(|| m.prefix.clone())
                .unwrap_or_else(|| "/v1/memory".into());
            Ok(Arc::new(crate::http_backend::HttpBackend::with_prefix(
                base, key, prefix,
            )) as Arc<dyn crate::MemoryBackend>)
        });
    }

    /// `args="a b c"` option form (selection options are flat strings).
    fn split_args(s: String) -> Vec<String> {
        s.split_whitespace().map(|t| t.to_string()).collect()
    }

    fn layer_tag(layer: LayerKind) -> &'static str {
        match layer {
            LayerKind::Global => "Global",
            LayerKind::Agent => "Agent",
            LayerKind::Project => "Project",
            LayerKind::TaskSession => "TaskSession",
            LayerKind::EphemeralTurn => "EphemeralTurn",
        }
    }

    /// Subprocess backend: one JSON request line in, one JSON response line out.
    /// Spawned per call (same model as hook execution) so a wedged plugin is
    /// killed on timeout instead of holding the runtime.
    #[derive(Debug, Clone)]
    pub struct StdioBackend {
        command: String,
        args: Vec<String>,
        timeout: Duration,
        /// Max trust tier the plugin may report on a record. Default
        /// Untrusted - same laundering guard as the HTTP adapter.
        trust_ceiling: TrustTier,
    }

    /// Reap a plugin child, polling `try_wait` for up to a second before killing.
    /// `Child::wait` blocks forever, so a plugin that answers and then lingers
    /// would hang the caller. Every path after spawn goes through this.
    fn reap(child: &mut std::process::Child) {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    impl StdioBackend {
        pub fn new(command: String, args: Vec<String>, timeout_ms: u64) -> Self {
            Self {
                command,
                args,
                timeout: Duration::from_millis(timeout_ms.max(1)),
                trust_ceiling: TrustTier::Untrusted,
            }
        }

        /// Raise or lower the trust ceiling for tiers the plugin reports.
        /// Default is Untrusted; only raise this for a plugin the operator
        /// fully controls.
        pub fn with_trust_ceiling(mut self, ceiling: TrustTier) -> Self {
            self.trust_ceiling = ceiling;
            self
        }

        fn clamp_record(&self, mut record: crate::MemoryRecord) -> crate::MemoryRecord {
            record.provenance.trust =
                crate::clamp_trust(record.provenance.trust, self.trust_ceiling);
            record
        }

        /// One request/response exchange. Never panics; timeout kills the child.
        fn call(&self, req: &serde_json::Value) -> Result<serde_json::Value, PantheonError> {
            use std::io::{BufRead, BufReader, Read, Write};
            use std::process::{Command, Stdio};
            // Memory stdio plugins get the same scrub as hook and tool plugins:
            // env_clear, then PATH plus the allowlist.
            let mut cmd = Command::new(&self.command);
            cmd.args(&self.args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            cmd.env_clear();
            if let Ok(path) = std::env::var("PATH") {
                cmd.env("PATH", path);
            }
            let mut child = cmd.spawn().map_err(|e| {
                perr(
                    "MEM_PLUGIN_SPAWN",
                    format!("spawning {}: {e}", self.command),
                )
            })?;
            {
                let mut stdin = match child.stdin.take() {
                    Some(s) => s,
                    None => {
                        reap(&mut child);
                        return Err(perr(
                            "MEM_PLUGIN_IO",
                            format!("{}: stdin unavailable", self.command),
                        ));
                    }
                };
                let line = format!("{req}\n");
                if let Err(e) = stdin.write_all(line.as_bytes()) {
                    reap(&mut child);
                    return Err(perr(
                        "MEM_PLUGIN_IO",
                        format!("writing to {}: {e}", self.command),
                    ));
                }
            }
            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    reap(&mut child);
                    return Err(perr(
                        "MEM_PLUGIN_IO",
                        format!("{}: stdout unavailable", self.command),
                    ));
                }
            };
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                // `take` caps the reads at the oversize limit + 1 so an
                // oversized (or newline-less) response is detected as
                // `MEM_PLUGIN_OVERSIZE` instead of buffering without bound.
                let mut reader = BufReader::new(stdout).take(crate::MAX_RESPONSE_BYTES as u64 + 1);
                let mut line = String::new();
                let res = reader.read_line(&mut line).map(|n| (n, line));
                let _ = tx.send(res);
            });
            let (n, line) = match rx.recv_timeout(self.timeout) {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    reap(&mut child);
                    return Err(perr(
                        "MEM_PLUGIN_IO",
                        format!("reading from {}: {e}", self.command),
                    ));
                }
                Err(_) => {
                    reap(&mut child);
                    return Err(perr(
                        "MEM_PLUGIN_TIMEOUT",
                        format!(
                            "{} did not answer within {}ms",
                            self.command,
                            self.timeout.as_millis()
                        ),
                    ));
                }
            };
            if n > crate::MAX_RESPONSE_BYTES {
                reap(&mut child);
                return Err(perr(
                    "MEM_PLUGIN_OVERSIZE",
                    format!(
                        "{} response exceeds the {}MB memory-backend response cap",
                        self.command,
                        crate::MAX_RESPONSE_BYTES / (1024 * 1024)
                    ),
                ));
            }
            let line = line.trim();
            if line.is_empty() {
                reap(&mut child);
                return Err(perr(
                    "MEM_PLUGIN_EMPTY",
                    format!("{} produced no response", self.command),
                ));
            }
            let v: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    reap(&mut child);
                    return Err(perr(
                        "MEM_PLUGIN_DECODE",
                        format!("{} returned invalid JSON: {e}", self.command),
                    ));
                }
            };
            // Every path past the read reaps the child, including the success and
            // read-error paths, so no call leaks a zombie.
            reap(&mut child);
            if v.get("ok").and_then(|b| b.as_bool()) == Some(true) {
                Ok(v)
            } else {
                // `code` is plugin-controlled, so only allowlisted codes pass; the
                // rest become MEM_PLUGIN_REMOTE with the sanitized original in the
                // cause. `cause` is free text: control chars stripped, capped.
                let raw_code = v
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("MEM_PLUGIN_ERROR");
                let code = crate::sanitize_remote_code(raw_code, "MEM_PLUGIN_REMOTE");
                let mut cause = v
                    .get("cause")
                    .and_then(|c| c.as_str())
                    .map(crate::sanitize_remote_text)
                    .unwrap_or_else(|| "plugin reported failure".to_string());
                if code == "MEM_PLUGIN_REMOTE" && raw_code != "MEM_PLUGIN_ERROR" {
                    cause = format!(
                        "{cause} [remote code: {}]",
                        crate::sanitize_remote_text(raw_code)
                    );
                }
                Err(perr(&code, cause))
            }
        }
    }

    impl crate::MemoryBackend for StdioBackend {
        fn recall(
            &self,
            _policy: &pantheon_api::capability::Policy,
            namespaces: &[&str],
            layers: &[LayerKind],
            query: &str,
            limit: usize,
        ) -> Result<Vec<crate::Recalled>, PantheonError> {
            if namespaces.is_empty() {
                return Ok(Vec::new());
            }
            let names: Vec<&str> = layers.iter().map(|l| layer_tag(*l)).collect();
            let resp = self.call(&serde_json::json!({
                "op": "recall",
                "query": query,
                "limit": limit,
                "layers": names,
                "namespaces": namespaces,
            }))?;
            let hits: Vec<crate::Recalled> =
                serde_json::from_value(resp.get("hits").cloned().unwrap_or(serde_json::json!([])))
                    .map_err(|e| perr("MEM_PLUGIN_DECODE", format!("recall hits invalid: {e}")))?;
            // Trust laundering guard: the plugin's claimed tiers are capped at
            // the ceiling before anything downstream sees them.
            Ok(hits
                .into_iter()
                .map(|mut h| {
                    h.record = self.clamp_record(h.record);
                    h
                })
                .collect())
        }

        fn write(
            &self,
            _policy: &pantheon_api::capability::Policy,
            proposal: crate::Proposal,
            max_bytes: usize,
        ) -> Result<crate::MemoryRecord, PantheonError> {
            let resp = self.call(&serde_json::json!({
                "op": "write",
                "layer": layer_tag(proposal.layer),
                "namespace": proposal.namespace,
                "key": proposal.key,
                "value": proposal.value,
                "provenance": proposal.provenance,
                "max_bytes": max_bytes,
            }))?;
            let record: crate::MemoryRecord = serde_json::from_value(
                resp.get("record")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .map_err(|e| perr("MEM_PLUGIN_DECODE", format!("write record invalid: {e}")))?;
            Ok(self.clamp_record(record))
        }

        fn list_agent(&self, namespace: &str) -> Result<Vec<(String, String)>, PantheonError> {
            let resp = self.call(&serde_json::json!({
                "op": "list_agent",
                "namespace": namespace,
            }))?;
            serde_json::from_value(resp.get("rows").cloned().unwrap_or(serde_json::json!([])))
                .map_err(|e| perr("MEM_PLUGIN_DECODE", format!("list_agent rows invalid: {e}")))
        }

        fn get(
            &self,
            layer: LayerKind,
            namespace: &str,
            key: &str,
        ) -> Result<Option<crate::MemoryRecord>, PantheonError> {
            let resp = self.call(&serde_json::json!({
                "op": "get",
                "layer": layer_tag(layer),
                "namespace": namespace,
                "key": key,
            }))?;
            if resp.get("found").and_then(|f| f.as_bool()) != Some(true) {
                return Ok(None);
            }
            let record: crate::MemoryRecord = serde_json::from_value(
                resp.get("record")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .map_err(|e| perr("MEM_PLUGIN_DECODE", format!("get record invalid: {e}")))?;
            Ok(Some(self.clamp_record(record)))
        }

        fn forget(
            &self,
            layer: LayerKind,
            namespace: &str,
            key: &str,
        ) -> Result<bool, PantheonError> {
            let resp = self.call(&serde_json::json!({
                "op": "forget",
                "layer": layer_tag(layer),
                "namespace": namespace,
                "key": key,
            }))?;
            Ok(resp
                .get("removed")
                .and_then(|r| r.as_bool())
                .unwrap_or(false))
        }

        fn confirm(
            &self,
            _policy: &pantheon_api::capability::Policy,
            layer: LayerKind,
            namespace: &str,
            key: &str,
        ) -> Result<crate::MemoryRecord, PantheonError> {
            let resp = self.call(&serde_json::json!({
                "op": "confirm",
                "layer": layer_tag(layer),
                "namespace": namespace,
                "key": key,
            }))?;
            let record: crate::MemoryRecord = serde_json::from_value(
                resp.get("record")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .map_err(|e| perr("MEM_PLUGIN_DECODE", format!("confirm record invalid: {e}")))?;
            Ok(self.clamp_record(record))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::{LayerKind, MemoryBackend};

        /// True when `pid` is neither alive nor a zombie. A reaped child has no
        /// `/proc` entry; an unreaped one lingers as `State: Z`, and here the test
        /// process is the parent, so a zombie means the backend leaked it.
        fn child_gone(pid: u32) -> bool {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => true,
                Ok(stat) => !stat.contains(") Z") && !stat.contains(") R") && !stat.contains(") S"),
            }
        }

        fn pid_of(pidfile: &std::path::Path) -> u32 {
            std::fs::read_to_string(pidfile)
                .unwrap()
                .trim()
                .parse()
                .expect("plugin wrote its pid")
        }

        #[test]
        fn stdio_backend_reaps_child_on_success_and_on_decode_error() {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-stdio-reap-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).unwrap();

            // The script has to consume stdin before responding: the backend
            // writes the request right after spawn, and a child that echoes and
            // exits without reading loses that race under load (EPIPE made this
            // flaky once per full-suite run).
            let pidfile = dir.join("ok.pid");
            let script = format!(
                "echo $$ > {}; cat > /dev/null; echo '{{\"ok\":true,\"removed\":true}}'",
                pidfile.display()
            );
            let backend = StdioBackend::new("sh".into(), vec!["-c".into(), script], 5000);
            assert!(backend.forget(LayerKind::Agent, "ns", "k").unwrap());
            assert!(
                child_gone(pid_of(&pidfile)),
                "success-path child must be reaped, not left a zombie"
            );

            // Read/decode-error path. Same stdin handshake and the same reap, so a
            // child that answers badly still gets cleaned up.
            let pidfile = dir.join("bad.pid");
            let script = format!(
                "echo $$ > {}; cat > /dev/null; echo 'not json at all'",
                pidfile.display()
            );
            let backend = StdioBackend::new("sh".into(), vec!["-c".into(), script], 5000);
            assert!(backend.forget(LayerKind::Agent, "ns", "k").is_err());
            assert!(
                child_gone(pid_of(&pidfile)),
                "decode-error-path child must be reaped, not left a zombie"
            );
        }
    }
}

pub mod secret_scan {
    //! Curated secret-pattern scan for the memory write path.
    //!
    //! No regex: simple substring and assignment shapes matched by hand over
    //! lowercased bytes. A hit refuses the write with `SecretDetected`, naming the
    //! class of pattern, never the matched text.
    //!
    //! Narrow on purpose so prose about secrets doesn't trip it: labeled
    //! assignments (`"the password policy requires rotation"` passes,
    //! `"password = hunter2"` fails), known token prefixes plus 8 opaque characters
    //! (`"task-list"` isn't `sk-`), and PEM private-key blocks.
    //!
    //! A tripwire, not a vault: catches the pasted key, not a determined exfil.

    /// (lowercase label, pattern class)
    const LABELED: &[(&str, &str)] = &[
        ("password", "labeled password"),
        ("passwd", "labeled password"),
        ("passphrase", "labeled password"),
        ("api_key", "labeled api key"),
        ("apikey", "labeled api key"),
        ("api-key", "labeled api key"),
        ("client_secret", "labeled client secret"),
        ("secret", "labeled secret"),
        // The boundary check treats `_` as a word character, so `secret` doesn't
        // match inside `aws_secret_access_key` (nor `secret_access_key`, whose left
        // neighbour is `_`). Common enough to list explicitly.
        ("secret_key", "labeled secret"),
        ("secret_access_key", "labeled secret"),
        ("aws_secret_access_key", "labeled secret"),
        ("db_password", "labeled password"),
        ("access_token", "labeled access token"),
        ("auth_token", "labeled auth token"),
        ("token", "labeled token"),
        ("bearer", "bearer token"),
        ("private_key", "private key material"),
    ];

    /// (lowercase prefix, pattern class)
    const PREFIXED: &[(&str, &str)] = &[
        ("sk-", "api key prefix"),
        ("ghp_", "github token prefix"),
        ("gho_", "github token prefix"),
        ("ghu_", "github token prefix"),
        ("gsk_", "api key prefix"),
        ("xoxb-", "slack token prefix"),
        ("xoxp-", "slack token prefix"),
        ("xoxa-", "slack token prefix"),
        ("akia", "aws key id prefix"),
        ("aiza", "google api key prefix"),
        ("-----begin private key-----", "private key block"),
        ("-----begin rsa private key-----", "private key block"),
        ("-----begin openssh private key-----", "private key block"),
        ("-----begin ec private key-----", "private key block"),
    ];

    fn is_word(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || hay.len() < needle.len() {
            return None;
        }
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// `label` at `j` (end of label) is followed by `=`/`:` and a non-empty
    /// value, skipping whitespace/quotes/brackets. `password = ""` and
    /// `{"password": ""}` do not count; `password=hunter2` does.
    fn assignment_follows(lower: &[u8], mut j: usize) -> bool {
        while j < lower.len() && matches!(lower[j], b' ' | b'\t' | b'"' | b'\'' | b'[' | b'(') {
            j += 1;
        }
        if j >= lower.len() || !matches!(lower[j], b'=' | b':') {
            return false;
        }
        j += 1;
        while j < lower.len() && matches!(lower[j], b' ' | b'\t' | b'"' | b'\'') {
            j += 1;
        }
        if j >= lower.len() {
            return false;
        }
        !matches!(
            lower[j],
            b'"' | b'\'' | b'}' | b']' | b')' | b',' | b'\n' | b'\r'
        )
    }

    fn labeled_hit(lower: &[u8]) -> Option<&'static str> {
        for (label, class) in LABELED {
            let lb = label.as_bytes();
            let mut start = 0;
            while let Some(pos) = find_sub(&lower[start..], lb) {
                let i = start + pos;
                let left_ok = i == 0 || !is_word(lower[i - 1]);
                let j = i + lb.len();
                let right_ok = j >= lower.len() || !is_word(lower[j]);
                if left_ok && right_ok && assignment_follows(lower, j) {
                    return Some(class);
                }
                start = i + 1;
            }
        }
        None
    }

    /// Token prefixes need a left word boundary (so `task-list` is not `sk-`)
    /// and at least 8 opaque characters after the prefix. PEM blocks are
    /// distinctive enough to match bare.
    fn prefixed_hit(lower: &[u8]) -> Option<&'static str> {
        for (prefix, class) in PREFIXED {
            let pb = prefix.as_bytes();
            let bare = pb.starts_with(b"-----");
            let mut start = 0;
            while let Some(pos) = find_sub(&lower[start..], pb) {
                let i = start + pos;
                if bare {
                    return Some(class);
                }
                let left_ok = i == 0 || !lower[i - 1].is_ascii_alphanumeric();
                if left_ok {
                    let tail = &lower[i + pb.len()..];
                    let n = tail
                        .iter()
                        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_' || **b == b'-')
                        .count();
                    if n >= 8 {
                        return Some(class);
                    }
                }
                start = i + 1;
            }
        }
        None
    }

    /// Scan `text` for secret patterns. Returns the matched pattern *class*
    /// on a hit, `None` when clean.
    pub(crate) fn detect_secret(text: &str) -> Option<&'static str> {
        // ASCII-lowercased bytes: 1:1 mapping keeps byte indices valid, and
        // every pattern is ASCII.
        let lower: Vec<u8> = text.bytes().map(|b| b.to_ascii_lowercase()).collect();
        labeled_hit(&lower).or_else(|| prefixed_hit(&lower))
    }
}

pub mod store {
    //! SQLite memory provider with FTS recall and provenance.
    //!
    //! Two tables: `memories` and `memories_fts`. Every recalled row carries its
    //! provenance so the agent and `pantheon logs` can see where a belief came from.
    //!
    //! Rows written before trust tiers existed backfill as `memory` on open - they
    //! were human or import authored in practice, and mid-trust is safer than
    //! treating legacy data as authoritative.
    use crate::{LayerKind, MemoryRecord, Proposal, Provenance};
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::provenance::TrustTier;
    use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
    use serde::{Deserialize, Serialize};
    use std::path::Path;
    use std::sync::Mutex;

    fn serr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Memory,
            false,
            cause,
            "check the memory database path and disk space",
            "",
        )
    }

    /// A recall hit: the record plus its rank.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Recalled {
        pub record: MemoryRecord,
        pub rank: f64,
    }

    /// Per-layer byte budgets: the total stored *value* bytes allowed per
    /// (layer, namespace). Enforced in [`MemoryStore::put`] with trust-aware
    /// eviction (lowest trust first, oldest first within a tier).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct LayerBudgets {
        pub global: usize,
        pub agent: usize,
        pub project: usize,
        pub task_session: usize,
    }

    impl Default for LayerBudgets {
        fn default() -> Self {
            Self {
                // Budgets are generous on purpose: they bound growth, not
                // normal use. A few hundred records of prose fit comfortably;
                // a runaway loop pasting tool output does not.
                global: 100 * 1024,
                agent: 200 * 1024,
                project: 200 * 1024,
                task_session: 50 * 1024,
            }
        }
    }

    /// Store-wide memory limits. The native store carries one; callers that
    /// need tighter bounds (tests, constrained agents) use
    /// [`MemoryStore::open_with_config`] / [`MemoryStore::set_config`].
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct MemoryConfig {
        /// Max bytes for a single value - the pre-existing `max_bytes` gate,
        /// now owned by the store config instead of floating at call sites.
        pub max_value_bytes: usize,
        pub budgets: LayerBudgets,
        /// Max recall hits returned per layer by `search_scoped`: one layer's
        /// matches can no longer crowd out narrower layers.
        pub max_recall_per_layer: usize,
    }

    impl Default for MemoryConfig {
        fn default() -> Self {
            Self {
                max_value_bytes: 4096,
                budgets: LayerBudgets::default(),
                max_recall_per_layer: 20,
            }
        }
    }

    impl MemoryConfig {
        /// Byte budget for `layer`, or `None` when the layer is not
        /// budget-enforced (`EphemeralTurn` is never persisted; the gated
        /// write path rejects it before `put`).
        pub fn budget_for(&self, layer: LayerKind) -> Option<usize> {
            match layer {
                LayerKind::Global => Some(self.budgets.global),
                LayerKind::Agent => Some(self.budgets.agent),
                LayerKind::Project => Some(self.budgets.project),
                LayerKind::TaskSession => Some(self.budgets.task_session),
                LayerKind::EphemeralTurn => None,
            }
        }
    }

    #[derive(Debug)]
    pub struct MemoryStore {
        conn: Mutex<Connection>,
        path: Option<std::path::PathBuf>,
        config: Mutex<MemoryConfig>,
    }

    const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  layer TEXT NOT NULL,
  namespace TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  source TEXT NOT NULL,
  origin TEXT NOT NULL,
  trust TEXT NOT NULL DEFAULT 'memory',
  recorded_at_ms INTEGER NOT NULL,
  UNIQUE(layer, namespace, key)
);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
  key, value, namespace, layer, origin, content='memories', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, key, value, namespace, layer, origin)
  VALUES (new.id, new.key, new.value, new.namespace, new.layer, new.origin);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, key, value, namespace, layer, origin)
  VALUES ('delete', old.id, old.key, old.value, old.namespace, old.layer, old.origin);
  INSERT INTO memories_fts(rowid, key, value, namespace, layer, origin)
  VALUES (new.id, new.key, new.value, new.namespace, new.layer, new.origin);
END;";

    /// Backfill for stores created before trust tiers. Legacy rows become
    /// `memory` tier: mid-trust, informative, never authoritative. Idempotent.
    const MIGRATE_TRUST: &str = "
ALTER TABLE memories ADD COLUMN trust TEXT NOT NULL DEFAULT 'memory';";

    /// Add the trust column if the table predates it. `ALTER TABLE ... ADD
    /// COLUMN` with a NOT NULL DEFAULT is instant in SQLite (no table rewrite).
    fn migrate_trust_column(conn: &Connection) -> Result<(), PantheonError> {
        let has_trust: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name='trust'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .map_err(|e| serr("MEM_SCHEMA", e.to_string()))?;
        if !has_trust {
            conn.execute_batch(MIGRATE_TRUST)
                .map_err(|e| serr("MEM_SCHEMA", e.to_string()))?;
        }
        Ok(())
    }

    fn trust_str(t: TrustTier) -> &'static str {
        t.as_str()
    }

    fn trust_from(s: &str) -> TrustTier {
        // A corrupt or unknown trust string is Untrusted. Anything else would
        // promote a tampered or mistyped row into the trusted tier on read.
        TrustTier::parse(s).unwrap_or(TrustTier::Untrusted)
    }

    /// SQL rank of a stored trust tier. `col` is a column reference in the
    /// upsert (`memories.trust` = the row already stored, `excluded.trust` =
    /// the incoming one). Mirrors `TrustTier::rank` in pantheon-api.
    fn sql_rank(col: &str) -> String {
        format!(
        "(CASE WHEN {col} = 'system' THEN 3          WHEN {col} = 'user' THEN 2          WHEN {col} = 'memory' THEN 1          ELSE 0 END)"
    )
    }

    /// The `put` upsert. Trust is a ceiling, not a field the writer sets: a
    /// model-origin write must not clobber a value a human confirmed or reset its
    /// tier.
    ///
    /// The guard is `existing_rank > incoming_rank`, strictly, so a repeat write at
    /// the same tier still overwrites (an agent re-asserting its own note works)
    /// and only a higher-trust write wins. On a blocked write every column is held,
    /// so `put` returns a record and the outcome is a clean no-op.
    fn upsert_memory_sql() -> String {
        let held = sql_rank("memories.trust");
        let incoming = sql_rank("excluded.trust");
        let guard = format!("{held} > {incoming}");
        let keep =
            |col: &str| format!("CASE WHEN {guard} THEN memories.{col} ELSE excluded.{col} END");
        format!(
        "INSERT INTO memories (layer, namespace, key, value, source, origin, trust, recorded_at_ms)\n         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)\n         ON CONFLICT(layer, namespace, key) DO UPDATE SET\n           value={v}, source={s}, origin={o}, trust={t}, recorded_at_ms={ts}",
        v = keep("value"),
        s = keep("source"),
        o = keep("origin"),
        t = keep("trust"),
        ts = keep("recorded_at_ms"),
    )
    }

    /// Map a `memories` row (selected in the order used by `get`) to a record.
    /// Shared by `get` and `put` so the re-read in `put` is identical to a normal
    /// read.
    fn record_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
        Ok(MemoryRecord {
            layer: layer_from(&r.get::<_, String>(0)?),
            namespace: r.get(1)?,
            key: r.get(2)?,
            value: r.get(3)?,
            provenance: Provenance {
                source: r.get(4)?,
                origin: r.get(5)?,
                trust: trust_from(&r.get::<_, String>(6)?),
                recorded_at_ms: r.get(7)?,
            },
        })
    }

    fn layer_str(l: LayerKind) -> &'static str {
        match l {
            LayerKind::Global => "global",
            LayerKind::Agent => "agent",
            LayerKind::Project => "project",
            LayerKind::TaskSession => "task_session",
            LayerKind::EphemeralTurn => "ephemeral_turn",
        }
    }

    fn layer_from(s: &str) -> LayerKind {
        match s {
            "global" => LayerKind::Global,
            "agent" => LayerKind::Agent,
            "project" => LayerKind::Project,
            "task_session" => LayerKind::TaskSession,
            _ => LayerKind::EphemeralTurn,
        }
    }

    /// WAL so a reader never blocks a writer, plus a 5s busy timeout so brief
    /// contention waits instead of failing. The daemon and the CLI share this file.
    fn tune_connection(conn: &Connection) -> Result<(), PantheonError> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(|e| serr("MEM_PRAGMA", e.to_string()))
    }

    /// Delete one row and its FTS entry. Without the explicit 'delete' command the
    /// external-content index keeps the row searchable after the row is gone.
    fn delete_row(
        conn: &Connection,
        id: i64,
        key: &str,
        value: &str,
        namespace: &str,
        layer: &str,
        origin: &str,
    ) -> Result<(), PantheonError> {
        // Same delete-command shape as the `memories_au` trigger.
        conn.execute(
            "INSERT INTO memories_fts(memories_fts, rowid, key, value, namespace, layer, origin)
         VALUES('delete', ?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, key, value, namespace, layer, origin],
        )
        .map_err(|e| serr("MEM_FTS_DELETE", e.to_string()))?;
        conn.execute("DELETE FROM memories WHERE id=?1", params![id])
            .map_err(|e| serr("MEM_DELETE", e.to_string()))?;
        Ok(())
    }

    /// Trust-aware eviction for one (layer, namespace): while stored value bytes
    /// exceed `budget`, delete the lowest-trust row (oldest within a tier), never
    /// `exclude_id`. `length()` on TEXT counts characters, so cast to BLOB.
    fn evict_over_budget(
        conn: &Connection,
        layer: LayerKind,
        namespace: &str,
        budget: usize,
        exclude_id: i64,
    ) -> Result<(), PantheonError> {
        let usage: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(length(CAST(value AS BLOB))),0) FROM memories
             WHERE layer=?1 AND namespace=?2",
                params![layer_str(layer), namespace],
                |r| r.get(0),
            )
            .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
        if usage <= budget as i64 {
            return Ok(());
        }
        let candidates: Vec<(i64, String, String, String, String, String, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT id, key, value, namespace, layer, origin, length(CAST(value AS BLOB))
                 FROM memories WHERE layer=?1 AND namespace=?2 AND id != ?3
                 ORDER BY (CASE trust WHEN 'system' THEN 3 WHEN 'user' THEN 2
                                       WHEN 'memory' THEN 1 ELSE 0 END) ASC,
                          recorded_at_ms ASC, id ASC",
                )
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map(params![layer_str(layer), namespace, exclude_id], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            rows.collect::<Result<_, _>>()
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?
        };
        let mut usage = usage;
        for (id, key, value, ns, lay, origin, bytes) in candidates {
            if usage <= budget as i64 {
                break;
            }
            delete_row(conn, id, &key, &value, &ns, &lay, &origin)?;
            usage -= bytes;
        }
        Ok(())
    }

    impl MemoryStore {
        pub fn open(path: &Path) -> Result<Self, PantheonError> {
            Self::open_with_config(path, MemoryConfig::default())
        }

        /// Open with explicit limits (budgets, recall caps, max value bytes).
        pub fn open_with_config(path: &Path, config: MemoryConfig) -> Result<Self, PantheonError> {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| serr("MEM_MKDIR", e.to_string()))?;
                }
            }
            let conn = Connection::open(path).map_err(|e| serr("MEM_OPEN", e.to_string()))?;
            tune_connection(&conn)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // The data dir is 0700, but memory rows can outlive a copied
                // directory or a loosened umask. Tighten the db and its WAL
                // sidecars the first time they exist, before any row is read.
                for suffix in ["", "-wal", "-shm"] {
                    let mut raw = path.as_os_str().to_os_string();
                    raw.push(suffix);
                    let candidate = std::path::PathBuf::from(raw);
                    if let Ok(meta) = std::fs::metadata(&candidate) {
                        if meta.permissions().mode() & 0o077 != 0 {
                            let _ = std::fs::set_permissions(
                                &candidate,
                                std::fs::Permissions::from_mode(0o600),
                            );
                        }
                    }
                }
            }
            conn.execute_batch(SCHEMA)
                .map_err(|e| serr("MEM_SCHEMA", e.to_string()))?;
            migrate_trust_column(&conn)?;
            Ok(Self {
                conn: Mutex::new(conn),
                path: Some(path.to_path_buf()),
                config: Mutex::new(config),
            })
        }

        pub fn open_in_memory() -> Result<Self, PantheonError> {
            let conn = Connection::open_in_memory().map_err(|e| serr("MEM_OPEN", e.to_string()))?;
            // WAL is a no-op on `:memory:` (SQLite reports `memory`); the
            // busy timeout is still meaningful.
            tune_connection(&conn)?;
            conn.execute_batch(SCHEMA)
                .map_err(|e| serr("MEM_SCHEMA", e.to_string()))?;
            migrate_trust_column(&conn)?;
            Ok(Self {
                conn: Mutex::new(conn),
                path: None,
                config: Mutex::new(MemoryConfig::default()),
            })
        }

        /// Replace the store's limits. Takes effect on the next `put`/`recall`.
        pub fn set_config(&self, config: MemoryConfig) -> Result<(), PantheonError> {
            *self
                .config
                .lock()
                .map_err(|e| serr("MEM_LOCK", e.to_string()))? = config;
            Ok(())
        }

        /// Snapshot of the store's current limits.
        pub fn config(&self) -> Result<MemoryConfig, PantheonError> {
            self.config
                .lock()
                .map(|c| c.clone())
                .map_err(|e| serr("MEM_LOCK", e.to_string()))
        }

        /// Delete a record, returning whether a row went. Lookup and the FTS+row
        /// delete share one transaction, so a crash can't leave a
        /// present-but-unsearchable row.
        pub fn forget(
            &self,
            layer: LayerKind,
            namespace: &str,
            key: &str,
        ) -> Result<bool, PantheonError> {
            let mut conn = self
                .conn
                .lock()
                .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| serr("MEM_DELETE", e.to_string()))?;
            let row: Option<(i64, String, String, String, String, String)> = tx
                .query_row(
                    "SELECT id, key, value, namespace, layer, origin FROM memories
                 WHERE layer=?1 AND namespace=?2 AND key=?3",
                    params![layer_str(layer), namespace, key],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            match row {
                None => Ok(false),
                Some((id, key, value, namespace, layer, origin)) => {
                    delete_row(&tx, id, &key, &value, &namespace, &layer, &origin)?;
                    tx.commit().map_err(|e| serr("MEM_DELETE", e.to_string()))?;
                    Ok(true)
                }
            }
        }

        /// Agent records for a namespace in write order, with each tier. The plain
        /// `list_agent` stays for key/value callers; anything round-tripping through
        /// a file uses this so trust survives.
        pub fn list_agent_meta(
            &self,
            namespace: &str,
        ) -> Result<Vec<(String, String, TrustTier)>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
            let mut stmt = conn
                .prepare(
                    "SELECT key, value, trust FROM memories
                 WHERE layer='agent' AND namespace=?1
                 ORDER BY recorded_at_ms, id",
                )
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map([namespace], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        trust_from(&r.get::<_, String>(2)?),
                    ))
                })
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| serr("MEM_QUERY", e.to_string()))?);
            }
            Ok(out)
        }

        /// List Agent-layer records for a namespace in stable write order.
        pub fn list_agent(&self, namespace: &str) -> Result<Vec<(String, String)>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
            let mut stmt = conn
                .prepare(
                    "SELECT key, value FROM memories
                 WHERE layer='agent' AND namespace=?1
                 ORDER BY recorded_at_ms, id",
                )
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map([namespace], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| serr("MEM_QUERY", e.to_string()))?);
            }
            Ok(out)
        }

        /// Return the backing path, if this store is file-backed.
        pub fn path(&self) -> Option<&Path> {
            self.path.as_deref()
        }

        /// Raise one record's tier. Only raises - demotion is delete + rewrite.
        ///
        /// `layer` is part of the target because the row identity is
        /// (layer, namespace, key); by (namespace, key) alone this could touch
        /// several layers and return an arbitrary one.
        pub fn promote(
            &self,
            layer: LayerKind,
            namespace: &str,
            key: &str,
            tier: pantheon_api::provenance::TrustTier,
        ) -> Result<MemoryRecord, PantheonError> {
            let updated = {
                let conn = self
                    .conn
                    .lock()
                    .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
                // Only raises: the WHERE clause admits tiers strictly below the
                // requested one, so promote(.., Memory) can't rewrite an already-Memory
                // row. Promoting to Untrusted is a no-op - nothing is below it.
                let allowed: &[&str] = match tier {
                    pantheon_api::provenance::TrustTier::System => &["untrusted", "memory", "user"],
                    pantheon_api::provenance::TrustTier::User => &["untrusted", "memory"],
                    pantheon_api::provenance::TrustTier::Memory => &["untrusted"],
                    pantheon_api::provenance::TrustTier::Untrusted => &[],
                };
                let mut updated = 0usize;
                for existing in allowed {
                    updated += conn
                        .execute(
                            "UPDATE memories SET trust=?4
                     WHERE layer=?1 AND namespace=?2 AND key=?3 AND trust=?5",
                            params![layer_str(layer), namespace, key, trust_str(tier), existing],
                        )
                        .map_err(|e| serr("MEM_PUT", e.to_string()))?;
                }
                updated
            };
            if updated == 0 {
                // Distinguish "no such record" from "already at or above the
                // requested tier" so confirm on a user record is a clear
                // no-op message, not a missing-row error.
                return match self.get(layer, namespace, key)? {
                    Some(rec) => Ok(rec),
                    None => Err(serr(
                        "MEM_NOT_FOUND",
                        format!("no promotable record {key} in {namespace}"),
                    )),
                };
            }
            self.get(layer, namespace, key)?
                .ok_or_else(|| serr("MEM_NOT_FOUND", format!("no record {key} in {namespace}")))
        }

        /// Fetch one record by its full identity. By (namespace, key) alone the key
        /// can exist in several layers and the lookup would return an arbitrary row.
        pub fn get(
            &self,
            layer: LayerKind,
            namespace: &str,
            key: &str,
        ) -> Result<Option<MemoryRecord>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
            let mut stmt = conn
                .prepare(
                    "SELECT layer, namespace, key, value, source, origin, trust, recorded_at_ms
                 FROM memories WHERE layer=?1 AND namespace=?2 AND key=?3",
                )
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            let mut rows = stmt
                .query_map(params![layer_str(layer), namespace, key], record_from_row)
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            match rows.next() {
                Some(row) => row.map(Some).map_err(|e| serr("MEM_QUERY", e.to_string())),
                None => Ok(None),
            }
        }

        pub fn put(&self, p: &Proposal) -> Result<MemoryRecord, PantheonError> {
            let budget = self.config()?.budget_for(p.layer);
            // A value bigger than the whole layer budget can never fit: refuse
            // rather than write-then-evict, which would delete the row just written
            // or leave the layer over budget. Non-budgeted layers skip this.
            if let Some(budget) = budget {
                if p.value.len() > budget {
                    return Err(serr(
                        "MEM_BUDGET_EXCEEDED",
                        format!(
                            "value is {} bytes but the {:?} layer budget is {} bytes",
                            p.value.len(),
                            p.layer,
                            budget
                        ),
                    ));
                }
            }
            let mut conn = self
                .conn
                .lock()
                .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
            // Upsert + eviction + re-read in one transaction, so a failure anywhere
            // rolls the whole put back: an evict failure can't leave the row stored
            // while reporting Err, and a crash inside eviction can't leave a
            // present-but-unsearchable row. Immediate mode takes the write lock up
            // front so a second process can't interleave.
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| serr("MEM_PUT", e.to_string()))?;
            tx.execute(
                // Trust is a ceiling, not a field the writer sets. Ranks:
                // untrusted(0) < memory(1) < user(2) < system(3). The guard is
                // strictly `existing > incoming`, so equal rank overwrites (an agent
                // re-asserting its own note works) and only a higher-trust existing
                // row holds. Incoming wins at equal rank.
                &upsert_memory_sql(),
                params![
                    layer_str(p.layer),
                    p.namespace,
                    p.key,
                    p.value,
                    p.provenance.source,
                    p.provenance.origin,
                    trust_str(p.provenance.trust),
                    p.provenance.recorded_at_ms
                ],
            )
            .map_err(|e| serr("MEM_PUT", e.to_string()))?;
            let id: i64 = tx
                .query_row(
                    "SELECT id FROM memories WHERE layer=?1 AND namespace=?2 AND key=?3",
                    params![layer_str(p.layer), p.namespace, p.key],
                    |r| r.get(0),
                )
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            // Bound growth: evict lowest-trust, oldest-first until the layer is
            // back under budget. The row just written is never evicted: `put` must
            // not delete what it just stored.
            if let Some(budget) = budget {
                evict_over_budget(&tx, p.layer, &p.namespace, budget, id)?;
            }
            // Re-read instead of echoing the proposal: when a higher-trust row
            // held, the stored value is the old one and returning `p` would claim
            // the write landed.
            //
            // It runs on the open transaction, not `self.get` - `conn` is a
            // MutexGuard and `get` locks it again, deadlocking a non-reentrant mutex.
            let mut stmt = tx
                .prepare(
                    "SELECT layer, namespace, key, value, source, origin, trust, recorded_at_ms
                 FROM memories WHERE layer=?1 AND namespace=?2 AND key=?3",
                )
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            let mut rows = stmt
                .query_map(
                    params![layer_str(p.layer), p.namespace, p.key],
                    record_from_row,
                )
                .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
            let rec = match rows.next() {
                Some(row) => row.map_err(|e| serr("MEM_QUERY", e.to_string()))?,
                None => {
                    return Err(serr(
                        "MEM_PUT",
                        format!("record vanished after write: {}/{}", p.namespace, p.key),
                    ));
                }
            };
            // Drop the statement before commit: it borrows the transaction.
            drop(rows);
            drop(stmt);
            tx.commit().map_err(|e| serr("MEM_PUT", e.to_string()))?;
            Ok(rec)
        }

        /// FTS recall for one namespace and a set of layers.
        ///
        /// `namespaces` is enforced in SQL. Without the predicate every recall crosses
        /// agent boundaries, and post-filtering would cap `limit` before the filter
        /// ran, letting a busy foreign namespace crowd out the caller's own rows.
        ///
        /// Each layer is queried separately with its own cap (bounded by `limit`), so
        /// one layer can't crowd out the narrower ones.
        pub fn search_scoped(
            &self,
            namespaces: &[&str],
            layers: &[LayerKind],
            query: &str,
            limit: usize,
        ) -> Result<Vec<Recalled>, PantheonError> {
            if query.trim().is_empty() {
                return Ok(Vec::new());
            }
            if namespaces.is_empty() {
                return Ok(Vec::new());
            }
            let fts_query = fts_query_for(query);
            if fts_query.is_empty() {
                return Ok(Vec::new());
            }
            let conn = self
                .conn
                .lock()
                .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
            let per_layer = limit
                .min(
                    self.config
                        .lock()
                        .map_err(|e| serr("MEM_LOCK", e.to_string()))?
                        .max_recall_per_layer,
                )
                .max(1);
            // An empty layer list means "all layers"; a non-empty list is already
            // narrowest-first priority order.
            let effective_layers: Vec<LayerKind> = if layers.is_empty() {
                vec![
                    LayerKind::Global,
                    LayerKind::Agent,
                    LayerKind::Project,
                    LayerKind::TaskSession,
                ]
            } else {
                layers.to_vec()
            };
            let mut out = Vec::new();
            for layer in &effective_layers {
                let mut hits = search_one_layer(&conn, namespaces, &fts_query, *layer, per_layer)?;
                out.append(&mut hits);
            }
            // Narrowest layers first: stable sort by layer priority.
            out.sort_by_key(|r| {
                effective_layers
                    .iter()
                    .position(|l| *l == r.record.layer)
                    .unwrap_or(usize::MAX)
            });
            out.truncate(limit);
            Ok(out)
        }
    }

    /// Token overlap between query and value, so paraphrases sharing words score
    /// even when FTS misses. Lexical fallback, not a vector index - no embeddings.
    pub fn semantic_score(query: &str, value: &str) -> f64 {
        let q: std::collections::HashSet<String> = query
            .split_whitespace()
            .map(|t| t.to_ascii_lowercase())
            .filter(|t| t.len() > 2)
            .collect();
        if q.is_empty() {
            return 0.0;
        }
        let v: std::collections::HashSet<String> = value
            .split_whitespace()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        let hits = q.intersection(&v).count() as f64;
        hits / q.len() as f64
    }

    /// Quote each token for FTS5, stripping query-syntax characters so the
    /// query text cannot inject FTS operators.
    fn fts_query_for(query: &str) -> String {
        query
            .split_whitespace()
            .map(|token| {
                token
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                    .collect::<String>()
            })
            .filter(|token| !token.is_empty())
            .map(|token| format!("\"{token}\""))
            .collect::<Vec<_>>()
            .join(" OR ")
    }

    /// One layer's share of `search_scoped`. `["*"]` is the explicit unrestricted
    /// form, for operator tooling (`pantheon memory recall '*' ...`). An empty list
    /// is a hard refusal: a caller that hasn't established who it reads as gets
    /// nothing rather than everything.
    fn search_one_layer(
        conn: &Connection,
        namespaces: &[&str],
        fts_query: &str,
        layer: LayerKind,
        limit: usize,
    ) -> Result<Vec<Recalled>, PantheonError> {
        // Explicit `?N` indices. Mixing numbered placeholders with bare `?` makes
        // SQLite auto-number the bare ones from 1 and collide, so the bind count
        // stops matching.
        let mut next_index = 2; // ?1 is the FTS match term
        let unrestricted = namespaces == ["*"];
        let ns_clause = if unrestricted {
            String::new()
        } else {
            let idx: Vec<String> = (0..namespaces.len())
                .map(|i| format!("?{}", next_index + i))
                .collect();
            next_index += namespaces.len();
            format!(" AND m.namespace IN ({})", idx.join(","))
        };
        let layer_idx = next_index;
        next_index += 1;
        let sql = format!(
            "SELECT m.layer, m.namespace, m.key, m.value, m.source, m.origin,
            m.trust, m.recorded_at_ms, bm25(memories_fts) AS rank
     FROM memories_fts f
     JOIN memories m ON m.id = f.rowid
     WHERE memories_fts MATCH ?1
       {ns_clause} AND m.layer = ?{layer_idx}
     ORDER BY rank LIMIT ?{next_index}"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| serr("MEM_SEARCH", e.to_string()))?;
        // Bind in *index* order, which is not clause order: the match term
        // is ?1 even though its clause comes after the IN lists.
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        binds.push(Box::new(fts_query.to_string()));
        if !unrestricted {
            for n in namespaces {
                binds.push(Box::new(*n));
            }
        }
        binds.push(Box::new(layer_str(layer)));
        binds.push(Box::new(limit as i64));
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                Ok(Recalled {
                    record: MemoryRecord {
                        layer: layer_from(&r.get::<_, String>(0)?),
                        namespace: r.get(1)?,
                        key: r.get(2)?,
                        value: r.get(3)?,
                        provenance: Provenance {
                            source: r.get(4)?,
                            origin: r.get(5)?,
                            trust: trust_from(&r.get::<_, String>(6)?),
                            recorded_at_ms: r.get(7)?,
                        },
                    },
                    rank: r.get(8)?,
                })
            })
            .map_err(|e| serr("MEM_SEARCH", e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| serr("MEM_SEARCH", e.to_string()))?);
        }
        Ok(out)
    }

    /// Whether the FTS5 sidecar can answer. A freshly created index has no rows and
    /// is healthy; a damaged one raises on `MATCH`, so probing a query is the only
    /// way to tell them apart.
    pub fn fts_health(db: &std::path::Path) -> Result<bool, PantheonError> {
        let conn = Connection::open(db).map_err(|e| serr("MEM_FTS_OPEN", e.to_string()))?;
        let probe: Result<i64, _> = conn.query_row(
        "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'the' OR memories_fts MATCH 'project'",
        [],
        |r| r.get(0),
    );
        match probe {
            Ok(_) => Ok(true),
            // No table yet is normal for a fresh or pre-search store.
            Err(e) if e.to_string().contains("no such table") => Ok(false),
            Err(e) => Err(serr("MEM_FTS_UNUSABLE", e.to_string())),
        }
    }

    /// Full rebuild of the memory FTS index. `memories_fts` is external-content
    /// (`content='memories'`) kept in sync by triggers, so the canonical rows are
    /// already there: drop it, let `SCHEMA` recreate it with its triggers, then
    /// `rebuild`.
    ///
    /// Hand-writing the CREATE is the trap: an external-content table with the wrong
    /// columns still opens and still answers MATCH, so a subtly wrong rebuild passes
    /// every "does it work" check while ranking against a different column set.
    pub fn rebuild_fts(db: &std::path::Path) -> Result<u64, PantheonError> {
        let conn = Connection::open(db).map_err(|e| serr("MEM_FTS_OPEN", e.to_string()))?;
        conn.execute_batch("DROP TABLE IF EXISTS memories_fts;")
            .map_err(|e| serr("MEM_FTS_DROP", e.to_string()))?;
        // SCHEMA is `CREATE ... IF NOT EXISTS` throughout, so re-running it
        // recreates the table *and* the triggers that maintain it.
        conn.execute_batch(SCHEMA)
            .map_err(|e| serr("MEM_FTS_RECREATE", e.to_string()))?;
        // The FTS5 builtin rebuild command repopulates an external-content index
        // from its content table. This is the part a hand-rolled INSERT list would
        // get wrong.
        conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('rebuild');")
            .map_err(|e| serr("MEM_FTS_REBUILD", e.to_string()))?;
        let n: i64 = conn
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .map_err(|e| serr("MEM_COUNT", e.to_string()))?;
        Ok(n as u64)
    }

    /// Drop the FTS sidecar and leave the triggers - the state a partial write or
    /// an interrupted migration leaves. Takes a path rather than a `Connection` so a
    /// consumer can damage a store without the public API growing a raw handle.
    /// Idempotent.
    pub fn damage_fts_for_test(db: &std::path::Path) {
        let conn = Connection::open(db).expect("open memory db");
        conn.execute_batch("DROP TABLE IF EXISTS memories_fts;")
            .expect("drop fts");
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::LayerKind;

        fn proposal(trust: TrustTier) -> Proposal {
            Proposal {
                layer: LayerKind::Agent,
                namespace: "test".to_string(),
                key: "k".to_string(),
                value: "v".to_string(),
                provenance: Provenance {
                    source: "test".to_string(),
                    origin: "test".to_string(),
                    trust,
                    recorded_at_ms: 1,
                },
            }
        }

        #[test]
        fn promote_never_demotes() {
            let store = MemoryStore::open_in_memory().unwrap();
            store.put(&proposal(TrustTier::Memory)).unwrap();
            // "Promoting" to Untrusted must leave the Memory tier intact.
            let rec = store
                .promote(LayerKind::Agent, "test", "k", TrustTier::Untrusted)
                .unwrap();
            assert_eq!(rec.provenance.trust, TrustTier::Memory);
            // A genuine raise still works.
            let rec = store
                .promote(LayerKind::Agent, "test", "k", TrustTier::User)
                .unwrap();
            assert_eq!(rec.provenance.trust, TrustTier::User);
            // And a raise back down to Memory is refused.
            let rec = store
                .promote(LayerKind::Agent, "test", "k", TrustTier::Memory)
                .unwrap();
            assert_eq!(rec.provenance.trust, TrustTier::User);
        }

        #[test]
        fn unknown_trust_fails_closed_to_untrusted() {
            assert_eq!(trust_from("bogus-tier"), TrustTier::Untrusted);
            assert_eq!(trust_from("memory"), TrustTier::Memory);
        }

        #[test]
        fn unknown_layer_fails_closed_to_most_ephemeral() {
            assert_eq!(layer_from("bogus-layer"), LayerKind::EphemeralTurn);
            assert_eq!(layer_from("agent"), LayerKind::Agent);
        }
    }

    #[cfg(test)]
    mod transaction_tests {
        use super::*;

        fn proposal_with(key: &str, value: &str, trust: TrustTier) -> Proposal {
            Proposal {
                layer: LayerKind::Agent,
                namespace: "test".to_string(),
                key: key.to_string(),
                value: value.to_string(),
                provenance: Provenance {
                    source: "test".to_string(),
                    origin: "test".to_string(),
                    trust,
                    recorded_at_ms: 1,
                },
            }
        }

        #[test]
        fn put_with_eviction_keeps_rows_and_fts_in_sync() {
            let store = MemoryStore::open_in_memory().unwrap();
            let mut cfg = MemoryConfig::default();
            cfg.budgets.agent = 10;
            store.set_config(cfg).unwrap();
            store
                .put(&proposal_with("k1", "appleone", TrustTier::Untrusted))
                .unwrap();
            // 8 + 8 bytes > 10 budget: the oldest lowest-trust row (k1) is
            // evicted as part of the same transaction as k2's upsert.
            store
                .put(&proposal_with("k2", "bananaoo", TrustTier::Untrusted))
                .unwrap();
            assert!(store.get(LayerKind::Agent, "test", "k1").unwrap().is_none());
            assert!(store.get(LayerKind::Agent, "test", "k2").unwrap().is_some());
            assert!(
                store
                    .search_scoped(&["test"], &[LayerKind::Agent], "appleone", 10)
                    .unwrap()
                    .is_empty(),
                "evicted row must not stay searchable"
            );
            assert_eq!(
                store
                    .search_scoped(&["test"], &[LayerKind::Agent], "bananaoo", 10)
                    .unwrap()
                    .len(),
                1,
                "surviving row must stay searchable"
            );
        }

        #[test]
        fn failed_put_stores_nothing() {
            // Eviction alone can't be fault-injected in-process (evict SQL only
            // fails on corruption, which fails the upsert trigger first), so this
            // pins the contract at the put boundary: a failed put stores nothing.
            let dir = std::env::temp_dir().join(format!(
                "pantheon-mem-tx-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let path = dir.join("memory.db");
            let store = MemoryStore::open(&path).unwrap();
            damage_fts_for_test(&path);
            let err = store
                .put(&proposal_with("k1", "some value", TrustTier::Untrusted))
                .expect_err("put against a dropped FTS table must fail");
            assert_eq!(err.code, "MEM_PUT");
            assert!(
                store.get(LayerKind::Agent, "test", "k1").unwrap().is_none(),
                "a failed put must not leave a stored row behind"
            );
        }

        #[test]
        fn failed_forget_rolls_back_the_row_delete() {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-mem-tx-forget-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let path = dir.join("memory.db");
            let store = MemoryStore::open(&path).unwrap();
            store
                .put(&proposal_with("k1", "some value", TrustTier::Untrusted))
                .unwrap();
            damage_fts_for_test(&path);
            assert!(store.forget(LayerKind::Agent, "test", "k1").is_err());
            // No half-delete: the FTS delete failed first so the row delete never
            // ran, and the transaction means a crash can't split the ordering
            // either.
            assert!(
                store.get(LayerKind::Agent, "test", "k1").unwrap().is_some(),
                "a failed forget must leave the row intact"
            );
        }

        #[cfg(unix)]
        #[test]
        fn open_tightens_the_memory_db_and_sidecars() {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!(
                "pantheon-mem-perms-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("memory.db");
            {
                let _store = MemoryStore::open(&path).unwrap();
            }
            for suffix in ["", "-wal", "-shm"] {
                let mut raw = path.as_os_str().to_os_string();
                raw.push(suffix);
                let candidate = std::path::PathBuf::from(raw);
                if let Ok(meta) = std::fs::metadata(&candidate) {
                    let mode = meta.permissions().mode() & 0o777;
                    assert_eq!(
                        mode,
                        0o600,
                        "{} must be owner-only, got {mode:o}",
                        candidate.display()
                    );
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
