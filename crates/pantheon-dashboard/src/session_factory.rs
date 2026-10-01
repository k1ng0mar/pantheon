//! Build a [`Session`](pantheon_runtime::session::Session) for dashboard
//! maintenance endpoints (compress) without depending on the TUI crate.
//!
//! This mirrors `pantheon-tui`'s `resume_after_grant` construction — coder
//! policy, resolved model policy, secrets broker, tool enablement —
//! reusing the shared config document types from `pantheon-api` and the
//! TUI's resolution rules. Only the pieces `compress_now` consults are
//! resolved (default model, the compression and vision auxiliaries and
//! their keys); the
//! other aux slots are inert for compression and their resolution lives
//! in the TUI.

use pantheon_api::capability::Policy;
use pantheon_api::config::Config;
use pantheon_api::config_schema::PolicyPreset;
use pantheon_api::model::{
    AuxiliaryKind, AuxiliaryModel, DefaultModel, FallbackChain, ModelPolicy, ReasoningLevel,
};
use pantheon_runtime::session::Session;
use pantheon_secrets::vault::SecretVault;
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

/// Resolve the default chat model: `[model]` section wins, then
/// `PANTHEON_PROVIDER`/`PANTHEON_MODEL`, then the local default —
/// the TUI's `build_model_policy` order.
fn default_model(cfg: Option<&Config>) -> DefaultModel {
    let section = cfg.and_then(|c| c.model.as_ref());
    let cfg_str = |f: fn(&pantheon_api::config::ModelSection) -> &str| {
        section
            .map(f)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    DefaultModel {
        provider: cfg_str(|m| &m.provider)
            .or_else(|| env_nonempty("PANTHEON_PROVIDER"))
            .unwrap_or_else(|| "local".into()),
        model: cfg_str(|m| &m.model)
            .or_else(|| env_nonempty("PANTHEON_MODEL"))
            .unwrap_or_else(|| "llama3.2".into()),
    }
}

/// Resolve the compression auxiliary: `PANTHEON_COMPRESSION_PROVIDER` /
/// `PANTHEON_COMPRESSION_MODEL` win field-wise, then the `[compression]`
/// section; `"default"`/absent inherits the default model — the TUI's
/// `resolve_aux_target` rule.
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

/// Resolve the vision auxiliary: `PANTHEON_VISION_PROVIDER` /
/// `PANTHEON_VISION_MODEL` win field-wise, then the `[vision]` section;
/// `"default"`/absent inherits the default model — the TUI's
/// `resolve_aux_target` rule. Returns `None` when nothing pins a
/// vision model: the runtime treats that as `auto` (attached images
/// ride the user row to the default model directly).
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
    let mut chain = FallbackChain::default();
    if let Some(fallbacks) = section.map(|m| &m.fallbacks) {
        for f in fallbacks {
            chain.fallbacks.push(DefaultModel {
                provider: f.provider.clone(),
                model: f.model.clone(),
            });
        }
    }
    // `PANTHEON_REASONING_BUDGET` / `PANTHEON_REASONING` win, then the
    // `[model]` fields, then off — the TUI's resolution.
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

/// Seed `PANTHEON_COMPRESSION_API_KEY` in a memory vault from the
/// compression slot's key env: explicit `api_key_env` wins; a
/// default-inheriting provider inherits `[model].api_key_env`; otherwise
/// the runtime falls back to `PANTHEON_KEY_<PROVIDER>`, which the system
/// env broker below already loads. Mirrors the TUI's `seed_env_key` /
/// `resolve_aux_key_env` for this slot.
fn seed_compression_key(
    broker: pantheon_secrets::SecretsBroker,
    cfg: Option<&Config>,
) -> pantheon_secrets::SecretsBroker {
    let section = cfg.and_then(|c| c.compression.as_ref());
    let explicit = section
        .and_then(|s| s.aux.api_key_env.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let inherits_default = section
        .map(|s| s.aux.provider.trim())
        .map(|p| p.is_empty() || p.eq_ignore_ascii_case("default"))
        .unwrap_or(true);
    let key_env = explicit.or_else(|| {
        if inherits_default {
            cfg.and_then(|c| c.model.as_ref())
                .and_then(|m| m.api_key_env.clone())
        } else {
            None
        }
    });
    let (vault_name, key_env) = ("PANTHEON_COMPRESSION_API_KEY", key_env);
    let Some(env) = key_env else {
        return broker;
    };
    let Ok(value) = std::env::var(&env) else {
        return broker;
    };
    let mem = pantheon_secrets::MemoryVault::new();
    let _ = mem.set(vault_name, pantheon_secrets::SecretValue::new(value));
    broker.with_vault(Box::new(mem))
}

/// Resolve `[budget]` to the run [`Budget`]: max turns, tool calls,
/// delegate depth, token cap. Absent = the runtime defaults. Mirrors
/// `pantheon-tui`'s `config_budget`/`resolve_budget_section` (0 counts as
/// unset); this crate must not depend on the TUI, so the mapping lives
/// here and the two must be kept in sync.
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
        },
    }
}

/// The `[budget].max_tokens` config tier, kept apart from the live
/// budget — the TUI's `set_budget_max_tokens` block: `/tokens`
/// overwrites `budget.max_tokens` for the session, and `/tokens off`
/// must fall back to this configured value rather than forget it.
/// (0 = unset, same as `budget_for`.)
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
    seed_compression_key(broker, cfg)
}

/// Open a session for a maintenance endpoint, mirroring
/// `pantheon-tui`'s `resume_after_grant`. `data_dir` is the Pantheon data
/// directory (the dashboard's `App::data_dir`).
pub fn open_maintenance_session(data_dir: &Path) -> Result<Session, String> {
    let cfg = load_config(data_dir)?;
    let cfg_ref = cfg.as_ref();
    let policy = match cfg_ref.and_then(|c| c.policy) {
        Some(PolicyPreset::CoderMemory) => Policy::coder_with_memory(),
        _ => Policy::coder(),
    };
    let model_policy = model_policy(cfg_ref);
    let secrets = secrets_broker(cfg_ref);
    let session = Session::new(data_dir.to_path_buf(), policy, model_policy, secrets)
        .map_err(|e| format!("open session: {e}"))?;
    let enablement = cfg_ref
        .and_then(|c| c.tools.as_ref())
        .map(pantheon_runtime::tool_config::ToolEnablement::from_section)
        .unwrap_or_default();
    session.set_tool_enablement(enablement);
    // Run budgets from `[budget]` in config.toml (max turns, tool calls,
    // delegate depth, token cap): the TUI threads these at startup
    // (`config_budget`) and dashboard sessions must behave the same —
    // without this the `[budget].max_tokens` tier silently fell through
    // to the model max / 16k fallback.
    session.set_budget(budget_for(cfg_ref));
    // The configured token cap, kept apart from the live budget exactly
    // like the TUI's `set_budget_max_tokens` block, so `/tokens off`
    // falls back to the configured value instead of forgetting it.
    session.set_budget_max_tokens(configured_max_tokens(cfg_ref));
    Ok(session)
}

#[cfg(test)]
mod budget_tiering_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Crate convention (env.rs, lib.rs tests): std-only temp dirs, no
    /// tempfile dev-dependency.
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

    /// Fail-before: a dashboard-created session must carry the
    /// `[budget].max_tokens` config tier exactly like the TUI's
    /// interactive sessions (session.rs `set_budget` +
    /// `set_budget_max_tokens`).
    #[test]
    fn budget_config_tier_reaches_dashboard_session() {
        let dir = scratch_dir("tier");
        std::fs::write(
            dir.join("config.toml"),
            "[budget]\nmax_tokens = 5000\nmax_turns = 20\n",
        )
        .expect("write config");
        let session = open_maintenance_session(&dir).expect("open maintenance session");
        // The live run budget carries the resolved tier ...
        assert_eq!(
            session.budget_snapshot().max_tokens,
            Some(5000),
            "budget.max_tokens must mirror [budget].max_tokens"
        );
        assert_eq!(session.budget_snapshot().max_turns, 20);
        // ... and the kept-apart configured-cap slot must hold it too, so
        // `/tokens off` semantics (fall back to the config tier) match
        // the TUI.
        let configured = session
            .budget_max_tokens
            .lock()
            .expect("budget lock")
            .clone();
        assert_eq!(configured, Some(5000));
        // End to end through the real precedence function: config tier
        // wins over the model max / 16k fallback when the session has no
        // `/tokens` override.
        assert_eq!(
            pantheon_api::config::resolve_max_output_tokens(configured, configured, Some(8192)),
            5000
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Absent `[budget]` = runtime defaults: no caps anywhere.
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
