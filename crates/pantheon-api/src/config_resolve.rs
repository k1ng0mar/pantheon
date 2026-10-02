//! Model-resolution precedence, in one place.
//!
//! Three call sites used to resolve the session's chat model
//! independently, and they disagreed:
//!
//! - `pantheon_runtime::Session::from_env` - environment only, ignoring
//!   `config.toml` entirely;
//! - the TUI's `build_model_policy` - explicit override, then config,
//!   then environment;
//! - the dashboard session factory - `[model]` section, then
//!   environment.
//!
//! The TUI's own doc comment even claimed "environment > config.toml"
//! while its code did the opposite. The rule is now stated once, here,
//! and every surface migrates to it:
//!
//! **explicit argument > environment variable > `config.toml` >
//! hardcoded default.**
//!
//! Environment beats the config file: an operator exporting
//! `PANTHEON_PROVIDER` in a shell or a service unit is making a
//! deliberate, visible choice for that process; the config file is the
//! persistent default. Explicit call arguments (CLI flags, API fields)
//! beat both. Empty and whitespace-only values are ignored at every
//! level - an empty env var must not shadow a configured value.
//!
//! Follow-ups (other leaves own those crates): migrate the TUI's
//! `build_model_policy` and the dashboard session factory's
//! `default_model` to [`resolve_default_model`]. Note the behavior
//! change that migration carries: today both prefer the config file
//! over the environment; under this rule the environment wins.

use crate::config::Config;
use crate::model::{DefaultModel, FallbackChain};

/// Last-resort provider when nothing else names one.
pub const DEFAULT_PROVIDER: &str = "local";
/// Last-resort model when nothing else names one.
pub const DEFAULT_MODEL: &str = "llama3.2";

/// Non-empty environment variable, trimmed. Empty/whitespace values
/// read as unset so they cannot shadow a configured value.
pub fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve one model field under the canonical precedence: explicit
/// argument > environment variable > `config.toml` value > default.
pub fn resolve_field(
    explicit: Option<String>,
    env_name: &str,
    cfg_value: Option<&str>,
    default: &str,
) -> String {
    explicit
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| env_nonempty(env_name))
        .or_else(|| {
            cfg_value
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| default.to_string())
}

/// Resolve the default chat model: explicit provider/model arguments >
/// `PANTHEON_PROVIDER`/`PANTHEON_MODEL` > the `[model]` section >
/// the local default.
pub fn resolve_default_model(
    cfg: Option<&Config>,
    provider: Option<String>,
    model: Option<String>,
) -> DefaultModel {
    let section = cfg.and_then(|c| c.model.as_ref());
    DefaultModel {
        provider: resolve_field(
            provider,
            "PANTHEON_PROVIDER",
            section.map(|s| s.provider.as_str()),
            DEFAULT_PROVIDER,
        ),
        model: resolve_field(
            model,
            "PANTHEON_MODEL",
            section.map(|s| s.model.as_str()),
            DEFAULT_MODEL,
        ),
    }
}

/// The `[model].fallbacks` chain as [`FallbackChain`]. Empty when the
/// section is absent or declares no fallbacks.
pub fn resolve_fallback_chain(cfg: Option<&Config>) -> FallbackChain {
    let fallbacks = cfg
        .and_then(|c| c.model.as_ref())
        .map(|m| {
            m.fallbacks
                .iter()
                .map(|f| DefaultModel {
                    provider: f.provider.clone(),
                    model: f.model.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    FallbackChain { fallbacks }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate process-global env; serialize them.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn cfg_with_model(provider: &str, model: &str) -> Config {
        let mut c = Config::default();
        c.model = Some(crate::config::ModelSection {
            provider: provider.to_string(),
            model: model.to_string(),
            ..Default::default()
        });
        c
    }

    fn clear_env() {
        std::env::remove_var("PANTHEON_PROVIDER");
        std::env::remove_var("PANTHEON_MODEL");
    }

    #[test]
    fn precedence_is_explicit_over_env_over_config_over_default() {
        let _g = ENV_GUARD.lock().expect("env guard");
        clear_env();
        let cfg = cfg_with_model("cfg-provider", "cfg-model");

        // Default only.
        let d = resolve_default_model(None, None, None);
        assert_eq!(d.provider, "local");
        assert_eq!(d.model, "llama3.2");

        // Config beats default.
        let d = resolve_default_model(Some(&cfg), None, None);
        assert_eq!(d.provider, "cfg-provider");
        assert_eq!(d.model, "cfg-model");

        // Env beats config.
        std::env::set_var("PANTHEON_PROVIDER", "env-provider");
        std::env::set_var("PANTHEON_MODEL", "env-model");
        let d = resolve_default_model(Some(&cfg), None, None);
        assert_eq!(d.provider, "env-provider");
        assert_eq!(d.model, "env-model");

        // Explicit beats env.
        let d = resolve_default_model(
            Some(&cfg),
            Some("flag-provider".into()),
            Some("flag-model".into()),
        );
        assert_eq!(d.provider, "flag-provider");
        assert_eq!(d.model, "flag-model");
        clear_env();
    }

    #[test]
    fn empty_values_do_not_shadow() {
        let _g = ENV_GUARD.lock().expect("env guard");
        clear_env();
        let cfg = cfg_with_model("cfg-provider", "cfg-model");

        // An empty env var reads as unset: config still wins.
        std::env::set_var("PANTHEON_PROVIDER", "   ");
        let d = resolve_default_model(Some(&cfg), None, None);
        assert_eq!(d.provider, "cfg-provider");

        // An empty explicit arg reads as unset: env still wins.
        std::env::set_var("PANTHEON_PROVIDER", "env-provider");
        let d = resolve_default_model(Some(&cfg), Some("".into()), None);
        assert_eq!(d.provider, "env-provider");
        clear_env();
    }

    #[test]
    fn fields_resolve_independently() {
        let _g = ENV_GUARD.lock().expect("env guard");
        clear_env();
        let cfg = cfg_with_model("cfg-provider", "cfg-model");
        std::env::set_var("PANTHEON_MODEL", "env-model");
        // Provider falls through to config while model takes env.
        let d = resolve_default_model(Some(&cfg), None, None);
        assert_eq!(d.provider, "cfg-provider");
        assert_eq!(d.model, "env-model");
        clear_env();
    }

    #[test]
    fn fallback_chain_from_config() {
        let _g = ENV_GUARD.lock().expect("env guard");
        let mut cfg = cfg_with_model("p", "m");
        let section = cfg.model.as_mut().expect("model section");
        section.fallbacks = vec![
            crate::config::FallbackEntry {
                provider: "fb1p".into(),
                model: "fb1m".into(),
            },
            crate::config::FallbackEntry {
                provider: "fb2p".into(),
                model: "fb2m".into(),
            },
        ];
        let chain = resolve_fallback_chain(Some(&cfg));
        assert_eq!(chain.fallbacks.len(), 2);
        assert_eq!(chain.fallbacks[0].provider, "fb1p");
        assert_eq!(chain.fallbacks[1].model, "fb2m");

        let empty = resolve_fallback_chain(None);
        assert!(empty.fallbacks.is_empty());
    }
}
