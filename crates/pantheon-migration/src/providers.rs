//! Custom provider detection, and reconciliation of carried credentials
//! against Pantheon's own provider catalog.
//!
//! Two separate problems, deliberately kept apart:
//!
//! - **Custom providers** (`[providers.<name>]` in a Hermes `config.yaml`)
//!   describe an endpoint Pantheon has no catalog entry for. They need a
//!   `[custom_providers.<name>]` section written into `config.toml`.
//! - **Builtin provider keys** are already covered by the credential carry,
//!   because Pantheon's catalog names each provider's key env after the same
//!   `<PROVIDER>_API_KEY` convention the sources use. Nothing needs writing —
//!   but nothing *verifies* it either, so a key that matches no catalog entry
//!   looks identical to one that does. [`reconcile_keys`] is that report.
//!
//! The catalog is read through `pantheon-providers`, so the reconciliation cannot
//! drift from the catalog the runtime actually uses.

use pantheon_providers::catalog;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------
// Custom providers
// ---------------------------------------------------------------------------

/// One custom endpoint, in Pantheon's `[custom_providers.*]` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomProvider {
    /// The Pantheon provider id (table name).
    pub id: String,
    /// Human label, from the source when it had one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Full base URL, no trailing slash.
    pub base_url: String,
    /// `openai` or `anthropic`.
    pub api_mode: String,
    /// Env var **name** holding the key. Never the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// The source's default model for this provider, when it declared one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Models the source advertised, with whatever limits it declared. Most
    /// agent configs list bare names (`glm-5.3-flash: {}`), so limits are
    /// usually absent; where a source does declare them they are carried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<CustomModelRow>,
}

/// One model row from a source `providers:` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomModelRow {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// Parse a Hermes `providers:` block.
///
/// Line-scanned rather than YAML-deserialised, for the same reason the MCP
/// parser is: a full deserialise of the source config would pull unrelated
/// values — including credentials — into a struct, and this function only ever
/// needs the endpoint shape. Returns `(id, label, base_url, key_env,
/// default_model, models)` per entry.
pub fn parse_hermes_providers(config_yaml: &str) -> Vec<CustomProvider> {
    let lines: Vec<&str> = config_yaml.lines().collect();
    let Some(start) = lines
        .iter()
        .position(|l| l.trim_end() == "providers:" || l.trim_end() == "providers: {}")
    else {
        return Vec::new();
    };
    let base = lines[start].len() - lines[start].trim_start().len();

    let mut out: Vec<CustomProvider> = Vec::new();
    let mut cur: Option<CustomProvider> = None;
    // The indent of the `models:` key currently in scope, or `None`. Tracked by
    // indent rather than a boolean because a `models:` block is *not* the last
    // thing in a provider: the real config puts `api_key:` after it, and a
    // boolean that never resets swallows the key and leaves the provider
    // unusable.
    let mut models_indent: Option<usize> = None;

    macro_rules! flush {
        () => {
            if let Some(p) = cur.take() {
                if !p.base_url.is_empty() {
                    out.push(p);
                }
            }
        };
    }

    for line in &lines[start + 1..] {
        let t = line.trim_end();
        let s = t.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        let indent = t.len() - t.trim_start().len();
        if indent <= base {
            break; // block ended
        }

        // A new provider at the first level under `providers:`.
        if indent == base + 2 && s.ends_with(':') {
            flush!();
            // A new provider always starts a fresh `models:` scope.
            models_indent = None;
            let id = s.trim_end_matches(':').trim().trim_matches('"').to_string();
            cur = Some(CustomProvider {
                id,
                label: String::new(),
                base_url: String::new(),
                api_mode: "openai".into(),
                key_env: None,
                default_model: None,
                models: Vec::new(),
            });
            continue;
        }
        let Some(p) = cur.as_mut() else { continue };

        // Inside `models:`, but only while indented deeper than the key. The
        // first line at or above that indent ends the block.
        if let Some(mi) = models_indent {
            if indent > mi {
                if let Some((k, v)) = s.split_once(':') {
                    let k = k.trim().trim_matches('"').to_string();
                    if !k.is_empty() && !p.models.iter().any(|m| m.id == k) {
                        p.models.push(CustomModelRow {
                            id: k,
                            context_limit: number_field(v, "context_window"),
                            max_output_tokens: number_field(v, "max_tokens"),
                        });
                    }
                }
                continue;
            }
            models_indent = None;
        }

        if s == "models:" {
            models_indent = Some(indent);
            continue;
        }
        let Some((k, v)) = s.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        match k {
            "name" => p.label = unquote(v),
            // Hermes spells the base URL `api:` in some entries and
            // `base_url:` in others.
            "api" | "base_url" | "baseUrl" => p.base_url = unquote(v),
            "api_key" | "apiKey" | "key_env" | "keyEnv" => {
                p.key_env = indirection(v).or_else(|| {
                    // A literal key is reported by presence only; the value
                    // never becomes a key_env.
                    if v.is_empty() {
                        None
                    } else {
                        Some("<literal: not migrated>".into())
                    }
                })
            }
            // `model:` is the default in the real config; `default_model` is
            // the other spelling seen in the wild.
            "model" | "default_model" | "defaultModel" => {
                let m = unquote(v);
                if !m.is_empty() {
                    p.default_model = Some(m);
                }
            }
            // `api_mode` is Pantheon's spelling; Hermes infers it from `api:`.
            "api_mode" | "apiMode" => p.api_mode = normalise_mode(v),
            _ => {}
        }
    }
    flush!();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// A numeric field from a source model row, if it declared one. Sources write
/// these as `context_window` / `contextWindow` / `max_tokens` / `maxTokens`.
fn number_field(v: &str, key: &str) -> Option<u32> {
    let mut v = v.trim();
    if v.is_empty() || v == "{}" {
        return None;
    }
    // Accept `context_window: 500000` inside an inline row too.
    let lower = v.to_ascii_lowercase();
    let want = key.to_ascii_lowercase();
    if let Some(i) = lower.find(&want) {
        v = v[i + want.len()..]
            .trim_start()
            .trim_start_matches(':')
            .trim();
    }
    let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn normalise_mode(v: &str) -> String {
    if v.trim().eq_ignore_ascii_case("anthropic") {
        "anthropic".into()
    } else {
        "openai".into()
    }
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_string()
}

/// The variable name inside a `${VAR}` reference.
fn indirection(v: &str) -> Option<String> {
    let v = v.trim();
    let inner = v.strip_prefix("${")?.strip_suffix('}')?;
    let n = inner.trim();
    if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(n.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Catalog reconciliation
// ---------------------------------------------------------------------------

/// How one carried credential relates to Pantheon's provider catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyMatch {
    /// The env var name is exactly a catalog provider's `key_env`, so the
    /// carried key is already usable as-is.
    Catalogued,
    /// A catalog provider exists for this key under a *different* env name.
    /// The carried key needs renaming to be picked up.
    NeedsRename { catalog_env: String },
    /// No catalog provider claims this env var. It may be a service token
    /// (channel, MCP) or a provider Pantheon has no entry for.
    Unmatched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyReport {
    pub env_var: String,
    pub match_kind: KeyMatch,
    /// The catalog provider id, when one applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// The catalog's provider id -> key env, read live from `pantheon-providers`
/// so it cannot drift from what the runtime resolves.
pub fn catalog_key_envs() -> Vec<(String, String)> {
    catalog::providers()
        .iter()
        .filter(|p| !p.key_env.trim().is_empty())
        .map(|p| (p.id.clone(), p.key_env.trim().to_string()))
        .collect()
}

/// Classify one carried env var against the catalog.
pub fn reconcile_key(env_var: &str, catalog_envs: &[(String, String)]) -> KeyReport {
    if let Some((id, _)) = catalog_envs.iter().find(|(_, e)| e == env_var) {
        return KeyReport {
            env_var: env_var.to_string(),
            match_kind: KeyMatch::Catalogued,
            provider: Some(id.clone()),
        };
    }
    // A catalog provider whose name matches the variable's stem, but which
    // expects a different variable. `XAI_API_KEY` vs a catalog `key_env` of
    // `XAI_KEY` is the shape this catches.
    let stem = env_var
        .trim_end_matches("_API_KEY")
        .trim_end_matches("_TOKEN")
        .trim_end_matches("_KEY")
        .to_ascii_uppercase();
    if !stem.is_empty() && stem.len() > 2 {
        for (id, kenv) in catalog_envs {
            let kstem = kenv
                .trim_end_matches("_API_KEY")
                .trim_end_matches("_TOKEN")
                .trim_end_matches("_KEY")
                .to_ascii_uppercase();
            if kstem == stem {
                return KeyReport {
                    env_var: env_var.to_string(),
                    match_kind: KeyMatch::NeedsRename {
                        catalog_env: kenv.clone(),
                    },
                    provider: Some(id.clone()),
                };
            }
        }
    }
    KeyReport {
        env_var: env_var.to_string(),
        match_kind: KeyMatch::Unmatched,
        provider: None,
    }
}

/// Reconcile a whole set of carried env vars.
pub fn reconcile_keys(env_vars: &[String]) -> Vec<KeyReport> {
    let cat = catalog_key_envs();
    env_vars.iter().map(|e| reconcile_key(e, &cat)).collect()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render `[custom_providers.<id>]` sections in Pantheon's TOML shape.
///
/// A provider whose key was a literal in the source is emitted with the
/// `key_env` line **omitted** and a comment saying so, rather than with an
/// invented env var that would resolve to nothing.
pub fn render_custom_providers(providers: &[CustomProvider]) -> String {
    let mut s = String::new();
    s.push_str("# Generated by `pantheon migrate apply` from a source agent config.\n");
    s.push_str("# Review before merging into <data_dir>/config.toml.\n");
    s.push_str("# No model list is written: pantheon fetches the live list from the\n");
    s.push_str("# endpoint, and records a model here only once you name one.\n\n");
    for p in providers {
        s.push_str(&format!("[custom_providers.{}]\n", toml_key(&p.id)));
        s.push_str(&format!("base_url = {:?}\n", p.base_url));
        s.push_str(&format!("api_mode = {:?}\n", p.api_mode));
        match &p.key_env {
            Some(k) if !k.starts_with('<') => s.push_str(&format!("key_env = {k:?}\n")),
            _ => {
                s.push_str(
                    "# key_env: the source stored a literal key, not a variable reference.\n",
                );
                s.push_str(&format!(
                    "# Supply it with `pantheon provider add --name {} --key ...`.\n",
                    p.id
                ));
            }
        }
        if p.default_model.is_some() {
            s.push_str(&format!(
                "# default model in source: {}\n",
                p.default_model.as_deref().unwrap_or("")
            ));
        }
        // Deliberately no `models = [...]` here. A source agent config's
        // model list is a snapshot of a third-party endpoint taken whenever
        // that agent last synced — for an aggregator it is stale almost
        // immediately. Pantheon fetches the live list from the endpoint on
        // demand (`pantheon provider models <name>`) and records a model only
        // once the operator has actually named one.
        if !p.models.is_empty() {
            s.push_str(&format!(
                "# the source advertised {} model id(s); not written down.\n\
                 # Pantheon fetches the live list from the endpoint instead, and\n\
                 # records a model here only when you name one yourself.\n\
                 # See: pantheon provider models {}\n",
                p.models.len(),
                p.id
            ));
        }
        s.push('\n');
    }
    s
}

/// Bare or quoted TOML key, quoted when it is not a bare key.
fn toml_key(k: &str) -> String {
    let bare = !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        k.to_string()
    } else {
        format!("{k:?}")
    }
}

/// A one-line human summary of a provider set.
pub fn summarise(providers: &[CustomProvider]) -> String {
    if providers.is_empty() {
        return "no custom providers".to_string();
    }
    let with_key = providers
        .iter()
        .filter(|p| p.key_env.as_deref().is_some_and(|k| !k.starts_with('<')))
        .count();
    let models: usize = providers.iter().map(|p| p.models.len()).sum();
    format!(
        "{} custom provider(s), {with_key} with a key variable, {models} model id(s) seen in the source (not written down)",
        providers.len()
    )
}

/// Read a provider set from a source config file, or an empty set.
pub fn read_providers(path: &Path) -> Vec<CustomProvider> {
    std::fs::read_to_string(path)
        .map(|b| parse_hermes_providers(&b))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Where the reviewable sidecar goes.
pub fn provider_sidecar_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("providers").join("imported.toml")
}

/// Write the sidecar. This is the **default** destination: a provider is
/// written somewhere reviewable, never straight into the live config.
pub fn write_provider_sidecar(
    data_dir: &Path,
    source: &str,
    providers: &[CustomProvider],
) -> Result<std::path::PathBuf, pantheon_api::error::PantheonError> {
    use pantheon_api::error::{Layer, PantheonError};
    let dir = data_dir.join("providers");
    std::fs::create_dir_all(&dir).map_err(|e| {
        PantheonError::new(
            "MIGRATE_PROV_MKDIR",
            Layer::Runtime,
            false,
            e.to_string(),
            "check the data dir is writable",
            source,
        )
    })?;
    let path = provider_sidecar_path(data_dir);
    let body = format!(
        "# Source: {source}\n# Review, then merge with:\n#   pantheon provider add --name <id> --base-url <url> --key-env <env>\n{}",
        render_custom_providers(providers)
    );
    std::fs::write(&path, body).map_err(|e| {
        PantheonError::new(
            "MIGRATE_PROV_WRITE",
            Layer::Runtime,
            false,
            e.to_string(),
            "check the data dir is writable",
            source,
        )
    })?;
    Ok(path)
}

/// Merge provider sections into an existing `config.toml` **body**, without
/// clobbering anything.
///
/// A text-level merge, not a TOML round-trip: re-serialising a user's
/// `config.toml` would reorder and reformat every table in it, and a
/// migration should leave the file it did not write byte-identical outside the
/// sections it adds. An existing `[custom_providers.<id>]` is **skipped**, not
/// replaced, and reported.
pub fn merge_into_config(
    existing: &str,
    providers: &[CustomProvider],
) -> (String, Vec<String>, Vec<String>) {
    let mut added = Vec::new();
    let mut skipped = Vec::new();
    let existing_ids: Vec<String> = existing
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("[custom_providers.")
                .and_then(|r| r.strip_suffix(']'))
                .map(|k| k.trim().trim_matches('"').to_string())
        })
        .collect();

    let mut out = String::new();
    if !existing.ends_with('\n') && !existing.is_empty() {
        out.push('\n');
    }
    out.push_str(existing);
    if !existing.contains("[custom_providers.") {
        out.push_str("\n# --- added by `pantheon migrate apply` ---\n");
    }

    for p in providers {
        if existing_ids.iter().any(|e| e == &p.id) {
            skipped.push(p.id.clone());
            continue;
        }
        out.push_str(&format!("\n[custom_providers.{}]\n", toml_key(&p.id)));
        out.push_str(&format!("base_url = {:?}\n", p.base_url));
        out.push_str(&format!("api_mode = {:?}\n", p.api_mode));
        if let Some(k) = p.key_env.as_deref().filter(|k| !k.starts_with('<')) {
            out.push_str(&format!("key_env = {k:?}\n"));
        }
        // See `render_custom_providers`: no harvested model list. A model
        // row appears here only after the operator names one, via
        // `pantheon model` (which records it) — never copied from a source
        // config's stale snapshot.
        added.push(p.id.clone());
    }
    (out, added, skipped)
}

/// Merge into the live `<data_dir>/config.toml`, backing it up first.
pub fn merge_into_live_config(
    data_dir: &Path,
    providers: &[CustomProvider],
) -> Result<(std::path::PathBuf, Vec<String>, Vec<String>), pantheon_api::error::PantheonError> {
    use pantheon_api::error::{Layer, PantheonError};
    if providers.is_empty() {
        return Ok((data_dir.join("config.toml"), Vec::new(), Vec::new()));
    }
    let path = data_dir.join("config.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let (merged, added, skipped) = merge_into_config(&existing, providers);
    if added.is_empty() {
        return Ok((path, added, skipped));
    }
    // Pre-image beside the file, so a bad merge is one `mv` from undone.
    if !existing.is_empty() {
        let bak = path.with_extension("toml.pre-migrate");
        std::fs::write(&bak, &existing).map_err(|e| {
            PantheonError::new(
                "MIGRATE_PROV_BACKUP",
                Layer::Runtime,
                false,
                e.to_string(),
                "check the data dir is writable",
                path.to_string_lossy(),
            )
        })?;
    }
    std::fs::write(&path, &merged).map_err(|e| {
        PantheonError::new(
            "MIGRATE_PROV_MERGE",
            Layer::Runtime,
            false,
            e.to_string(),
            "revert from config.toml.pre-migrate, or use `pantheon provider add`",
            path.to_string_lossy(),
        )
    })?;
    Ok((path, added, skipped))
}
