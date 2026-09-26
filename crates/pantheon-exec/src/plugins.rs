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
use pantheon_core::capability::{Capability, Decision};
use pantheon_core::error::{Layer, PantheonError};
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
pub fn discover_plugins(data_dir: &Path, project_root: &Path) -> Vec<DiscoveredPlugin> {
    let mut out = Vec::new();
    let user_plugins = data_dir.join("plugins");
    scan_dir(&user_plugins, PluginLocation::User, &mut out);
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
/// Returns the path to the executable that the plugin supervisor should
/// spawn. Does NOT execute anything — that happens in the supervisor.
pub fn verify_plugin(plugin: &DiscoveredPlugin) -> Result<PathBuf, PantheonError> {
    let runner = &plugin.manifest.runner;
    let runner_path = plugin.root.join(runner);

    // The runner must be a real file.
    if !runner_path.is_file() {
        return Err(merr(
            "PLUGIN_NO_RUNNER",
            format!("runner '{}' not found in {}", runner, plugin.root.display()),
        ));
    }

    // If it's a script, the first line determines the interpreter.
    if runner.ends_with(".sh") {
        let first = std::fs::read_to_string(&runner_path)
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

    Ok(runner_path)
}

/// Evaluate whether a tool from this plugin is allowed under the current policy.
pub fn tool_allowed(
    plugin: &DiscoveredPlugin,
    tool_name: &str,
    policy: &pantheon_core::capability::Policy,
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
    let dest = data_dir.join("plugins").join(&entry.name);
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
    let tmp_dest = data_dir.join("plugins").join(format!(".{}", entry.name));
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

#[cfg(test)]
#[path = "plugins_tests.rs"]
mod tests;
