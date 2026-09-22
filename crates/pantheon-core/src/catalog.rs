//! Provider + model catalog (§5 + §14): where a provider lives, which wire
//! format it speaks, and what a model can do — context limit, tool support,
//! vision, reasoning, streaming, cost. Static data; the runtime reads it,
//! agents never choose from it.

use serde::{Deserialize, Serialize};

/// Wire format a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiMode {
    /// OpenAI chat-completions shape (`/chat/completions`).
    OpenAi,
    /// Anthropic Messages shape (`/messages`).
    Anthropic,
}

/// List prices in USD per million tokens. `None` = unknown / free / local.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelCost {
    pub input_per_mtok_usd: Option<f64>,
    pub output_per_mtok_usd: Option<f64>,
}

impl ModelCost {
    pub fn estimate(&self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        let i = self.input_per_mtok_usd?;
        let o = self.output_per_mtok_usd?;
        Some(input_tokens as f64 * i / 1_000_000.0 + output_tokens as f64 * o / 1_000_000.0)
    }
}

/// Static metadata for one provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderMeta {
    pub id: &'static str,
    pub label: &'static str,
    /// Default base URL (no trailing slash, endpoint path is appended by
    /// the adapter: `/chat/completions` or `/messages`).
    pub base_url: &'static str,
    pub api_mode: ApiMode,
    /// Env var that overrides the base URL, e.g. `PANTHEON_BASE_OPENAI`.
    pub base_env: &'static str,
    /// Env var that overrides the API key, e.g. `PANTHEON_KEY_OPENAI`.
    pub key_env: &'static str,
}

/// Static metadata for one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelMeta {
    pub provider: String,
    pub model: String,
    /// Context window in tokens; `None` = unknown.
    pub context_limit: Option<u32>,
    /// Provider-imposed max output tokens; `None` = unknown/default.
    pub max_output_tokens: Option<u32>,
    pub tools: bool,
    pub vision: bool,
    pub reasoning: bool,
    pub streaming: bool,
    pub cost: ModelCost,
}

pub const PROVIDERS: &[ProviderMeta] = &[
    ProviderMeta {
        id: "openai",
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        api_mode: ApiMode::OpenAi,
        base_env: "PANTHEON_BASE_OPENAI",
        key_env: "PANTHEON_KEY_OPENAI",
    },
    ProviderMeta {
        id: "anthropic",
        label: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
        api_mode: ApiMode::Anthropic,
        base_env: "PANTHEON_BASE_ANTHROPIC",
        key_env: "PANTHEON_KEY_ANTHROPIC",
    },
    ProviderMeta {
        id: "deepseek",
        label: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        api_mode: ApiMode::OpenAi,
        base_env: "PANTHEON_BASE_DEEPSEEK",
        key_env: "PANTHEON_KEY_DEEPSEEK",
    },
    ProviderMeta {
        id: "openrouter",
        label: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        api_mode: ApiMode::OpenAi,
        base_env: "PANTHEON_BASE_OPENROUTER",
        key_env: "PANTHEON_KEY_OPENROUTER",
    },
    ProviderMeta {
        id: "groq",
        label: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        api_mode: ApiMode::OpenAi,
        base_env: "PANTHEON_BASE_GROQ",
        key_env: "PANTHEON_KEY_GROQ",
    },
    ProviderMeta {
        id: "local",
        label: "Local (Ollama)",
        base_url: "http://127.0.0.1:11434/v1",
        api_mode: ApiMode::OpenAi,
        base_env: "PANTHEON_BASE_LOCAL",
        key_env: "PANTHEON_KEY_LOCAL",
    },
    ProviderMeta {
        id: "router",
        label: "Local llm-router",
        base_url: "http://127.0.0.1:8015/v1",
        api_mode: ApiMode::OpenAi,
        base_env: "PANTHEON_BASE_ROUTER",
        key_env: "PANTHEON_KEY_ROUTER",
    },
];

fn table() -> &'static [ModelMeta] {
    static TABLE: std::sync::OnceLock<Vec<ModelMeta>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        vec![
            ModelMeta {
                provider: "openai".into(),
                model: "gpt-4o".into(),
                context_limit: Some(128_000),
                max_output_tokens: Some(16_384),
                tools: true,
                vision: true,
                reasoning: false,
                streaming: true,
                cost: ModelCost {
                    input_per_mtok_usd: Some(2.50),
                    output_per_mtok_usd: Some(10.00),
                },
            },
            ModelMeta {
                provider: "openai".into(),
                model: "gpt-4o-mini".into(),
                context_limit: Some(128_000),
                max_output_tokens: Some(16_384),
                tools: true,
                vision: true,
                reasoning: false,
                streaming: true,
                cost: ModelCost {
                    input_per_mtok_usd: Some(0.15),
                    output_per_mtok_usd: Some(0.60),
                },
            },
            ModelMeta {
                provider: "anthropic".into(),
                model: "claude-opus-4".into(),
                context_limit: Some(200_000),
                max_output_tokens: Some(32_000),
                tools: true,
                vision: true,
                reasoning: true,
                streaming: true,
                cost: ModelCost {
                    input_per_mtok_usd: Some(15.00),
                    output_per_mtok_usd: Some(75.00),
                },
            },
            ModelMeta {
                provider: "anthropic".into(),
                model: "claude-sonnet-4".into(),
                context_limit: Some(200_000),
                max_output_tokens: Some(64_000),
                tools: true,
                vision: true,
                reasoning: true,
                streaming: true,
                cost: ModelCost {
                    input_per_mtok_usd: Some(3.00),
                    output_per_mtok_usd: Some(15.00),
                },
            },
            ModelMeta {
                provider: "deepseek".into(),
                model: "deepseek-chat".into(),
                context_limit: Some(128_000),
                max_output_tokens: Some(8_192),
                tools: true,
                vision: false,
                reasoning: true,
                streaming: true,
                cost: ModelCost {
                    input_per_mtok_usd: Some(0.27),
                    output_per_mtok_usd: Some(1.10),
                },
            },
            // Local llm-router pools (context limits from its router.yaml).
            ModelMeta {
                provider: "router".into(),
                model: "chat".into(),
                context_limit: Some(256_000),
                max_output_tokens: None,
                tools: true,
                vision: false,
                reasoning: true,
                streaming: true,
                cost: ModelCost::default(),
            },
            ModelMeta {
                provider: "router".into(),
                model: "code".into(),
                context_limit: Some(262_144),
                max_output_tokens: None,
                tools: true,
                vision: false,
                reasoning: true,
                streaming: true,
                cost: ModelCost::default(),
            },
            ModelMeta {
                provider: "router".into(),
                model: "media".into(),
                context_limit: Some(128_000),
                max_output_tokens: None,
                tools: false,
                vision: true,
                reasoning: false,
                streaming: true,
                cost: ModelCost::default(),
            },
        ]
    })
}

/// Look up a provider by id.
pub fn provider(id: &str) -> Option<&'static ProviderMeta> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Look up a model's static metadata (exact provider + model match).
pub fn model(provider_id: &str, model_id: &str) -> Option<&'static ModelMeta> {
    table()
        .iter()
        .find(|m| m.provider == provider_id && m.model == model_id)
}

/// Model metadata with conservative defaults for unknown models:
/// context unknown, tools on, vision off, reasoning off, streaming on,
/// cost unknown.
pub fn model_meta(provider_id: &str, model_id: &str) -> ModelMeta {
    model(provider_id, model_id)
        .cloned()
        .unwrap_or_else(|| ModelMeta {
            provider: provider_id.to_string(),
            model: model_id.to_string(),
            context_limit: None,
            max_output_tokens: None,
            tools: true,
            vision: false,
            reasoning: false,
            streaming: true,
            cost: ModelCost::default(),
        })
}

/// Base URL for a provider: env override → catalog → the provider id
/// itself (treated as a full base URL, legacy passthrough).
pub fn base_url_for(provider_id: &str) -> String {
    if let Some(p) = provider(provider_id) {
        if let Ok(u) = std::env::var(p.base_env) {
            if !u.is_empty() {
                return u;
            }
        }
        return p.base_url.to_string();
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
        .map(|p| p.key_env.to_string())
        .unwrap_or_else(|| format!("PANTHEON_KEY_{}", provider_id.to_uppercase()));
    std::env::var(env_name).unwrap_or_else(|_| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_resolve_with_wire_modes() {
        assert_eq!(provider("anthropic").unwrap().api_mode, ApiMode::Anthropic);
        assert_eq!(provider("openai").unwrap().api_mode, ApiMode::OpenAi);
        assert_eq!(
            provider("router").unwrap().base_url,
            "http://127.0.0.1:8015/v1"
        );
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
        // No env override expected for this exotic id.
        assert_eq!(
            base_url_for("https://proxy.example/v1"),
            "https://proxy.example/v1"
        );
    }
}
