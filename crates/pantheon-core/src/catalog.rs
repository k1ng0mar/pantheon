//! Provider + model catalog (§5 + §14): where a provider lives, which wire
//! format it speaks, and what a model can do — context limit, tool support,
//! vision, reasoning, streaming, cost.
//!
//! Loaded from a YAML file at runtime. Default: `catalog.yaml` embedded at
//! compile time via `include_str!`. Override: `PANTHEON_CATALOG=/path/to.yaml`.
//! Static data; the runtime reads it, agents never choose from it.

use serde::{Deserialize, Serialize};
use std::sync::{OnceLock, RwLock};

/// Wire format a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiMode {
    /// OpenAI chat-completions shape (`/chat/completions`).
    #[serde(rename = "openai")]
    OpenAi,
    /// Anthropic Messages shape (`/messages`).
    #[serde(rename = "anthropic")]
    Anthropic,
}

/// List prices in USD per million tokens. `None` = unknown / free / local.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelCost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok_usd: Option<f64>,
}

impl ModelCost {
    pub fn estimate(&self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        let i = self.input_per_mtok_usd?;
        let o = self.output_per_mtok_usd?;
        Some(input_tokens as f64 * i / 1_000_000.0 + output_tokens as f64 * o / 1_000_000.0)
    }
}

/// Catalog row: where the provider lives, which wire it speaks, key env vars.
/// A row that has `auto: true` and `models: [...]` is a full provider entry;
/// a row that has just a `base_url` and no `auto` is a known endpoint you can
/// point at but no curated models yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderMeta {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub api_mode: ApiMode,
    /// Env var that overrides the base URL, e.g. `PANTHEON_BASE_OPENAI`.
    #[serde(default)]
    pub base_env: String,
    /// Env var that overrides the API key, e.g. `PANTHEON_KEY_OPENAI`.
    #[serde(default)]
    pub key_env: String,
    /// HTTP header carrying the key. Default `Authorization` (sent as
    /// `Bearer <key>`). Some vendors differ — Xiaomi MiMo wants the raw
    /// key in an `api-key` header — so this is per-provider data, not a
    /// protocol assumption. Any non-Authorization name sends the raw key.
    #[serde(default = "default_auth_header")]
    pub key_header: String,
    /// Curated model rows for this provider (catalog says which models are
    /// tested and their capabilities). Empty means "no curated models,
    /// use the generic OpenAI/Anthropic adapter with whatever model name
    /// you pass at runtime".
    #[serde(default)]
    pub models: Vec<ModelMeta>,
    /// Set true for prominent labs that should show in the picker UI.
    /// Defaults to false (curated but hidden).
    #[serde(default)]
    pub prominent: bool,
    /// A development-only endpoint, not a product provider. The local
    /// llm-router is one: it is how the e2e suite reaches a model, and
    /// offering it in a user's setup wizard would be offering a
    /// `127.0.0.1` process that is usually not running. Hidden from every
    /// picker; still resolvable by id, so the dev path keeps working.
    #[serde(default)]
    pub dev: bool,
    /// Short blurb for the picker UI ("Chinese lab", "fast inference", ...).
    #[serde(default)]
    pub tag: String,
}

/// Static metadata for one model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMeta {
    /// Filled from the parent provider's id at load time; skipped in YAML.
    #[serde(skip)]
    pub provider: String,
    pub model: String,
    /// Context window in tokens; `None` = unknown.
    #[serde(default)]
    pub context_limit: Option<u32>,
    /// Provider-imposed max output tokens; `None` = unknown/default.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub tools: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default = "default_true")]
    pub streaming: bool,
    #[serde(default)]
    pub cost: ModelCost,
}

fn default_true() -> bool {
    true
}

fn default_auth_header() -> String {
    "Authorization".into()
}

/// The full catalog: one slice of providers, each with their model rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub providers: Vec<ProviderMeta>,
}

/// Embedded default catalog: keeps the binary self-sufficient.
const DEFAULT_CATALOG: &str = include_str!("../catalog.yaml");

static CATALOG: OnceLock<Catalog> = OnceLock::new();

/// Load the catalog. Resolution order:
/// 1. `PANTHEON_CATALOG` env var → file path
/// 2. `<data_dir>/catalog.yaml` if it exists
/// 3. Embedded default (compile-time)
fn load_catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        let raw = if let Ok(p) = std::env::var("PANTHEON_CATALOG") {
            std::fs::read_to_string(&p).ok()
        } else {
            None
        };
        let text = raw.as_deref().unwrap_or(DEFAULT_CATALOG);
        match serde_yaml::from_str::<Catalog>(text) {
            Ok(mut c) => {
                for p in &mut c.providers {
                    let id = p.id.clone();
                    for m in &mut p.models {
                        m.provider = id.clone();
                    }
                }
                c
            }
            Err(e) => {
                eprintln!("catalog parse error: {}", e);
                Catalog::default()
            }
        }
    })
}

/// Catalog accessor. Cheap to call; cached.
pub fn catalog() -> &'static Catalog {
    load_catalog()
}

/// User-defined providers from `[custom_providers.*]` in config.toml.
/// Registered at CLI startup; consulted by every lookup below so custom
/// endpoints behave exactly like cataloged ones.
static CUSTOM: RwLock<Vec<ProviderMeta>> = RwLock::new(Vec::new());

/// Register (or replace, by id) a user-defined provider. Idempotent.
pub fn register_custom_provider(meta: ProviderMeta) {
    if let Ok(mut customs) = CUSTOM.write() {
        if let Some(slot) = customs.iter_mut().find(|p| p.id == meta.id) {
            *slot = meta;
        } else {
            customs.push(meta);
        }
    }
}

/// Iterate providers.
pub fn providers() -> &'static [ProviderMeta] {
    &catalog().providers
}

/// All providers: cataloged plus user-registered customs. The picker and
/// the `providers` verb use this; the runtime path (`provider`) resolves
/// either way.
pub fn all_providers() -> Vec<ProviderMeta> {
    let mut out: Vec<ProviderMeta> = catalog().providers.clone();
    if let Ok(customs) = CUSTOM.read() {
        out.extend(customs.iter().cloned());
    }
    out
}

/// Every provider a user can actually choose, which is every provider minus
/// the development-only ones. A setup wizard that offers `127.0.0.1` is
/// offering a process that is not running; the row can stay in the catalog
/// and still be absent from every picker.
pub fn selectable_providers() -> Vec<ProviderMeta> {
    all_providers().into_iter().filter(|p| !p.dev).collect()
}

/// Iterate all model rows across all providers.
pub fn models() -> Vec<&'static ModelMeta> {
    catalog()
        .providers
        .iter()
        .flat_map(|p| p.models.iter())
        .collect()
}

/// Look up a provider by id (cataloged or user-registered custom).
pub fn provider(id: &str) -> Option<ProviderMeta> {
    if let Ok(customs) = CUSTOM.read() {
        if let Some(p) = customs.iter().find(|p| p.id == id) {
            return Some(p.clone());
        }
    }
    providers().iter().find(|p| p.id == id).cloned()
}

/// Look up a model's static metadata (exact provider + model match).
pub fn model(provider_id: &str, model_id: &str) -> Option<&'static ModelMeta> {
    models()
        .into_iter()
        .find(|m| m.provider == provider_id && m.model == model_id)
}

/// Model metadata with conservative defaults for unknown models:
/// context unknown, tools on, vision off, reasoning off, streaming on,
/// cost unknown.
pub fn model_meta(provider_id: &str, model_id: &str) -> ModelMeta {
    if let Some(m) = model(provider_id, model_id) {
        return m.clone();
    }
    ModelMeta {
        provider: provider_id.to_string(),
        model: model_id.to_string(),
        context_limit: None,
        max_output_tokens: None,
        tools: true,
        vision: false,
        reasoning: false,
        streaming: true,
        cost: ModelCost::default(),
    }
}

/// Base URL for a provider: env override → catalog → the provider id
/// itself (treated as a full base URL, legacy passthrough).
/// NOTE: may still contain `{var}` template placeholders — use
/// [`resolve_base_url`] when building requests.
pub fn base_url_for(provider_id: &str) -> String {
    if let Some(p) = provider(provider_id) {
        if !p.base_env.is_empty() {
            if let Ok(u) = std::env::var(&p.base_env) {
                if !u.is_empty() {
                    return u;
                }
            }
        }
        return p.base_url.clone();
    }
    if let Ok(u) = std::env::var(format!("PANTHEON_BASE_{}", provider_id.to_uppercase())) {
        if !u.is_empty() {
            return u;
        }
    }
    // Legacy passthrough: a custom provider's id IS its base URL. That is
    // only true if the id is actually shaped like one. Without this check a
    // typo such as `provider = "http"` became the literal base URL "http",
    // so the request went to the relative path "http/chat/completions",
    // failed as a retryable network error, and the chain reported
    // PROVIDER_EXHAUSTED. The real cause -- an unknown provider -- never
    // reached the user, and `doctor` called the config valid.
    if provider_id.starts_with("http://") || provider_id.starts_with("https://") {
        return provider_id.to_string();
    }
    // Unknown id that is not a URL. resolve_base_url turns this into a
    // PROVIDER_CONFIG error naming the bad id, which is non-retryable and so
    // surfaces directly instead of collapsing into PROVIDER_EXHAUSTED.
    String::new()
}

/// Placeholder vars in a template base URL: `{resource}` → `["resource"]`.
/// Doubled braces are not templates; empty `{}` is ignored.
pub fn template_vars(base_url: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = base_url.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = base_url[i..].find('}') {
                let name = base_url[i + 1..i + end].trim();
                if !name.is_empty()
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    && !out.iter().any(|n: &String| n == name)
                {
                    out.push(name.to_string());
                }
                i += end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Env-safe fragment: `my-llm` → `MY_LLM`. Shared by
/// [`config_env_name`] and the CLI's `sanitize_env_suffix` so both build
/// the same `PANTHEON_*` names.
pub fn env_part(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Env var holding one template value: provider `azure` + var `resource`
/// → `PANTHEON_AZURE_RESOURCE`. Values live in `<data_dir>/.env`
/// (written by `pantheon model`), same as API keys.
pub fn config_env_name(provider_id: &str, var: &str) -> String {
    format!("PANTHEON_{}_{}", env_part(provider_id), env_part(var))
}

/// Required template vars for a provider id (empty = ready to use).
pub fn required_config_vars(provider_id: &str) -> Vec<String> {
    template_vars(&base_url_for(provider_id))
}

/// Resolve a provider's effective base URL, interpolating `{var}`
/// placeholders from `PANTHEON_<PROVIDER>_<VAR>` env values. Errors name
/// every missing var and point at `pantheon model` — never let a raw
/// `{placeholder}` reach the wire.
pub fn resolve_base_url(provider_id: &str) -> Result<String, String> {
    let base = base_url_for(provider_id);
    if base.is_empty() {
        return Err(format!(
            "unknown provider {provider_id:?}: it is not in the catalog and is not a URL. \
             run `pantheon model` to add it, or set PANTHEON_BASE_{} to its base URL",
            provider_id.to_uppercase()
        ));
    }
    resolve_template(provider_id, &base)
}

/// Resolve an explicit base string with the provider's env namespace.
/// Same as [`resolve_base_url`] but for a URL not (yet) in the catalog —
/// the `pantheon model` flow uses this before the custom row is saved.
pub fn resolve_template(provider_id: &str, base: &str) -> Result<String, String> {
    let vars = template_vars(base);
    if vars.is_empty() {
        return Ok(base.to_string());
    }
    let mut out = base.to_string();
    let mut missing = Vec::new();
    for v in &vars {
        let env_name = config_env_name(provider_id, v);
        match std::env::var(&env_name) {
            Ok(val) if !val.trim().is_empty() => {
                out = out.replace(&format!("{{{v}}}"), val.trim());
            }
            _ => missing.push(format!("{env_name} (for {{{v}}})")),
        }
    }
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "provider {provider_id:?} needs {} — run `pantheon model` to fill them (stored in <data_dir>/.env)",
            missing.join(", ")
        ))
    }
}

/// HTTP header carrying the key for a provider (`Authorization` unless
/// the row says otherwise). Consults customs too.
pub fn key_header_for(provider_id: &str) -> String {
    provider(provider_id)
        .map(|p| p.key_header)
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(default_auth_header)
}

/// API key for a provider: env override → `fallback` (the configured key).
pub fn key_for(provider_id: &str, fallback: &str) -> String {
    let env_name = provider(provider_id)
        .map(|p| {
            if p.key_env.is_empty() {
                format!("PANTHEON_KEY_{}", p.id.to_uppercase())
            } else {
                p.key_env.clone()
            }
        })
        .unwrap_or_else(|| format!("PANTHEON_KEY_{}", provider_id.to_uppercase()));
    std::env::var(env_name).unwrap_or_else(|_| fallback.to_string())
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
