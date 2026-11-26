//! Build a [`Session`](pantheon_runtime::session::Session) for the dashboard
//! maintenance endpoints (compress) without depending on the TUI crate.
//!
//! Mirrors `pantheon-tui`'s `resume_after_grant`: coder policy, resolved
//! model policy, secrets broker, tool enablement, reused shared config types
//! from `pantheon-api` and the TUI's resolution rules. Only what `compress_now`
//! reads gets resolved (default model, compression/vision auxiliaries + keys);
//! the other aux slots are inert for compression and handled by the TUI.

use pantheon_api::capability::Policy;
use pantheon_api::config::Config;
use pantheon_api::config_schema::PolicyPreset;
use pantheon_api::model::{
    AuxiliaryKind, AuxiliaryModel, DefaultModel, ModelPolicy, ReasoningLevel,
};
use pantheon_runtime::session::Session;
use std::path::Path;

pub(crate) fn load_config(data_dir: &Path) -> Result<Option<Config>, String> {
    let path = data_dir.join("config.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        // No config file = defaults, same as a fresh install.
        Err(_) => return Ok(None),
    };
    toml::from_str::<Config>(&raw)
        .map(Some)
        .map_err(|e| format!("{} does not parse: {e}", path.display()))
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// default chat model under the canonical precedence (explicit > `[model]` >
/// local default, empty values ignored), per `resolve_default_model`. Config is
/// the single source - env is NOT consulted, so a stale `PANTHEON_PROVIDER`/
/// `PANTHEON_MODEL` in a shell can't shadow it.
fn default_model(cfg: Option<&Config>) -> DefaultModel {
    pantheon_api::config_resolve::resolve_default_model(cfg, None, None)
}

/// compression auxiliary: `PANTHEON_COMPRESSION_PROVIDER`/`..._MODEL` win
/// field-wise, then `[compression]`; `"default"`/absent inherits the default
/// model - the TUI's `resolve_aux_target` rule.
fn compression_aux(cfg: Option<&Config>, default: &DefaultModel) -> AuxiliaryModel {
    let section = cfg.and_then(|c| c.compression.as_ref());
    let cfg_str = |f: fn(&pantheon_api::config::AuxSection) -> &str| {
        section
            .map(|s| &s.aux)
            .map(f)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let raw_provider =
        env_nonempty("PANTHEON_COMPRESSION_PROVIDER").or_else(|| cfg_str(|s| &s.provider));
    let provider = match raw_provider.as_deref() {
        None => default.provider.clone(),
        Some(p) if p.eq_ignore_ascii_case("default") => default.provider.clone(),
        Some(p) => p.to_string(),
    };
    let model = env_nonempty("PANTHEON_COMPRESSION_MODEL")
        .or_else(|| cfg_str(|s| &s.model))
        .unwrap_or_else(|| default.model.clone());
    let timeout_secs = section
        .and_then(|s| s.aux.timeout)
        .filter(|&t| t > 0)
        .unwrap_or_else(|| AuxiliaryKind::Compression.default_timeout_secs());
    AuxiliaryModel {
        kind: AuxiliaryKind::Compression,
        provider,
        model,
        timeout_secs,
        target_percent: section.and_then(|s| s.target_percent),
    }
}

/// vision auxiliary: `PANTHEON_VISION_PROVIDER`/`..._MODEL` win field-wise,
/// then `[vision]`; `"default"`/absent inherits the default model - the TUI's
/// `resolve_aux_target` rule. `None` when nothing pins a vision model: the
/// runtime treats that as `auto` (attached images ride the user row to the
/// default model).
fn vision_aux(cfg: Option<&Config>, default: &DefaultModel) -> Option<AuxiliaryModel> {
    let section = cfg.and_then(|c| c.vision.as_ref());
    let cfg_str = |f: fn(&pantheon_api::config::AuxSection) -> &str| {
        section
            .map(f)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let raw_provider =
        env_nonempty("PANTHEON_VISION_PROVIDER").or_else(|| cfg_str(|s| &s.provider));
    let raw_model = env_nonempty("PANTHEON_VISION_MODEL").or_else(|| cfg_str(|s| &s.model));
    if raw_provider.is_none() && raw_model.is_none() {
        return None; // unconfigured: auto
    }
    let provider = match raw_provider.as_deref() {
        None => default.provider.clone(),
        Some(p) if p.eq_ignore_ascii_case("default") => default.provider.clone(),
        Some(p) => p.to_string(),
    };
    let model = raw_model.unwrap_or_else(|| default.model.clone());
    let timeout_secs = section
        .and_then(|s| s.timeout)
        .filter(|&t| t > 0)
        .unwrap_or(AuxiliaryKind::Vision.default_timeout_secs());
    Some(AuxiliaryModel {
        kind: AuxiliaryKind::Vision,
        provider,
        model,
        timeout_secs,
        target_percent: None,
    })
}

fn video_aux(cfg: Option<&Config>, default: &DefaultModel) -> Option<AuxiliaryModel> {
    let section = cfg.and_then(|c| c.video.as_ref());
    let cfg_str = |f: fn(&pantheon_api::config::AuxSection) -> &str| {
        section
            .map(f)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let raw_provider = env_nonempty("PANTHEON_VIDEO_PROVIDER").or_else(|| cfg_str(|s| &s.provider));
    let raw_model = env_nonempty("PANTHEON_VIDEO_MODEL").or_else(|| cfg_str(|s| &s.model));
    if raw_provider.is_none() && raw_model.is_none() {
        return None; // unconfigured: auto
    }
    let provider = match raw_provider.as_deref() {
        None => default.provider.clone(),
        Some(p) if p.eq_ignore_ascii_case("default") => default.provider.clone(),
        Some(p) => p.to_string(),
    };
    let model = raw_model.unwrap_or_else(|| default.model.clone());
    let timeout_secs = section
        .and_then(|s| s.timeout)
        .filter(|&t| t > 0)
        .unwrap_or(AuxiliaryKind::Video.default_timeout_secs());
    Some(AuxiliaryModel {
        kind: AuxiliaryKind::Video,
        provider,
        model,
        timeout_secs,
        target_percent: None,
    })
}

fn model_policy(cfg: Option<&Config>) -> ModelPolicy {
    let section = cfg.and_then(|c| c.model.as_ref());
    let default = default_model(cfg);
    let chain = pantheon_api::config_resolve::resolve_fallback_chain(cfg);
    // `PANTHEON_REASONING_BUDGET` / `PANTHEON_REASONING` win, then `[model]`
    // fields, then off - the TUI's resolution.
    let reasoning_budget = env_nonempty("PANTHEON_REASONING_BUDGET")
        .and_then(|v| v.parse::<u32>().ok())
        .or_else(|| section.and_then(|m| m.reasoning_budget));
    let reasoning = env_nonempty("PANTHEON_REASONING")
        .as_deref()
        .and_then(ReasoningLevel::parse)
        .or_else(|| {
            section
                .and_then(|m| m.reasoning.as_deref())
                .and_then(ReasoningLevel::parse)
        })
        .unwrap_or_default();
    ModelPolicy {
        auxiliaries: vision_aux(cfg, &default)
            .into_iter()
            .chain(video_aux(cfg, &default))
            .chain(std::iter::once(compression_aux(cfg, &default)))
            .collect(),
        default,
        fallbacks: chain,
        reasoning,
        reasoning_budget,
    }
}

/// `[budget]` -> run [`Budget`]: max turns, tool calls, delegate depth, token
/// cap. Absent = runtime defaults. Mirrors `pantheon-tui`'s
/// `config_budget`/`resolve_budget_section` (0 counts as unset); this crate
/// can't depend on the TUI, so the mapping lives here and they must stay in
/// sync.
fn budget_for(cfg: Option<&Config>) -> pantheon_agent::Budget {
    let nz = |v: Option<u32>, d: u32| v.filter(|&v| v > 0).unwrap_or(d);
    match cfg.and_then(|c| c.budget.as_ref()) {
        None => pantheon_agent::Budget::default(),
        Some(b) => pantheon_agent::Budget {
            max_turns: nz(b.max_turns, 16),
            max_tool_calls: nz(b.max_tool_calls, 32),
            max_tokens: b.max_tokens.filter(|&v| v > 0),
            max_delegate_depth: nz(b.max_delegate_depth, 2),
            allow_child_spawn: true,
            max_consecutive_tool_failures: b.max_consecutive_tool_failures.unwrap_or(5),
            warn_consecutive_tool_failures: 3,
            max_cost_cents: None,
        },
    }
}

/// configured `[budget].max_tokens` tier, kept apart from the live budget -
/// the TUI's `set_budget_max_tokens` block. `/tokens` overwrites
/// `budget.max_tokens` for the session; `/tokens off` has to fall back to
/// this config value, not forget it. (0 = unset, as in `budget_for`.)
fn configured_max_tokens(cfg: Option<&Config>) -> Option<u32> {
    cfg.and_then(|c| c.budget.as_ref())
        .and_then(|b| b.max_tokens)
        .filter(|&v| v > 0)
}

pub(crate) fn secrets_broker(cfg: Option<&Config>) -> pantheon_secrets::SecretsBroker {
    let model_key_env = cfg
        .and_then(|c| c.model.as_ref())
        .and_then(|m| m.api_key_env.clone());
    let broker =
        pantheon_secrets::SecretsBroker::from_system_env_with_api_key(model_key_env.as_deref());
    broker
}

/// maintenance-session policy, mirroring the TUI's `policy_for_config`:
/// config preset, then `PANTHEON_POLICY`, then `PANTHEON_ALLOW_MEMORY`, then
/// coder. The dashboard can't depend on the TUI crate, so this lives here.
fn policy_for_config(cfg: Option<&Config>) -> Policy {
    if let Some(preset) = cfg.and_then(|c| c.policy) {
        return preset.to_policy();
    }
    if let Ok(p) = std::env::var("PANTHEON_POLICY") {
        if let Some(preset) = PolicyPreset::parse(&p) {
            return preset.to_policy();
        }
    }
    let allow_memory = std::env::var("PANTHEON_ALLOW_MEMORY")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    if allow_memory {
        Policy::coder_with_memory()
    } else {
        Policy::coder()
    }
}

/// open a session for a maintenance endpoint, mirroring `pantheon-tui`'s
/// `resume_after_grant`. `data_dir` is the Pantheon data directory (the
/// dashboard's `App::data_dir`).
pub fn open_maintenance_session(data_dir: &Path) -> Result<Session, String> {
    let cfg = load_config(data_dir)?;
    let cfg_ref = cfg.as_ref();
    // policy preset honored verbatim, same precedence as the TUI's
    // `policy_for_config`. A Reader config must stay read-only here, not
    // silently widen to coder.
    let policy = policy_for_config(cfg_ref);
    let model_policy = model_policy(cfg_ref);
    let secrets = secrets_broker(cfg_ref);
    // `[memory]` wins over memory-backend.toml on load: sync BEFORE the
    // session reads the selection, or the sync lands a session late.
    if let Some(section) = cfg_ref.and_then(|c| c.memory.as_ref()) {
        if let Some(warning) = pantheon_memory::sync_selection_from_config(
            data_dir,
            &section.backend,
            &section.options,
        ) {
            eprintln!("{warning}");
        }
    }
    let session = Session::new(data_dir.to_path_buf(), policy, model_policy, secrets)
        .map_err(|e| format!("open session: {e}"))?;
    let enablement = cfg_ref
        .and_then(|c| c.tools.as_ref())
        .map(pantheon_runtime::tool_config::ToolEnablement::from_section)
        .unwrap_or_default();
    session.set_tool_enablement(enablement);
    // run budgets from `[budget]` in config.toml, like the TUI threads them
    // at startup (`config_budget`). without this the `[budget].max_tokens`
    // tier silently fell through to the model max / 16k fallback.
    session.set_budget(budget_for(cfg_ref));
    // the live `[budget]` section too, so `delegate` turns spawned from
    // maintenance sessions resolve the delegation knobs exactly as configured
    session.set_budget_section(cfg_ref.and_then(|c| c.budget.clone()).unwrap_or_default());
    // configured token cap kept apart from the live budget, so `/tokens off`
    // falls back to it instead of forgetting it
    session.set_budget_max_tokens(configured_max_tokens(cfg_ref));
    Ok(session)
}

#[cfg(test)]
mod budget_tiering_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// crate convention (env.rs, lib.rs tests): std temp dirs, no tempfile
    /// dev-dependency
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-budget-test-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// fail-before: a dashboard-created session must carry the
    /// `[budget].max_tokens` config tier exactly like the TUI's interactive
    /// sessions (session.rs `set_budget` + `set_budget_max_tokens`).
    #[test]
    fn budget_config_tier_reaches_dashboard_session() {
        let dir = scratch_dir("tier");
        std::fs::write(
            dir.join("config.toml"),
            "[budget]\nmax_tokens = 5000\nmax_turns = 20\n",
        )
        .expect("write config");
        let session = open_maintenance_session(&dir).expect("open maintenance session");
        // the live run budget carries the resolved tier ...
        assert_eq!(
            session.budget_snapshot().max_tokens,
            Some(5000),
            "budget.max_tokens must mirror [budget].max_tokens"
        );
        assert_eq!(session.budget_snapshot().max_turns, 20);
        // ... and the kept-apart configured-cap slot must hold it too, so
        // `/tokens off` semantics (fall back to the config tier) match the TUI
        let configured = *session.budget_max_tokens.lock().expect("budget lock");
        assert_eq!(configured, Some(5000));
        // end to end through the real precedence function: config tier
        // wins over the model max / 16k fallback when there's no `/tokens`
        // override
        assert_eq!(
            pantheon_api::config::resolve_max_output_tokens(configured, configured, Some(8192)),
            5000
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// absent `[budget]` = runtime defaults, no caps anywhere
    #[test]
    fn no_budget_section_means_defaults() {
        let dir = scratch_dir("defaults");
        let session = open_maintenance_session(&dir).expect("open maintenance session");
        assert_eq!(session.budget_snapshot().max_tokens, None);
        assert_eq!(session.budget_snapshot().max_turns, 16);
        assert_eq!(
            session
                .budget_max_tokens
                .lock()
                .expect("budget lock")
                .clone(),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod default_model_tests {
    use super::*;

    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn cfg_with_model(provider: &str, model: &str) -> Config {
        Config {
            model: Some(pantheon_api::config::ModelSection {
                provider: provider.to_string(),
                model: model.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn canonical_precedence_config_beats_env() {
        let _g = ENV_GUARD.lock().expect("env guard");
        std::env::remove_var("PANTHEON_PROVIDER");
        std::env::remove_var("PANTHEON_MODEL");
        let cfg = cfg_with_model("cfg-provider", "cfg-model");
        assert_eq!(default_model(Some(&cfg)).provider, "cfg-provider");
        // env must not shadow config - config is the single source of truth
        std::env::set_var("PANTHEON_PROVIDER", "env-provider");
        assert_eq!(default_model(Some(&cfg)).provider, "cfg-provider");
        std::env::set_var("PANTHEON_PROVIDER", "  ");
        assert_eq!(default_model(Some(&cfg)).provider, "cfg-provider");
        std::env::remove_var("PANTHEON_PROVIDER");
        std::env::remove_var("PANTHEON_MODEL");
    }
}
