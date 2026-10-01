//! Plugin discovery and install for Pantheon's tool plugins.
//!
//! Pantheon discovers plugins from two sources, in this precedence:
//!
//!   1. User:    $PANTHEON_DATA_DIR/plugins/<name>/
//!   2. Project: ./.pantheon/plugins/<name>/   (opt-in via config)
//!
//! Each plugin directory must contain a `manifest.yaml`. Hermes marketplace
//! plugins are a third source — pulled from a catalog JSON — but the catalog
//! adapter lives in the CLI layer; this crate only knows about directory-based
//! plugins and the manifest schema.
//!
//! Discovery only enumerates. Nothing is imported or executed until the
//! plugin is enabled in a session. When enabled, the plugin surface runs as a
//! subprocess that speaks a narrow JSON protocol. Every tool the plugin
//! declares is registered with a capability string from the manifest, so the
//! agent loop's capability gate sees plugin tools exactly like builtins.
//!
//! Security model:
//! - Manifests are parsed before any code runs.
//! - Required env vars are declared upfront; the runtime never injects
//!   secrets into the plugin process — the plugin reads them from its own
//!   environment at startup.
//! - Plugins never get access to the Pantheon config or API keys.
//! - Each tool carries its capability requirement; the loop denies calls that
//!   exceed the session's policy.
use pantheon_api::capability::{Capability, Decision};
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check the plugin manifest",
        "",
    )
}

/// Where a plugin lives on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginLocation {
    User,
    Project,
}

/// A plugin manifest. Describes the plugin, its tools, and what it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Plugin name (also the directory / registry key). Required.
    pub name: String,
    /// Short human description.
    #[serde(default)]
    pub description: String,
    /// Version string. Recommended.
    #[serde(default)]
    pub version: String,
    /// Git commit pin (for catalog installs). Optional.
    #[serde(default)]
    pub sha: Option<String>,
    /// Maintainer name. Optional.
    #[serde(default)]
    pub maintainer: String,
    /// What the plugin needs to run.
    #[serde(default)]
    pub capabilities: Vec<ToolCapability>,
    /// Environment variables the plugin reads at startup.
    /// The runtime does NOT populate these — the user must set them.
    #[serde(default)]
    pub env_vars: Vec<EnvVarDecl>,
    /// Executable to run (relative to the plugin dir, or a shell command).
    /// If absent, defaults to `run.sh`.
    #[serde(default = "default_runner")]
    pub runner: String,
    /// Whether the plugin is enabled by default after install.
    #[serde(default)]
    pub enabled: bool,
}

fn default_runner() -> String {
    "run.sh".into()
}

/// A tool the plugin exposes, plus the capability gate it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapability {
    pub name: String,
    /// Capability a session must grant for this tool to be callable.
    pub capability: Capability,
    /// JSON schema for the tool's arguments (OpenAI function format).
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: serde_json::Value,
}

/// An environment variable the plugin declares it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVarDecl {
    pub name: String,
    /// Whether the plugin can run without this var set (e.g. optional feature).
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

/// Discovered plugin with its location and manifest.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    pub manifest: PluginManifest,
    pub location: PluginLocation,
    pub root: PathBuf,
}

/// Scan for plugins in user and project directories.
///
/// Best-effort bundled seeding runs first: the embedded bundled-plugin
/// registry is materialized into `<data_dir>/plugins/bundled/` (tool
/// plugins) and `<data_dir>/extensions/bundled/` (hook plugins) so a
/// fresh install sees its shipped plugins. A seed failure is reported
/// and ignored — discovery must never break.
pub fn discover_plugins(data_dir: &Path, project_root: &Path) -> Vec<DiscoveredPlugin> {
    if let Err(e) = seed_bundled_plugins(data_dir) {
        eprintln!("bundled plugins: seed failed, continuing with what is on disk: {e}");
    }
    let mut out = Vec::new();
    let user_plugins = data_dir.join("plugins");
    scan_dir(&user_plugins, PluginLocation::User, &mut out);
    // Bundled (first-party) plugins live one level down; `scan_dir` only
    // looks one level deep, so the `bundled/` subdir is scanned
    // explicitly. `is_bundled` recognizes them by path shape.
    scan_dir(
        &user_plugins.join("bundled"),
        PluginLocation::User,
        &mut out,
    );
    let project_plugins = project_root.join(".pantheon").join("plugins");
    scan_dir(&project_plugins, PluginLocation::Project, &mut out);
    // Sort for deterministic ordering.
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    out
}

fn scan_dir(dir: &Path, loc: PluginLocation, out: &mut Vec<DiscoveredPlugin>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Defense in depth: dot-directories (`.quarantine`, `.staging`)
        // are never plugin roots, even though neither contains a
        // manifest.yaml directly today.
        if entry
            .file_name()
            .to_str()
            .map(|n| n.starts_with('.'))
            .unwrap_or(true)
        {
            continue;
        }
        if let Some(p) = load_plugin(&path, loc) {
            out.push(p);
        }
    }
}

/// Load one plugin from a directory. Returns None (and logs to stderr) if
/// the manifest is missing or invalid. A broken plugin never aborts discovery.
pub fn load_plugin(path: &Path, loc: PluginLocation) -> Option<DiscoveredPlugin> {
    let manifest_path = path.join("manifest.yaml");
    match std::fs::read_to_string(&manifest_path) {
        Ok(s) => match serde_yaml::from_str::<PluginManifest>(&s) {
            Ok(manifest) => Some(DiscoveredPlugin {
                manifest,
                location: loc,
                root: path.to_path_buf(),
            }),
            Err(e) => {
                eprintln!("plugin {}: manifest parse error: {e}", path.display());
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("plugin {}: cannot read manifest: {e}", path.display());
            None
        }
    }
}

/// Enable or disable a plugin by rewriting its manifest in place. The YAML
/// is re-serialized from the parsed manifest, so comments are lost but all
/// declared fields survive.
pub fn set_enabled(plugin: &DiscoveredPlugin, enabled: bool) -> Result<(), PantheonError> {
    let mut manifest = plugin.manifest.clone();
    manifest.enabled = enabled;
    let yaml = serde_yaml::to_string(&manifest)
        .map_err(|e| merr("PLUGIN_MANIFEST_WRITE", format!("serialize: {e}")))?;
    let path = plugin.root.join("manifest.yaml");
    // Atomic: write tmp then rename.
    let tmp = plugin.root.join("manifest.yaml.tmp");
    std::fs::write(&tmp, yaml).map_err(|e| merr("PLUGIN_MANIFEST_WRITE", format!("{e}")))?;
    std::fs::rename(&tmp, &path).map_err(|e| merr("PLUGIN_MANIFEST_WRITE", format!("{e}")))?;
    Ok(())
}

/// Install boundary. Verifies the manifest, resolves the runner path, and
/// confirms the plugin directory is self-contained before enabling.
///
/// The manifest `runner` must be a relative path with no `..` components
/// (absolute paths and escapes are rejected), and after symlink
/// resolution it must still sit inside the plugin dir.
///
/// Returns the CANONICALIZED path to the executable that the plugin
/// supervisor should spawn — never the raw manifest-joined path. Spawning
/// the raw path would reopen a symlink race: the containment check above
/// ran against the canonical target, so the raw path must not be used for
/// exec. Callers that spawn via [`crate::supervisor::PluginSupervisor`]
/// should prefer `spawn_verified`, which re-hashes immediately before
/// spawn and fails closed on mismatch. Does NOT execute anything — that
/// happens in the supervisor.
///
/// Approval gate: a third-party plugin that the operator has not explicitly
/// approved is refused here, even if its manifest says `enabled: true`.
/// Bundled (first-party) plugins skip the check. This is the enforcement
/// point for the session spawn path — hand-editing a manifest cannot bypass
/// the consent requirement.
///
/// NOTE on check-then-use: the approval check here hashes the plugin dir
/// at verification time. The directory can change between this check and
/// the supervisor's spawn, so `verify_plugin` alone does not bind the
/// executed bytes — `spawn_verified` closes that gap with a re-hash at
/// spawn time.
pub fn verify_plugin(plugin: &DiscoveredPlugin) -> Result<PathBuf, PantheonError> {
    if !crate::plugin_approval::is_approved(plugin) {
        return Err(merr(
            "PLUGIN_NOT_APPROVED",
            format!(
                "plugin '{}' is third-party and not approved; run `pantheon plugins approve {}` first",
                plugin.manifest.name, plugin.manifest.name
            ),
        ));
    }
    let runner = plugin.manifest.runner.trim();
    if runner.is_empty() {
        return Err(merr(
            "PLUGIN_UNSAFE_RUNNER",
            "manifest has an empty runner".into(),
        ));
    }
    let rel = Path::new(runner);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(merr(
            "PLUGIN_UNSAFE_RUNNER",
            format!("runner {runner:?} must be a relative path without '..'"),
        ));
    }
    let runner_path = plugin.root.join(rel);

    // The runner must be a real file.
    if !runner_path.is_file() {
        return Err(merr(
            "PLUGIN_NO_RUNNER",
            format!("runner '{}' not found in {}", runner, plugin.root.display()),
        ));
    }

    // Containment: resolve symlinks and prove the runner is inside the
    // plugin dir. A manifest pointing at a symlink to /bin/sh would
    // otherwise pass the relative-path check above.
    let canon_root = plugin.root.canonicalize().map_err(|e| {
        merr(
            "PLUGIN_UNSAFE_RUNNER",
            format!("canonicalize {}: {e}", plugin.root.display()),
        )
    })?;
    let canon_runner = runner_path.canonicalize().map_err(|e| {
        merr(
            "PLUGIN_UNSAFE_RUNNER",
            format!("canonicalize {}: {e}", runner_path.display()),
        )
    })?;
    if !canon_runner.starts_with(&canon_root) {
        return Err(merr(
            "PLUGIN_UNSAFE_RUNNER",
            format!("runner {runner:?} escapes the plugin dir"),
        ));
    }

    // If it's a script, the first line determines the interpreter. Read
    // the canonical target, not the raw path: the shebang check must see
    // the bytes that will actually execute.
    if runner.ends_with(".sh") {
        let first = std::fs::read_to_string(&canon_runner)
            .map_err(|e| merr("PLUGIN_READ_RUNNER", format!("{e}")))?
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        if !first.starts_with("#!") {
            return Err(merr(
                "PLUGIN_NO_SHEBANG",
                format!("{} has no shebang line", runner),
            ));
        }
    }

    // Check required env vars are present in the current environment.
    let missing: Vec<&str> = plugin
        .manifest
        .env_vars
        .iter()
        .filter(|v| v.required && std::env::var(&v.name).is_err())
        .map(|v| v.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(merr(
            "PLUGIN_MISSING_ENV",
            format!("required env vars not set: {}", missing.join(", ")),
        ));
    }

    // Return the canonical path, not the raw manifest-joined one: the
    // containment proof above is about THIS path. Spawning the raw path
    // would let a symlink swap between canonicalize and exec escape the
    // plugin dir.
    Ok(canon_runner)
}

/// Evaluate whether a tool from this plugin is allowed under the current policy.
pub fn tool_allowed(
    plugin: &DiscoveredPlugin,
    tool_name: &str,
    policy: &pantheon_api::capability::Policy,
) -> bool {
    let tc = match plugin
        .manifest
        .capabilities
        .iter()
        .find(|t| t.name == tool_name)
    {
        Some(tc) => tc,
        None => return false,
    };
    policy.check(&tc.capability) == Decision::Allow
}

/// Top-level catalog response. Schema matches the Hermes plugin catalog
/// JSON document: `entries` array plus metadata fields we ignore.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogResponse {
    pub entries: Vec<CatalogEntry>,
    #[serde(default)]
    pub removed: Vec<RemovedEntry>,
}

/// A removed catalog entry, refused by the installer.
#[derive(Debug, Clone, Deserialize)]
pub struct RemovedEntry {
    pub name: String,
    #[serde(default)]
    pub reason: String,
}

/// Coerce the catalog's `capabilities` field into a flat Vec<String>.
/// The catalog sends an object like `{"provides_tools": [...]}` but some
/// entries send a flat list, so accept both shapes.
fn deserialize_capabilities<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Cap {
        List(Vec<String>),
        Map(std::collections::HashMap<String, Vec<String>>),
        Null,
    }
    match Cap::deserialize(d)? {
        Cap::List(v) => Ok(v),
        Cap::Map(m) => {
            let mut out = Vec::new();
            for (k, items) in m {
                out.push(k);
                out.extend(items);
            }
            Ok(out)
        }
        Cap::Null => Ok(vec![]),
    }
}

/// One catalog entry. Mirrors the Hermes plugin-catalog schema (subset).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub repo: String,
    pub sha: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub maintainer: String,
    #[serde(default, deserialize_with = "deserialize_capabilities")]
    pub capabilities: Vec<String>,
}

/// Fetch the plugin catalog from the Hermes docs API. Returns the list
/// of catalog entries. Network errors surface as a structured Pantheon
/// error so the CLI can show a helpful message.
pub fn fetch_catalog() -> Result<CatalogResponse, PantheonError> {
    let url = "https://hermes-agent.nousresearch.com/docs/api/plugin-catalog.json";
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .map_err(|e| merr("PLUGIN_CATALOG_FETCH", format!("{e}")))?;
    if !(200..300).contains(&resp.status()) {
        return Err(merr(
            "PLUGIN_CATALOG_HTTP",
            format!("catalog returned HTTP {}", resp.status()),
        ));
    }
    let body = resp
        .into_string()
        .map_err(|e| merr("PLUGIN_CATALOG_BODY", format!("{e}")))?;
    serde_json::from_str::<CatalogResponse>(&body)
        .map_err(|e| merr("PLUGIN_CATALOG_DECODE", format!("{e}")))
}

/// Names of all catalog entries (for error messages / tab-completion hints).
pub fn catalog_names() -> Vec<String> {
    fetch_catalog()
        .map(|c| c.entries.iter().map(|e| e.name.clone()).collect())
        .unwrap_or_default()
}

/// Install destination for a catalog plugin under `<data_dir>/plugins/`.
/// The catalog is a remote document, so the entry name is slug-validated
/// before it touches the filesystem, and the plugins root is canonicalized
/// so a symlinked root cannot redirect the install. Extracted for testing.
fn plugin_install_dir(data_dir: &Path, name: &str) -> Result<PathBuf, PantheonError> {
    if !crate::skills::valid_slug(name) {
        return Err(merr(
            "PLUGIN_BAD_NAME",
            format!("plugin name {name:?}: names must match [a-zA-Z0-9_-]{{1,64}}"),
        ));
    }
    let root = data_dir.join("plugins");
    std::fs::create_dir_all(&root).map_err(|e| merr("PLUGIN_INSTALL_DIR", format!("{e}")))?;
    let canon_root = root
        .canonicalize()
        .map_err(|e| merr("PLUGIN_INSTALL_DIR", format!("{e}")))?;
    // `name` is a validated single safe segment, so joining it onto the
    // canonical root cannot escape.
    Ok(canon_root.join(name))
}

/// Install a catalog plugin: clone the repo at the pinned SHA, write a
/// manifest from the catalog entry, mark it enabled. Does NOT execute
/// anything — verification happens later via `verify_plugin`.
pub fn install_catalog(
    name: &str,
    data_dir: &Path,
    _project_root: &Path,
) -> Result<(), PantheonError> {
    let catalog = fetch_catalog()?;
    if catalog.removed.iter().any(|r| r.name == name) {
        return Err(merr(
            "PLUGIN_REMOVED",
            format!("'{name}' was removed from the catalog"),
        ));
    }
    let entry = catalog
        .entries
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| merr("PLUGIN_NOT_FOUND", format!("'{name}' not in catalog")))?;
    // The name comes from a remote catalog: slug-validate before it
    // touches the filesystem, and resolve under the canonical plugins root.
    let dest = plugin_install_dir(data_dir, &entry.name)?;
    if dest.exists() {
        return Err(merr(
            "PLUGIN_ALREADY_INSTALLED",
            format!("{} is already installed", entry.name),
        ));
    }
    std::fs::create_dir_all(&dest).map_err(|e| merr("PLUGIN_INSTALL_DIR", format!("{e}")))?;
    // Clone the repo (depth 1 for speed), then check out the pinned SHA.
    // We can't use `git clone --branch <sha>` because a SHA is not a branch
    // name; must clone first, then checkout.
    let repo = entry.repo.trim_end_matches(".git");
    let repo = repo.trim_end_matches('/');
    // Clone into a temp dir first, so a failed checkout doesn't leave a
    // half-baked plugin dir.
    let tmp_dest = dest.with_file_name(format!(".{}", entry.name));
    std::fs::create_dir_all(&tmp_dest).map_err(|e| merr("PLUGIN_INSTALL_DIR", format!("{e}")))?;
    let clone_result = std::process::Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("--")
        .arg(repo)
        .arg(&tmp_dest)
        .output()
        .map_err(|e| merr("PLUGIN_CLONE_FAILED", format!("git clone: {e}")))?;
    if !clone_result.status.success() {
        let stderr = String::from_utf8_lossy(&clone_result.stderr).to_string();
        std::fs::remove_dir_all(&tmp_dest).ok();
        return Err(merr("PLUGIN_CLONE_FAILED", format!("git clone: {stderr}")));
    }
    // Fetch the specific commit (shallow fetch of just that sha).
    let fetch = std::process::Command::new("git")
        .arg("fetch")
        .arg("origin")
        .arg(&entry.sha)
        .current_dir(&tmp_dest)
        .output()
        .map_err(|e| merr("PLUGIN_FETCH_SHA", format!("git fetch: {e}")))?;
    if !fetch.status.success() {
        let stderr = String::from_utf8_lossy(&fetch.stderr).to_string();
        std::fs::remove_dir_all(&tmp_dest).ok();
        return Err(merr("PLUGIN_FETCH_SHA", format!("git fetch sha: {stderr}")));
    }
    let checkout = std::process::Command::new("git")
        .arg("checkout")
        .arg(&entry.sha)
        .current_dir(&tmp_dest)
        .output()
        .map_err(|e| merr("PLUGIN_CHECKOUT", format!("git checkout: {e}")))?;
    if !checkout.status.success() {
        let stderr = String::from_utf8_lossy(&checkout.stderr).to_string();
        std::fs::remove_dir_all(&tmp_dest).ok();
        return Err(merr("PLUGIN_CHECKOUT", format!("git checkout: {stderr}")));
    }
    // Move into final location.
    std::fs::rename(&tmp_dest, &dest).map_err(|e| merr("PLUGIN_INSTALL_MOVE", format!("{e}")))?;
    // Write manifest from catalog entry (overwrites any repo manifest
    // so the pinned capabilities are authoritative).
    let manifest = PluginManifest {
        name: entry.name.clone(),
        description: entry.description.clone(),
        version: entry.sha.clone(),
        sha: Some(entry.sha.clone()),
        maintainer: entry.maintainer.clone(),
        capabilities: entry
            .capabilities
            .iter()
            .map(|c| ToolCapability {
                name: c.clone(),
                capability: Capability::Other(c.clone()),
                description: String::new(),
                parameters: serde_json::Value::Null,
            })
            .collect(),
        env_vars: vec![],
        runner: "run.sh".into(),
        enabled: true,
    };
    let yaml = serde_yaml::to_string(&manifest)
        .map_err(|e| merr("PLUGIN_MANIFEST_WRITE", format!("{e}")))?;
    std::fs::write(dest.join("manifest.yaml"), yaml)
        .map_err(|e| merr("PLUGIN_MANIFEST_WRITE", format!("{e}")))?;
    println!("installed {}@{} from {}", entry.name, entry.sha, entry.repo);
    Ok(())
}

// ---------------------------------------------------------------------------
// Bundled plugins: embedded registry, seeding, enable/disable.
// ---------------------------------------------------------------------------
// Pantheon ships plugins inside the binary (`bundled-plugins/<name>/`,
// one directory per plugin holding either `manifest.yaml` for a tool
// plugin or `plugin.yaml` for a hook plugin, plus implementation
// files — text only). The registry is GENERATED by build.rs from those
// directories (`BundledPlugin`, `BundledPluginFile`, `bundled_plugins()`
// below come from OUT_DIR) — adding a plugin is adding a directory, no
// code changes. `canonical_sha256` (the per-plugin digest embedded in
// the registry) is shared with build.rs via `#[path]` include so the
// build-time hash and the seed-time re-hash can never diverge.

/// Canonical content hash, shared with build.rs. See
/// `build-support/plugin_hash.rs` for the canonical form.
#[path = "../build-support/plugin_hash.rs"]
mod plugin_hash;

// Generated by build.rs from bundled-plugins/*/: BundledPlugin,
// BundledPluginFile, bundled_plugins().
include!(concat!(env!("OUT_DIR"), "/bundled_plugins_registry.rs"));

/// Which half of the plugin system a bundled plugin belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundledPluginKind {
    /// A `manifest.yaml` plugin whose runner's tools are projected into
    /// the tool registry behind the capability gate.
    Tool,
    /// A `plugin.yaml` hook plugin fired per hook point by the
    /// extension manager.
    Hook,
}

impl BundledPluginKind {
    /// Stable lowercase token used in config entries and API payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            BundledPluginKind::Tool => "tool",
            BundledPluginKind::Hook => "hook",
        }
    }

    /// Parse the [`BundledPluginKind::as_str`] token back.
    pub fn parse(s: &str) -> Option<BundledPluginKind> {
        match s.trim() {
            "tool" => Some(BundledPluginKind::Tool),
            "hook" => Some(BundledPluginKind::Hook),
            _ => None,
        }
    }
}

/// Minimal parse of a hook plugin's `plugin.yaml` — just the
/// human-facing fields. Unknown fields are ignored (the extensions
/// crate owns the full hook-manifest schema; this crate must not
/// depend on it).
#[derive(Debug, Deserialize)]
struct BundledHookManifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    /// Enabled by default after install (noisegate is the only bundled
    /// plugin that ships on).
    #[serde(default)]
    enabled: bool,
}

/// Human-facing metadata for one embedded bundled plugin, parsed from
/// its embedded manifest. Entries whose manifest does not parse are
/// skipped — a corrupt manifest is a build-time content bug, and it
/// must not poison listing or toggling.
#[derive(Debug, Clone)]
pub struct BundledPluginInfo {
    pub name: String,
    pub kind: BundledPluginKind,
    pub version: String,
    pub description: String,
    /// The manifest's default-enabled flag. The `[plugins.<name>]` config
    /// entry wins when present; when absent this is the default.
    pub enabled: bool,
}

/// Parse one registry entry's embedded manifest into [`BundledPluginInfo`].
fn parse_bundled_info(entry: &BundledPlugin) -> Option<BundledPluginInfo> {
    let kind = BundledPluginKind::parse(entry.kind)?;
    let manifest = entry.files.iter().find(|f| f.path == entry.manifest_file)?;
    let (name, version, description, enabled) = match kind {
        BundledPluginKind::Tool => {
            let m: PluginManifest = serde_yaml::from_str(manifest.content).ok()?;
            (m.name, m.version, m.description, m.enabled)
        }
        BundledPluginKind::Hook => {
            let m: BundledHookManifest = serde_yaml::from_str(manifest.content).ok()?;
            (m.name, m.version, m.description, m.enabled)
        }
    };
    if name.trim().is_empty() {
        return None;
    }
    Some(BundledPluginInfo {
        name,
        kind,
        version,
        description,
        enabled,
    })
}

/// Every bundled plugin Pantheon ships with a parseable manifest,
/// sorted by name. Deterministic order so the CLI, the dashboard, and
/// the TUI toggle screen render the same list.
pub fn bundled_plugin_infos() -> Vec<BundledPluginInfo> {
    let mut out: Vec<BundledPluginInfo> = bundled_plugins()
        .iter()
        .filter_map(parse_bundled_info)
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Materialize the embedded bundled-plugin registry:
///
/// - tool plugins → `<data_dir>/plugins/bundled/<name>/`
/// - hook plugins → `<data_dir>/extensions/bundled/<name>/`
///   (the layout the runtime's extension loader scans: `PANTHEON_EXT_DIR`
///   or `<data_dir>/extensions`)
///
/// Seeding is inert: an existing target directory is never touched, so
/// operator edits are not clobbered. After writing, the materialized
/// directory is re-hashed and compared against the embedded sha256 — on
/// mismatch the directory is deleted and an error is returned (fail
/// closed). Returns the names that were actually seeded.
///
/// Fail-fast: the first seed error aborts the whole call. Callers that
/// must not break on seed problems (e.g. [`discover_plugins`]) ignore
/// the error.
pub fn seed_bundled_plugins(data_dir: &Path) -> Result<Vec<String>, PantheonError> {
    let mut seeded = Vec::new();
    for entry in bundled_plugins() {
        if let Some(name) = seed_bundled_plugin(data_dir, &entry)? {
            seeded.push(name);
        }
    }
    Ok(seeded)
}

/// Seed one registry entry. Returns `Ok(Some(name))` when the plugin
/// was materialized, `Ok(None)` when the target dir already existed
/// (never overwritten).
fn seed_bundled_plugin(
    data_dir: &Path,
    entry: &BundledPlugin,
) -> Result<Option<String>, PantheonError> {
    if !crate::skills::valid_slug(entry.dir_name) {
        return Err(merr(
            "PLUGIN_SEED_NAME",
            format!(
                "bundled plugin dir name {:?} is not a valid slug",
                entry.dir_name
            ),
        ));
    }
    let scope = match entry.kind {
        "tool" => data_dir.join("plugins").join("bundled"),
        "hook" => data_dir.join("extensions").join("bundled"),
        other => {
            return Err(merr(
                "PLUGIN_SEED_KIND",
                format!(
                    "bundled plugin '{}': unknown kind {other:?}",
                    entry.dir_name
                ),
            ))
        }
    };
    let target = scope.join(entry.dir_name);
    if target.exists() {
        // Never overwrite: operator edits or newer installs win.
        return Ok(None);
    }
    // Reject manifest paths that would escape the plugin dir. The
    // embedded paths come from the build-time walk (trusted), but the
    // registry is data — validate anyway.
    for f in entry.files {
        let rel = Path::new(f.path);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(merr(
                "PLUGIN_SEED_PATH",
                format!(
                    "bundled plugin '{}': unsafe embedded path {:?}",
                    entry.dir_name, f.path
                ),
            ));
        }
    }
    // Write to a temp dir first and rename into place, so a crash
    // mid-seed never leaves a half-materialized plugin dir behind.
    let tmp = scope.join(format!(".{}", entry.dir_name));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)
            .map_err(|e| merr("PLUGIN_SEED", format!("clear stale tmp dir: {e}")))?;
    }
    let write_err = |e: std::io::Error| merr("PLUGIN_SEED", format!("{e}"));
    std::fs::create_dir_all(&tmp).map_err(write_err)?;
    let fail = |cause: String| -> PantheonError {
        std::fs::remove_dir_all(&tmp).ok();
        merr("PLUGIN_SEED", cause)
    };
    for f in entry.files {
        let dest = tmp.join(f.path);
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Err(fail(format!("create {}: {e}", parent.display())));
            }
        }
        if let Err(e) = std::fs::write(&dest, f.content) {
            return Err(fail(format!("write {}: {e}", dest.display())));
        }
        // Runner scripts need the exec bit to spawn. Content is
        // unaffected, so this runs before the re-hash below.
        #[cfg(unix)]
        if f.path.ends_with(".sh") {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            if let Err(e) = std::fs::set_permissions(&dest, perms) {
                return Err(fail(format!("chmod {}: {e}", dest.display())));
            }
        }
    }
    // Fail closed: the materialized tree must contain exactly the
    // embedded files, byte-identical. Re-hash what landed on disk and
    // compare against the embedded digest; on mismatch delete and
    // report — never leave a half-written or tampered dir in place.
    let on_disk = dir_file_set(&tmp).map_err(|e| fail(format!("re-read: {e}")))?;
    let embedded: std::collections::BTreeSet<String> =
        entry.files.iter().map(|f| f.path.to_string()).collect();
    if on_disk != embedded {
        return Err(fail(format!(
            "bundled plugin '{}': materialized file set does not match embedded registry",
            entry.dir_name
        )));
    }
    let mut pairs: Vec<(&str, String)> = Vec::with_capacity(entry.files.len());
    for f in entry.files {
        match std::fs::read_to_string(tmp.join(f.path)) {
            Ok(content) => pairs.push((f.path, content)),
            Err(e) => {
                return Err(fail(format!("re-read {}: {e}", f.path)));
            }
        }
    }
    let pair_refs: Vec<(&str, &str)> = pairs.iter().map(|(r, c)| (*r, c.as_str())).collect();
    let actual = plugin_hash::canonical_sha256(&pair_refs);
    if actual != entry.sha256 {
        return Err(fail(format!(
            "bundled plugin '{}': sha256 mismatch after materializing (expected {}, got {actual})",
            entry.dir_name, entry.sha256
        )));
    }
    // The target may have appeared while we were writing (concurrent
    // seed): never overwrite — drop the tmp dir and report "kept".
    if target.exists() {
        std::fs::remove_dir_all(&tmp).ok();
        return Ok(None);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| fail(format!("create {}: {e}", parent.display())))?;
    }
    std::fs::rename(&tmp, &target).map_err(|e| fail(format!("publish: {e}")))?;
    Ok(Some(entry.dir_name.to_string()))
}

/// The set of regular files under `dir`, as `/`-separated relative
/// paths. Any non-regular entry (symlink, socket, …) is an error — a
/// directory this function just wrote contains only regular files, so
/// anything else fails the seed closed.
fn dir_file_set(dir: &Path) -> Result<std::collections::BTreeSet<String>, String> {
    fn walk(
        dir: &Path,
        root: &Path,
        out: &mut std::collections::BTreeSet<String>,
    ) -> Result<(), String> {
        let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read entry: {e}"))?;
            let ft = entry
                .file_type()
                .map_err(|e| format!("stat {}: {e}", entry.path().display()))?;
            // `file_type()` does not follow symlinks: anything that is
            // not a plain file or dir fails closed here.
            if ft.is_symlink() {
                return Err(format!("unexpected symlink {}", entry.path().display()));
            }
            if ft.is_dir() {
                walk(&entry.path(), root, out)?;
            } else if ft.is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|e| format!("relativize: {e}"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel);
            } else {
                return Err(format!("unexpected file type {}", entry.path().display()));
            }
        }
        Ok(())
    }
    let mut out = std::collections::BTreeSet::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

/// Enable or disable a bundled plugin through the `[plugins]` config
/// table — the single enablement state shared by the config file, the
/// dashboard, the TUI, and the agent's `enable_plugin` tool. Works for
/// both tool and hook plugins in the embedded registry: for a bundled
/// plugin the config entry wins over the manifest's own `enabled` flag
/// (see the session's plugin spawn path).
///
/// Only registry names are accepted (`PLUGIN_UNKNOWN_PLUGIN`
/// otherwise) — this is the no-arbitrary-plugin-install boundary.
/// Enabling stamps the registry `kind` and `version` so the entry is
/// self-describing; disabling keeps them and flips the flag. The rest
/// of the config file is preserved (edited as TOML, not re-serialized
/// from the `Config` struct).
pub fn set_bundled_enabled(
    data_dir: &Path,
    name: &str,
    enabled: bool,
) -> Result<(), PantheonError> {
    let info = bundled_plugin_infos()
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| {
            merr(
                "PLUGIN_UNKNOWN_PLUGIN",
                format!(
                    "'{name}' is not a bundled plugin; only bundled plugins can be toggled here"
                ),
            )
        })?;
    write_plugin_enabled(data_dir, name, info.kind.as_str(), &info.version, enabled)
}

/// Core of [`set_bundled_enabled`]: the TOML-document edit. Split out
/// so tests can drive it without a populated registry.
fn write_plugin_enabled(
    data_dir: &Path,
    name: &str,
    kind: &str,
    version: &str,
    enabled: bool,
) -> Result<(), PantheonError> {
    if !crate::skills::valid_slug(name) {
        return Err(merr(
            "PLUGIN_BAD_NAME",
            format!("plugin name {name:?} is not a valid slug"),
        ));
    }
    let path = data_dir.join("config.toml");
    let mut doc: toml::Value = match std::fs::read_to_string(&path) {
        Ok(text) => text.parse().map_err(|e| {
            merr(
                "PLUGIN_CONFIG_PARSE",
                format!("parse {}: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(toml::map::Map::new())
        }
        Err(e) => {
            return Err(merr(
                "PLUGIN_CONFIG_READ",
                format!("read {}: {e}", path.display()),
            ));
        }
    };
    let plugins = doc
        .as_table_mut()
        .ok_or_else(|| {
            merr(
                "PLUGIN_CONFIG_PARSE",
                format!("{} is not a TOML table", path.display()),
            )
        })?
        .entry("plugins")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let entry = plugins
        .as_table_mut()
        .ok_or_else(|| {
            merr(
                "PLUGIN_CONFIG_PARSE",
                "config has a non-table [plugins] value".to_string(),
            )
        })?
        .entry(name)
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let table = entry.as_table_mut().ok_or_else(|| {
        merr(
            "PLUGIN_CONFIG_PARSE",
            format!("[plugins.{name}] is not a table"),
        )
    })?;
    table.insert("enabled".to_string(), toml::Value::Boolean(enabled));
    if enabled {
        table.insert("kind".to_string(), toml::Value::String(kind.to_string()));
        if !version.is_empty() {
            table.insert(
                "version".to_string(),
                toml::Value::String(version.to_string()),
            );
        }
    }
    let text = toml::to_string_pretty(&doc)
        .map_err(|e| merr("PLUGIN_CONFIG_WRITE", format!("serialize: {e}")))?;
    // Atomic: write tmp then rename, so a crash never leaves half a config.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            merr(
                "PLUGIN_CONFIG_WRITE",
                format!("create {}: {e}", parent.display()),
            )
        })?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            merr(
                "PLUGIN_CONFIG_WRITE",
                format!("write {}: {e}", path.display()),
            )
        })?;
    Ok(())
}

// Bundled-plugin unit tests live in a sibling file to keep this module
// readable; they are compiled in only for `cfg(test)`.
#[cfg(test)]
#[path = "plugins_bundled_tests.rs"]
mod bundled_plugin_tests;
