//! Provider + model catalog (§5 + §14): where a provider lives, which wire
//! format it speaks, and what a model can do — context limit, tool support,
//! vision, reasoning, streaming, cost.
//!
//! Loaded from a YAML file at runtime. Default: `catalog.yaml` embedded at
//! compile time via `include_str!`. Override: `PANTHEON_CATALOG=/path/to.yaml`.
//! Static data; the runtime reads it, agents never choose from it.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

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
            if let Ok(text) = std::fs::read_to_string(&p) {
                Some(text)
            } else {
                None
            }
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

/// Iterate providers.
pub fn providers() -> &'static [ProviderMeta] {
    &catalog().providers
}

/// Iterate all model rows across all providers.
pub fn models() -> Vec<&'static ModelMeta> {
    catalog()
        .providers
        .iter()
        .flat_map(|p| p.models.iter())
        .collect()
}

/// Look up a provider by id.
pub fn provider(id: &str) -> Option<&'static ProviderMeta> {
    providers().iter().find(|p| p.id == id)
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
    provider_id.to_string()
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
mod tests {
    use super::*;

    #[test]
    fn providers_resolve_with_wire_modes() {
        assert!(provider("anthropic").is_some());
        assert_eq!(provider("anthropic").unwrap().api_mode, ApiMode::Anthropic);
        assert_eq!(provider("openai").unwrap().api_mode, ApiMode::OpenAi);
        assert!(provider("router").is_some());
        assert!(provider("nope").is_none());
    }

    #[test]
    fn model_capabilities_and_cost() {
        let m = model("openai", "gpt-4o").unwrap();
        assert_eq!(m.context_limit, Some(128_000));
        assert!(m.tools && m.vision && m.streaming && !m.reasoning);
        let cost = m.cost.estimate(1_000_000, 1_000_000).unwrap();
        assert!((cost - 12.50).abs() < 1e-9);

        let c = model("anthropic", "claude-sonnet-4").unwrap();
        assert!(c.reasoning && c.vision && c.tools);
    }

    #[test]
    fn unknown_model_gets_conservative_defaults() {
        let m = model_meta("local", "llama3.2");
        assert_eq!(m.context_limit, None);
        assert!(m.tools && m.streaming && !m.vision && !m.reasoning);
        assert!(m.cost.estimate(10, 10).is_none());
    }

    #[test]
    fn unknown_provider_id_passthrough_is_base_url() {
        assert_eq!(
            base_url_for("https://proxy.example/v1"),
            "https://proxy.example/v1"
        );
    }

    #[test]
    fn prominent_providers_include_curated_labs() {
        let prom: Vec<&str> = providers()
            .iter()
            .filter(|p| p.prominent)
            .map(|p| p.id.as_str())
            .collect();
        // Should include the major labs from the catalog.
        assert!(prom.contains(&"anthropic"), "anthropic prominent: {prom:?}");
        assert!(prom.contains(&"openai"), "openai prominent: {prom:?}");
        assert!(prom.contains(&"google"), "google prominent: {prom:?}");
        assert!(prom.contains(&"deepseek"), "deepseek prominent: {prom:?}");
        assert!(prom.contains(&"groq"), "groq prominent: {prom:?}");
        assert!(prom.contains(&"xai"), "xai prominent: {prom:?}");
    }
}
