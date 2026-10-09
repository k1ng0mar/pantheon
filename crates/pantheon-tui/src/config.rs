//! Client-side config resolution for the TUI.
//!
//! The `config.toml` **document** (all section structs, `Config`,
//! load/save/validate) lives in [`pantheon_api::config`] - the shared,
//! client-agnostic home. This module re-exports it so existing
//! `crate::config::...` paths keep working, and adds what only the TUI
//! (as a client) needs:
//!
//! * resolution of document sections into runtime/agent types
//!   ([`resolve_budget_section`], [`resolve_browser_section`],
//!   [`resolve_websearch_section`], [`resolve_mcp_section`],
//!   [`config_budget`]) - inherent impls can't live here because the
//!   structs are foreign now;
//! * auxiliary-model wiring, secrets-broker construction, and the other
//!   composition helpers below.
//!
//! [`pantheon_api::config`]: https://docs.rs/pantheon-api

pub use pantheon_api::config::*;

use pantheon_secrets::SecretVault;

// --- Resolution of document sections into runtime/agent types ---------------
// These were inherent `resolve()` methods before the document moved to
// `pantheon-api`. The structs are foreign now, so they are free functions.

fn nz(value: Option<u32>, default: u32) -> u32 {
    value.filter(|&v| v > 0).unwrap_or(default)
}

/// Resolve `[mcp.servers.<name>] headers` values. A value starting with
/// `env:` reads the variable from the operator's environment at load
/// time (missing variable = the header is dropped: the server's own
/// 401 names the problem, not a Pantheon panic). Literal values pass
/// through. Header NAMES may appear in logs; values never do.
fn resolve_headers(
    headers: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for (k, v) in headers {
        let resolved = match v.strip_prefix("env:") {
            Some(name) => std::env::var(name).unwrap_or_default(),
            None => v.clone(),
        };
        if !resolved.is_empty() {
            out.insert(k.clone(), resolved);
        }
    }
    out
}

/// The `[tools]` enablement for a loaded config: absent section = every
/// group enabled, the pre-Tools-screen default.
pub fn tool_enablement(cfg: Option<&Config>) -> pantheon_runtime::tool_config::ToolEnablement {
    cfg.and_then(|c| c.tools.as_ref())
        .map(pantheon_runtime::tool_config::ToolEnablement::from_section)
        .unwrap_or_default()
}

/// Apply the `[tools]` section of a loaded config to a fresh session.
///
/// Call this on every `Session::new` before the first turn: the enablement
/// is snapshotted at registry build time, so a session built without it
/// would ignore the user's Tools-screen choices.
pub fn apply_tool_enablement(session: &pantheon_runtime::session::Session, cfg: Option<&Config>) {
    session.set_tool_enablement(tool_enablement(cfg));
}

/// Whether a tool group is enabled in a loaded config. Used for
/// out-of-registry gates: the `/voice` status command and the
/// vision/video auxiliary entries.
pub fn tool_group_enabled(cfg: Option<&Config>, group: pantheon_api::config::ToolGroup) -> bool {
    tool_enablement(cfg).is_enabled(group)
}

/// Thread the `[budget]` tiers into a freshly built session: the live
/// run budget (`set_budget`, from [`config_budget`]), the kept-apart
/// configured token cap (`set_budget_max_tokens`), so `/tokens off`
/// falls back to the config value instead of forgetting it, and the
/// live `[budget]` section itself (`set_budget_section`), so `delegate`
/// turns resolve delegation knobs exactly as configured. Call on every
/// `Session::new` that serves turns - the interactive TUI startup, the
/// /agui, gateway, and scheduled-run paths use this helper so they
/// cannot drift apart.
pub fn apply_budget_tiers(session: &pantheon_runtime::session::Session, cfg: Option<&Config>) {
    session.set_budget(cfg.map(config_budget).unwrap_or_default());
    session.set_budget_max_tokens(
        cfg.and_then(|c| c.budget.as_ref())
            .and_then(|b| b.max_tokens)
            .filter(|&v| v > 0),
    );
    session.set_budget_section(cfg.and_then(|c| c.budget.clone()).unwrap_or_default());
}

/// Resolve `[budget]` to the runtime [`pantheon_agent::Budget`].
/// Absent section = the runtime defaults.
pub fn resolve_budget_section(s: &BudgetSection) -> pantheon_agent::Budget {
    pantheon_agent::Budget {
        max_turns: nz(s.max_turns, 16),
        max_tool_calls: nz(s.max_tool_calls, 32),
        max_tokens: s.max_tokens.filter(|&v| v > 0),
        max_delegate_depth: nz(s.max_delegate_depth, 2),
        allow_child_spawn: true,
        // Not `nz`: `0` is a meaningful value here (explicitly disable the
        // cap), so only an absent key falls back to the default.
        max_consecutive_tool_failures: s.max_consecutive_tool_failures.unwrap_or(5),
    }
}

/// Effective run budget for a config. Absent `[budget]` = the runtime
/// defaults (16 turns, 32 tool calls, depth 2, uncapped tokens).
pub fn config_budget(cfg: &Config) -> pantheon_agent::Budget {
    cfg.budget
        .as_ref()
        .map(resolve_budget_section)
        .unwrap_or_default()
}

/// Resolve `[browser]` - now in [`pantheon_runtime::resolve_browser_section`]
/// so the dashboard's browser stream endpoints resolve the section
/// identically. Kept here as a thin delegate for existing callers.
pub fn resolve_browser_section(s: &BrowserSection) -> pantheon_runtime::BrowserToolConfig {
    pantheon_runtime::resolve_browser_section(s)
}

/// Resolve `[websearch]` to the runtime
/// [`pantheon_runtime::WebsearchToolConfig`]. Env vars
/// (`PANTHEON_WEBSEARCH_ENABLED`) win over the file, matching the
/// runtime defaults.
pub fn resolve_websearch_section(s: &WebsearchSection) -> pantheon_runtime::WebsearchToolConfig {
    let mut cfg = pantheon_runtime::WebsearchToolConfig::default();
    cfg.enabled = std::env::var("PANTHEON_WEBSEARCH_ENABLED")
        .map(|v| v != "0")
        .unwrap_or_else(|_| s.enabled.unwrap_or(cfg.enabled));
    if let Some(provider) = s.provider.clone() {
        if !provider.trim().is_empty() {
            cfg.provider = provider;
        }
    }
    if let Some(name) = s.api_key_secret.clone() {
        if !name.trim().is_empty() {
            cfg.api_key_secret = Some(name);
        }
    }
    if let Some(n) = s.max_results.filter(|&n| n > 0) {
        cfg.max_results = n;
    }
    // Endpoint override for self-hosted providers. The section wins; for
    // SearXNG the `SEARXNG_URL` env var is the conventional fallback the
    // provider registry documents.
    cfg.base_url = s
        .base_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .or_else(|| {
            if cfg.provider == "searxng" {
                std::env::var(pantheon_web::websearch::searxng_url_env())
                    .ok()
                    .filter(|u| !u.trim().is_empty())
            } else {
                None
            }
        });
    cfg
}

/// Hermes-style aux inheritance: `provider = "default"` (or absent/empty)
/// inherits the default model's provider; an absent/empty `model` inherits
/// the default model's model. Explicit values always win - slots stay
/// independent, they just don't have to repeat the default. Pure so tests
/// don't touch env.
fn resolve_aux_target(
    provider: Option<&str>,
    model: Option<&str>,
    default: &pantheon_api::model::DefaultModel,
) -> (String, String) {
    let provider = match provider.map(str::trim).filter(|p| !p.is_empty()) {
        None => default.provider.clone(),
        Some(p) if p.eq_ignore_ascii_case("default") => default.provider.clone(),
        Some(p) => p.to_string(),
    };
    let model = model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default.model.clone());
    (provider, model)
}

/// Effective key env var name for an aux pin: an explicit `api_key_env`
/// wins; a default-inheriting provider (`None`, empty, or `"default"`)
/// inherits `[model].api_key_env`; otherwise `None`, and the runtime
/// falls back to `PANTHEON_KEY_<PROVIDER>`.
fn resolve_aux_key_env(
    provider: Option<&str>,
    api_key_env: Option<&str>,
    model: Option<&ModelSection>,
) -> Option<String> {
    if let Some(env) = api_key_env.filter(|s| !s.trim().is_empty()) {
        return Some(env.to_string());
    }
    let inherits = provider
        .map(|p| {
            let p = p.trim();
            p.is_empty() || p.eq_ignore_ascii_case("default")
        })
        .unwrap_or(true);
    if inherits {
        model.and_then(|m| m.api_key_env.clone())
    } else {
        None
    }
}

/// Resolve one slot's target from its section + env pair, applying
/// Hermes-style inheritance against the default model. `PANTHEON_*` env
/// overrides win field-wise over the section; `"default"`/empty then
/// inherits. Returns `None` when nothing is pinned at all (no section,
/// no env) or when even the default target is empty - the `auto`
/// fallback in [`auxiliaries`] decides what that means per slot.
fn slot_target(
    slot: &AuxSlot,
    cfg: Option<&Config>,
    default: &pantheon_api::model::DefaultModel,
) -> Option<(String, String)> {
    let section = cfg.and_then(slot.section);
    let env_provider = std::env::var(format!("PANTHEON_{}_PROVIDER", slot.env_prefix))
        .ok()
        .filter(|v| !v.trim().is_empty());
    let env_model = std::env::var(format!("PANTHEON_{}_MODEL", slot.env_prefix))
        .ok()
        .filter(|v| !v.trim().is_empty());
    if section.is_none() && env_provider.is_none() && env_model.is_none() {
        return None;
    }
    let (provider, model) = resolve_aux_target(
        env_provider
            .as_deref()
            .or_else(|| section.map(|s| s.provider.as_str())),
        env_model
            .as_deref()
            .or_else(|| section.map(|s| s.model.as_str())),
        default,
    );
    if provider.trim().is_empty() || model.trim().is_empty() {
        return None;
    }
    Some((provider, model))
}

/// Resolve one slot's auxiliary entry (pinned target only, no `auto`).
fn slot_aux(
    slot: &AuxSlot,
    cfg: Option<&Config>,
    default: &pantheon_api::model::DefaultModel,
) -> Option<pantheon_api::model::AuxiliaryModel> {
    let (provider, model) = slot_target(slot, cfg, default)?;
    let timeout_secs = cfg
        .and_then(slot.section)
        .and_then(|s| s.timeout)
        .unwrap_or_else(|| slot.kind.default_timeout_secs());
    // `[compression] target_percent` rides the generic slot machinery:
    // the slot abstraction only exposes `&AuxSection`, so the
    // compression-only knob is resolved here, directly from config.
    let target_percent = if slot.kind == pantheon_api::model::AuxiliaryKind::Compression {
        cfg.and_then(|c| c.compression.as_ref())
            .and_then(|s| s.target_percent)
    } else {
        None
    };
    Some(pantheon_api::model::AuxiliaryModel {
        kind: slot.kind.clone(),
        provider,
        model,
        timeout_secs,
        target_percent,
    })
}

/// Resolve the reflection auxiliary model (`AuxiliaryKind::Reflection`).
/// `PANTHEON_REFLECTION_PROVIDER`/`PANTHEON_REFLECTION_MODEL` win, then
/// the `[reflect]` pin, else `auto` (the run's default model). Each field
/// inherits independently: `provider = "default"` (or absent) inherits
/// the default provider, an absent/empty `model` inherits the default
/// model. Reflection keeps its own row outside `AUX_SLOTS` because
/// `[reflect]` is a combined behavior + model-pin table, not a pure
/// [`AuxSection`]; the resolution order is identical.
///
/// Every LLM call the reflection pipeline makes resolves through this
/// slot - never the chat model directly - so pinning a cheap model here
/// keeps background self-improvement off the interactive model's bill.
pub fn reflect_aux_model(
    cfg: Option<&Config>,
    default: &pantheon_api::model::DefaultModel,
) -> pantheon_api::model::AuxiliaryModel {
    use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel};
    let section = cfg.and_then(|c| c.reflect.as_ref());
    let env_provider = std::env::var("PANTHEON_REFLECTION_PROVIDER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let env_model = std::env::var("PANTHEON_REFLECTION_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let (provider, model) = resolve_aux_target(
        env_provider
            .as_deref()
            .or_else(|| section.and_then(|r| r.provider.as_deref())),
        env_model
            .as_deref()
            .or_else(|| section.and_then(|r| r.model.as_deref())),
        default,
    );
    AuxiliaryModel {
        kind: AuxiliaryKind::Reflection,
        provider,
        model,
        timeout_secs: section
            .and_then(|r| r.timeout)
            .unwrap_or_else(|| AuxiliaryKind::Reflection.default_timeout_secs()),
        target_percent: None,
    }
}

/// Resolve the consolidation auxiliary model
/// (`AuxiliaryKind::Consolidation`). `PANTHEON_CONSOLIDATION_PROVIDER` /
/// `PANTHEON_CONSOLIDATION_MODEL` win, then the `[consolidation]` pin,
/// else `auto` (the run's default model). Each field inherits
/// independently: `provider = "default"` (or absent) inherits the
/// default provider, an absent/empty `model` inherits the default model.
/// Consolidation keeps its own resolver outside `AUX_SLOTS` because
/// `[consolidation]` is a combined behavior + model-pin table, not a
/// pure [`AuxSection`]; the resolution order is identical.
///
/// Every LLM call the consolidation pipeline makes resolves through
/// this slot - never the chat model directly - so pinning a cheap model
/// here keeps nightly memory consolidation off the interactive model's
/// bill.
pub fn consolidation_aux_model(
    cfg: Option<&Config>,
    default: &pantheon_api::model::DefaultModel,
) -> pantheon_api::model::AuxiliaryModel {
    use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel};
    let section = cfg.and_then(|c| c.consolidation.as_ref());
    let env_provider = std::env::var("PANTHEON_CONSOLIDATION_PROVIDER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let env_model = std::env::var("PANTHEON_CONSOLIDATION_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let (provider, model) = resolve_aux_target(
        env_provider
            .as_deref()
            .or_else(|| section.and_then(|s| s.provider.as_deref())),
        env_model
            .as_deref()
            .or_else(|| section.and_then(|s| s.model.as_deref())),
        default,
    );
    AuxiliaryModel {
        kind: AuxiliaryKind::Consolidation,
        provider,
        model,
        timeout_secs: section
            .and_then(|s| s.timeout)
            .unwrap_or_else(|| AuxiliaryKind::Consolidation.default_timeout_secs()),
        target_percent: None,
    }
}

/// Master gate for the nightly pass over a whole [`Config`]: the pass
/// pipeline and repair loop - runs iff this is true.
///
/// `[nightly]` present → the single enable rule
/// ([`pantheon_api::config::nightly_enabled`]): explicit flag wins, else
/// the model pin (table or `PANTHEON_NIGHTLY_*` env) implies on. Absent →
/// deprecated migration fallback: the legacy `[reflect]` /
/// `[consolidation]` `enabled` flags were explicit opt-ins, honored
/// field-by-field. Absent everything = off (nightly is off by default).
pub fn nightly_pass_enabled(cfg: Option<&Config>) -> bool {
    match cfg.and_then(|c| c.nightly.as_ref()) {
        Some(n) => pantheon_api::config::nightly_enabled(n),
        None => cfg.is_some_and(|c| {
            c.reflect.as_ref().is_some_and(|s| s.enabled)
                || c.consolidation.as_ref().is_some_and(|s| s.enabled)
        }),
    }
}

/// `[nightly]` → [`pantheon_nightly::NightlyConfig`].
///
/// `[nightly]` is the single authoritative section. When it is present,
/// the legacy `[reflect]` / `[consolidation]` tables are ignored
/// entirely for the pass; when it is absent they are honored
/// field-by-field as a deprecated migration fallback - `llm_enabled` =
/// either legacy flag, `auto_turns` from `[reflect]`, `min_sessions` /
/// `cron` from `[consolidation]`. `max_age_days` has no legacy equivalent
/// (the decay curve is gone), so it always takes the `[nightly]` value or
/// the default.
pub fn nightly_config(
    cfg: Option<&Config>,
    data_dir: &std::path::Path,
) -> pantheon_nightly::NightlyConfig {
    let n = cfg.and_then(|c| c.nightly.as_ref());
    let r = cfg.and_then(|c| c.reflect.as_ref());
    let c = cfg.and_then(|c| c.consolidation.as_ref());
    // Authoritative-when-present: legacy flags must not leak into a pass
    // whose `[nightly]` section exists - a legacy `enabled = true` must
    // not turn on LLM steps the user thought they turned off.
    let llm_enabled = match n {
        // The single enable rule: explicit flag wins, else the model pin
        // implies on. See pantheon_api::config::nightly_enabled.
        Some(s) => pantheon_api::config::nightly_enabled(s),
        None => r.map(|s| s.enabled).unwrap_or(false) || c.map(|s| s.enabled).unwrap_or(false),
    };
    pantheon_nightly::NightlyConfig {
        data_dir: data_dir.to_path_buf(),
        memory_trust: pantheon_api::provenance::TrustTier::Memory,
        since_ms: 0,
        max_runs: 50,
        min_sequence_repeats: 3,
        min_failure_repeats: 3,
        min_preference_hits: 3,
        min_sessions: n
            .map(|s| s.min_sessions)
            .or_else(|| c.map(|s| s.min_sessions))
            .unwrap_or(3),
        max_age_days: n.map(|s| s.max_age_days).unwrap_or(30),
        eval_timeout: std::time::Duration::from_secs(120),
        max_evals: 3,
        replay_command: n.and_then(|s| s.replay_command.clone()),
        llm_enabled,
        max_fix_attempts: pantheon_nightly::DEFAULT_MAX_FIX_ATTEMPTS,
        // Repair-phase bounds from the `[nightly]` section, falling
        // back to the workstream defaults when unset.
        mcp_max_failures: n.and_then(|s| s.repair_mcp_max_failures).unwrap_or(5),
        schedule_max_failures: n.and_then(|s| s.repair_schedule_max_failures).unwrap_or(3),
        tool_min_calls: n.and_then(|s| s.repair_tool_min_calls).unwrap_or(3),
        tool_probe_allowlist: n
            .and_then(|s| s.repair_tool_probe_allowlist.clone())
            .unwrap_or_default(),
        dry_run: false,
    }
}

/// The `[nightly]` cron: `[nightly].cron`, else legacy
/// `[consolidation].cron`, else the default.
pub fn nightly_cron(cfg: Option<&Config>) -> String {
    cfg.and_then(|c| c.nightly.as_ref())
        .map(|s| s.cron.clone())
        .or_else(|| {
            cfg.and_then(|c| c.consolidation.as_ref())
                .map(|s| s.cron.clone())
        })
        .unwrap_or_else(default_consolidation_cron)
}

/// The `[nightly]` auto-turns trigger: `[nightly].auto_turns`, else
/// legacy `[reflect].auto_turns`, else the default.
pub fn nightly_auto_turns(cfg: Option<&Config>) -> u32 {
    cfg.and_then(|c| c.nightly.as_ref())
        .map(|s| s.auto_turns)
        .or_else(|| cfg.and_then(|c| c.reflect.as_ref()).map(|s| s.auto_turns))
        .unwrap_or(DEFAULT_REFLECT_AUTO_TURNS)
}

/// The `[nightly]` replay command for held-out task replays.
pub fn nightly_replay_command(cfg: Option<&Config>) -> Option<String> {
    cfg.and_then(|c| c.nightly.as_ref())
        .and_then(|s| s.replay_command.clone())
}

/// Seed a named vault entry from an env-var name in config, so the session
/// resolves aux endpoint keys at the execution boundary like every other
/// secret. Missing section, missing env, or unset var = no-op.
fn seed_env_key(
    secrets: pantheon_secrets::SecretsBroker,
    env: Option<String>,
    vault_name: &'static str,
) -> pantheon_secrets::SecretsBroker {
    let Some(env) = env else {
        return secrets;
    };
    let Ok(value) = std::env::var(&env) else {
        return secrets;
    };
    let mem = pantheon_secrets::MemoryVault::new();
    let _ = mem.set(vault_name, pantheon_secrets::SecretValue::new(value));
    secrets.with_vault(Box::new(mem))
}

/// Seed every configured aux key in one call (the session-builder sites'
/// entry point). A default-inheriting provider seeds from
/// `[model].api_key_env` via [`resolve_aux_key_env`], so an aux slot that
/// inherits the default model also inherits its key without repeating it.
pub fn with_aux_keys(
    secrets: pantheon_secrets::SecretsBroker,
    cfg: Option<&Config>,
) -> pantheon_secrets::SecretsBroker {
    let mut secrets = secrets;
    let model_section = cfg.and_then(|c| c.model.as_ref());
    for slot in AUX_SLOTS {
        let env = cfg.and_then(slot.section).and_then(|s| {
            resolve_aux_key_env(
                Some(s.provider.as_str()),
                s.api_key_env.as_deref(),
                model_section,
            )
        });
        secrets = seed_env_key(secrets, env, slot.vault_name);
    }
    // `[reflect]` is not an AUX_SLOTS row (combined behavior + pin
    // table); seed its key the same way, with inheritance.
    let reflect_env = cfg.and_then(|c| c.reflect.as_ref()).and_then(|r| {
        resolve_aux_key_env(
            r.provider.as_deref(),
            r.api_key_env.as_deref(),
            model_section,
        )
    });
    secrets = seed_env_key(secrets, reflect_env, "PANTHEON_REFLECTION_API_KEY");
    // `[consolidation]` likewise: its key seeds PANTHEON_CONSOLIDATION_API_KEY.
    let consolidate_env = cfg.and_then(|c| c.consolidation.as_ref()).and_then(|s| {
        resolve_aux_key_env(
            s.provider.as_deref(),
            s.api_key_env.as_deref(),
            model_section,
        )
    });
    secrets = seed_env_key(secrets, consolidate_env, "PANTHEON_CONSOLIDATION_API_KEY");
    // `[nightly]` is its own aux slot now (`[nightly.model]`): seed its
    // key the same way, with inheritance - the pass's key lookup tries
    // PANTHEON_NIGHTLY_API_KEY first.
    let nightly_env = cfg
        .and_then(|c| c.nightly.as_ref())
        .and_then(|n| n.model.as_ref())
        .and_then(|m| {
            resolve_aux_key_env(
                Some(m.provider.as_str()),
                m.api_key_env.as_deref(),
                model_section,
            )
        });
    secrets = seed_env_key(secrets, nightly_env, "PANTHEON_NIGHTLY_API_KEY");
    secrets
}

/// `[model].api_key_env` → the env-var name holding the chat model key.
/// Never the key itself; `None` = the default `PANTHEON_API_KEY` path.
pub fn model_key_env(cfg: Option<&Config>) -> Option<String> {
    cfg.and_then(|c| c.model.as_ref())
        .and_then(|m| m.api_key_env.clone())
}

/// Env-var-safe version of a provider id: `my-llm` → `MY_LLM`.
/// Delegates to [`pantheon_providers::catalog::env_part`] - one cleaner for
/// every `PANTHEON_*` name.
pub fn sanitize_env_suffix(id: &str) -> String {
    pantheon_providers::catalog::env_part(id)
}

/// Effective key env var for a provider id: explicit `key_env` wins,
/// otherwise `PANTHEON_KEY_<ID>`. Naming only - no env lookup. (The
/// runtime read path is `catalog::key_for`, which resolves this same name
/// against the environment; keep the two in agreement via
/// `catalog::env_part`.)
pub fn provider_key_env(provider_id: &str, explicit: Option<&str>) -> String {
    explicit
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| format!("PANTHEON_KEY_{}", sanitize_env_suffix(provider_id)))
}

/// Insert (or replace) one `[custom_providers.<name>]` row, persist the
/// config, and register it with the global catalog. The single writer
/// behind `pantheon provider add` and the `pantheon model` builtin-override
/// path. Returns whether the id shadows a builtin catalog id (the caller
/// announces it).
pub fn upsert_custom_row(
    data_dir: &std::path::Path,
    name: &str,
    base_url: &str,
    api_mode: pantheon_providers::catalog::ApiMode,
    key_env: &str,
) -> Result<bool, String> {
    use pantheon_providers::catalog::ApiMode as Mode;
    let mut cfg = Config::load(data_dir).unwrap_or_default();
    let shadow = pantheon_providers::catalog::providers()
        .iter()
        .any(|p| p.id == name);
    cfg.custom_providers.insert(
        name.to_string(),
        CustomProviderSection {
            base_url: base_url.to_string(),
            api_mode: match api_mode {
                Mode::OpenAi => "openai".into(),
                Mode::Anthropic => "anthropic".into(),
            },
            key_env: Some(key_env.to_string()),
            // Preserve whatever the operator has already named on this
            // endpoint. Replacing the whole row used to drop them, so
            // re-picking a provider silently emptied its model list.
            models: cfg
                .custom_providers
                .get(name)
                .map(|s| s.models.clone())
                .unwrap_or_default(),
        },
    );
    cfg.save(data_dir).map_err(|e| e.to_string())?;
    register_custom_providers(&cfg);
    Ok(shadow)
}

/// Record a model the operator named by hand on a custom endpoint.
///
/// This is the only thing that ever adds a row to `[custom_providers.*].models`.
/// A model list harvested from another agent's config is deliberately *not*
/// written: it is a snapshot of a third-party endpoint and goes stale. This
/// records a name a human actually chose, which stays true.
///
/// Idempotent, and preserves any limits already known for that model.
pub fn remember_custom_model(
    data_dir: &std::path::Path,
    provider: &str,
    model: &str,
) -> Result<bool, String> {
    let model = model.trim();
    if model.is_empty() {
        return Ok(false);
    }
    let mut cfg = Config::load(data_dir).unwrap_or_default();
    let Some(sec) = cfg.custom_providers.get_mut(provider) else {
        return Err(format!(
            "no custom provider {provider:?}; add it with `pantheon provider add --name {provider}`"
        ));
    };
    if sec.models.iter().any(|m| m.id == model) {
        return Ok(false);
    }
    sec.models.push(CustomModel {
        id: model.to_string(),
        context_limit: None,
        max_output_tokens: None,
        video: false,
    });
    sec.models.sort_by(|a, b| a.id.cmp(&b.id));
    cfg.save(data_dir).map_err(|e| e.to_string())?;
    register_custom_providers(&cfg);
    Ok(true)
}

/// Load `<data_dir>/.env` (exports always win) and register
/// `[custom_providers.*]` with the catalog. Every verb entry calls this
/// once before doing anything else; forgetting it silently breaks
/// template and key resolution.
pub fn init_env_and_catalog(data_dir: &std::path::Path) {
    crate::dotenv::load_dotenv(data_dir);
    if let Ok(cfg) = Config::load(data_dir) {
        register_custom_providers(&cfg);
    }
}

/// Register every `[custom_providers.*]` entry with the global catalog so
/// the runtime resolves base URL / wire mode / key env for them. Called
/// once at terminal startup after loading the config; idempotent.
pub fn register_custom_providers(cfg: &Config) {
    for (name, sec) in &cfg.custom_providers {
        if sec.base_url.trim().is_empty() {
            continue;
        }
        let api_mode = match sec.api_mode.trim().to_ascii_lowercase().as_str() {
            "anthropic" => pantheon_providers::catalog::ApiMode::Anthropic,
            _ => pantheon_providers::catalog::ApiMode::OpenAi,
        };
        // Register the endpoint's models so `pantheon providers` lists them and
        // the model picker can name one. `model_meta` supplies conservative
        // defaults (tools on, vision/reasoning off, streaming on) and any
        // declared limit overrides them.
        let models: Vec<pantheon_providers::catalog::ModelMeta> = sec
            .models
            .iter()
            .filter(|m| !m.id.trim().is_empty())
            .map(|m| {
                let mut meta = pantheon_providers::catalog::model_meta(name, m.id.trim());
                if let Some(c) = m.context_limit {
                    meta.context_limit = Some(c);
                }
                if let Some(o) = m.max_output_tokens {
                    meta.max_output_tokens = Some(o);
                }
                // Operator-declared capability: a custom endpoint's model
                // flagged `video = true` is trusted to take native video
                // input (e.g. Qwen-Omni on an OpenAI-compatible endpoint).
                if m.video {
                    meta.video = true;
                }
                meta
            })
            .collect();
        pantheon_providers::catalog::register_custom_provider(
            pantheon_providers::catalog::ProviderMeta {
                id: name.clone(),
                label: name.clone(),
                base_url: sec.base_url.trim().trim_end_matches('/').to_string(),
                api_mode,
                base_env: String::new(),
                key_env: provider_key_env(name, sec.key_env.as_deref()),
                key_header: "Authorization".into(),
                models,
                prominent: true,
                recommended: false,
                dev: false,
                tag: "custom".into(),
            },
        );
    }
}

/// The one secrets broker every session-builder site constructs: the chat
/// model key (config-named env var, else `PANTHEON_API_KEY`), then every
/// aux key (judge, compression, title, embeddings, search synthesis,
/// vision, scheduled, MCP synthesis), environment fallback last.
///
/// Session, gateway, pipeline, and `chat --key` all share this so a key
/// configured once resolves identically on every path. An explicit `--key`
/// flag is layered on top with
/// [`SecretsBroker::with_vault_front`](pantheon_secrets::SecretsBroker::with_vault_front)
/// so the flag beats config and environment.
pub fn chat_secrets(cfg: Option<&Config>) -> pantheon_secrets::SecretsBroker {
    let broker = with_aux_keys(
        pantheon_secrets::SecretsBroker::from_system_env_with_api_key(
            model_key_env(cfg).as_deref(),
        ),
        cfg,
    );
    // A `set` needs a writable durable vault. The env mirror is read-only
    // by design, so on hosts with no usable OS keychain (headless Linux)
    // every write died with "read-only vault". The encrypted local file
    // vault is the documented fallback for exactly this case; it lives
    // under the data dir next to config.toml. Reads still prefer the
    // environment, so a rotated exported key beats a stale stored one.
    let mut broker = broker;
    if pantheon_secrets::KeychainVault::platform_available().is_err() {
        let dir = crate::terminal::data_dir();
        if let Ok(vault) = pantheon_secrets::EncryptedFileVault::open(
            dir.join("secrets.json"),
            dir.join(".secrets.key"),
        ) {
            broker = broker.with_vault(Box::new(vault));
        }
    }
    // Fail closed by default: without a `[secrets]` section both
    // allowlists are empty, so `env:` lookups resolve nothing and plugin
    // subprocesses receive no host vars beyond the curated minimum.
    match cfg.and_then(|c| c.secrets.as_ref()) {
        Some(s) => broker
            .with_env_allowlist(s.env_allowlist.clone())
            .with_plugin_env_allowlist(s.plugin_env_allowlist.clone()),
        None => broker,
    }
}

/// Resolve the model policy for one session: explicit override > environment >
/// `config.toml` > the hardcoded local default, plus the configured fallback
/// chain and every auxiliary slot.
///
/// This lives here, not in an interface module, because it is a property of
/// the config document rather than of any surface. The TUI, the AG-UI
/// session factory, and the scheduler all resolve a session the same way, and
/// a resolution rule that lives beside a UI keeps drifting from the one
/// beside the verb that replaced it.
pub fn build_model_policy(
    cfg: Option<&Config>,
    provider: Option<String>,
    model: Option<String>,
) -> pantheon_api::model::ModelPolicy {
    let cfg_model = cfg
        .and_then(|c| c.model.clone())
        .map(|m| (m.provider, m.model));
    let default = pantheon_api::model::DefaultModel {
        provider: provider
            .or_else(|| cfg_model.as_ref().map(|(p, _)| p.clone()))
            .or_else(|| std::env::var("PANTHEON_PROVIDER").ok())
            .unwrap_or_else(|| "local".into()),
        model: model
            .or_else(|| cfg_model.as_ref().map(|(_, m)| m.clone()))
            .or_else(|| std::env::var("PANTHEON_MODEL").ok())
            .unwrap_or_else(|| "llama3.2".into()),
    };
    let mut chain = pantheon_api::model::FallbackChain::default();
    if let Some(fallbacks) = cfg
        .and_then(|c| c.model.as_ref())
        .map(|m| m.fallbacks.clone())
    {
        for f in fallbacks {
            chain.fallbacks.push(pantheon_api::model::DefaultModel {
                provider: f.provider,
                model: f.model,
            });
        }
    }
    pantheon_api::model::ModelPolicy {
        reasoning_budget: resolve_reasoning_budget(cfg),
        reasoning: resolve_reasoning(cfg),
        default: default.clone(),
        fallbacks: chain,
        auxiliaries: auxiliaries(cfg, &default),
    }
}

/// Build the model policy for a scheduled job run.
///
/// Model rule for scheduled work, in precedence order:
///
/// 1. **Explicit pin** - `--model`/`--provider` on `schedule create`, or a
///    template's `model` var (which becomes a pin). Always wins.
/// 2. **Scheduled auxiliary** - the `[scheduled]` config section (or
///    `PANTHEON_SCHEDULED_PROVIDER`/`PANTHEON_SCHEDULED_MODEL`). This is the
///    default for unpinned jobs.
/// 3. **Never the interactive default** - unless the `[scheduled]` slot
///    itself resolves to it (`auto` with no pin configured).
///
/// Scheduled work is background work: it burns cheap tokens by default.
/// Before this, an unpinned job silently used the interactive chat model,
/// so configuring `[scheduled]` changed nothing at fire time.
pub fn build_scheduled_model_policy(
    cfg: Option<&Config>,
    provider: Option<String>,
    model: Option<String>,
) -> pantheon_api::model::ModelPolicy {
    // Resolve pins against config/env exactly like the interactive path, so
    // a partial pin (--model with no --provider) keeps today's fallback.
    let mut policy = build_model_policy(cfg, provider.clone(), model.clone());
    if provider.is_none() && model.is_none() {
        if let Some(aux) = policy
            .auxiliaries
            .iter()
            .find(|a| a.kind == pantheon_api::model::AuxiliaryKind::Scheduled)
        {
            policy.default = pantheon_api::model::DefaultModel {
                provider: aux.provider.clone(),
                model: aux.model.clone(),
            };
        }
    }
    policy
}

/// Exact thinking budget: `PANTHEON_REASONING_BUDGET` wins, then
/// `[model].reasoning_budget`. Zero disables (reads as "no budget").
/// Applies to budget wires only; effort-string wires ignore it.
fn resolve_reasoning_budget(cfg: Option<&Config>) -> Option<u32> {
    if let Ok(v) = std::env::var("PANTHEON_REASONING_BUDGET") {
        if let Ok(n) = v.trim().parse::<u32>() {
            return Some(n);
        }
    }
    cfg.and_then(|c| c.model.as_ref())
        .and_then(|m| m.reasoning_budget)
}

/// Reasoning effort for chat turns: `PANTHEON_REASONING` wins, then
/// `[model].reasoning`, then off. Unknown strings resolve to off - the
/// safe direction is sending no param, and `doctor` flags the typo (see
/// `Config::validate`) rather than failing the session.
fn resolve_reasoning(cfg: Option<&Config>) -> pantheon_api::model::ReasoningLevel {
    use pantheon_api::model::ReasoningLevel;
    if let Ok(v) = std::env::var("PANTHEON_REASONING") {
        if let Some(level) = ReasoningLevel::parse(&v) {
            return level;
        }
    }
    cfg.and_then(|c| c.model.as_ref())
        .and_then(|m| m.reasoning.as_deref())
        .and_then(ReasoningLevel::parse)
        .unwrap_or_default()
}

/// Every auxiliary for this host with a resolved target: an explicit
/// `[judge]` / `[compression]` / `[title_gen]` / `[search_synthesis]` /
/// `[vision]` / `[scheduled]` / `[mcp_synthesis]` / `[extraction]` /
/// `[rerank]` / `[planner]` / `[repair]` / `[verify]` section (or its env
/// override) wins; otherwise `auto` - the run's default model. Aux
/// models default to auto, so an absent section never switches a
/// capability off, it just means "use what you already use for chat".
///
/// The documented exceptions are `Embeddings`, `Repair`, and `Verify`:
/// absent = the local hashing embedder (never the chat model), the
/// repair slot OFF (fix-loop draft revision unavailable - plain
/// retries, then escalation), and the verifier OFF (never
/// auto-verified) - so an entry appears only when `[embeddings]` /
/// `[repair]` / `[verify]` (or their env) actually pins a target.
pub fn auxiliaries(
    cfg: Option<&Config>,
    default: &pantheon_api::model::DefaultModel,
) -> Vec<pantheon_api::model::AuxiliaryModel> {
    use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel};
    let auto = |kind: AuxiliaryKind| {
        let timeout_secs = kind.default_timeout_secs();
        AuxiliaryModel {
            kind,
            provider: default.provider.clone(),
            model: default.model.clone(),
            timeout_secs,
            // Absent `[compression]` = the historic default target.
            // A pinned section goes through `slot_aux`, which reads the
            // knob from config.
            target_percent: None,
        }
    };
    let mut out = Vec::with_capacity(AUX_SLOTS.len() + 1);
    // The Tools screen gates the vision and video-analysis aux entries:
    // a disabled group means the entry never resolves, so a configured
    // `[vision]` / `[video]` section cannot be consulted by accident.
    let enablement = tool_enablement(cfg);
    let aux_gated = |kind: &AuxiliaryKind| -> bool {
        let group = match kind {
            AuxiliaryKind::Vision => Some(pantheon_api::config::ToolGroup::Vision),
            AuxiliaryKind::Video => Some(pantheon_api::config::ToolGroup::VideoAnalysis),
            _ => None,
        };
        group.map(|g| !enablement.is_enabled(g)).unwrap_or(false)
    };
    for slot in AUX_SLOTS {
        if aux_gated(&slot.kind) {
            continue;
        }
        match slot_aux(slot, cfg, default) {
            Some(pinned) => out.push(pinned),
            // Embeddings is the documented exception: absent = the local
            // hashing embedder, never the chat model - no `auto` entry.
            None if slot.auto => out.push(auto(slot.kind.clone())),
            None => {}
        }
    }
    // Reflection is always an `auto`-eligible slot: absent `[reflect]`
    // pin = the run's default model.
    out.push(reflect_aux_model(cfg, default));
    // Consolidation is always an `auto`-eligible slot too: absent
    // `[consolidation]` pin = the run's default model. The distill step
    // resolves through this entry and can never borrow the chat model
    // directly.
    out.push(consolidation_aux_model(cfg, default));
    out
}

/// Resolve `[mcp]` plus imported declarations into the runtime
/// [`pantheon_runtime::tool_config::McpToolConfig`].
///
/// The config section is primary: entries that are disabled or
/// malformed (stdio without a command, remote without a url, unknown
/// transport) are skipped with a stderr warning. Migration declarations
/// (`<data_dir>/mcp/*.json`) fill in names the section does not define;
/// a declaration's `requires_env` names become `env:` refs so the
/// session's secrets broker resolves them at launch time.
///
/// The launcher is on when the `[mcp]` section is present (its
/// `enabled`, default true) or when declarations exist; absent both =
/// off. `PANTHEON_MCP_ENABLED=0` forces it off.
pub fn resolve_mcp_section(
    section: Option<&McpSection>,
    declarations: &[pantheon_migration::McpDeclaration],
) -> pantheon_runtime::tool_config::McpToolConfig {
    use pantheon_mcp::manager::{McpServerSpec, McpTransport};
    use std::time::Duration;

    let env_off = std::env::var("PANTHEON_MCP_ENABLED")
        .map(|v| v == "0")
        .unwrap_or(false);
    let mut cfg = pantheon_runtime::tool_config::McpToolConfig::default();
    let mut specs: Vec<McpServerSpec> = Vec::new();

    if let Some(sec) = section {
        cfg.enabled = !env_off && sec.enabled.unwrap_or(true);
        for (name, e) in &sec.servers {
            if !e.enabled {
                continue;
            }
            let transport = match e.transport.as_str() {
                "stdio" => McpTransport::Stdio,
                "sse" => McpTransport::Sse,
                "http" => McpTransport::Http,
                other => {
                    eprintln!("mcp: server '{name}': unknown transport {other:?}, skipped");
                    continue;
                }
            };
            let command = e.command.clone().filter(|c| !c.trim().is_empty());
            let url = e.url.clone().filter(|u| !u.trim().is_empty());
            match transport {
                McpTransport::Stdio if command.is_none() => {
                    eprintln!("mcp: server '{name}': stdio needs a command, skipped");
                    continue;
                }
                McpTransport::Stdio => {}
                _ if url.is_none() => {
                    eprintln!("mcp: server '{name}': remote transport needs a url, skipped");
                    continue;
                }
                _ => {}
            }
            specs.push(McpServerSpec {
                name: name.clone(),
                transport,
                command,
                args: e.args.clone(),
                env: e.env.clone(),
                url,
                enabled: e.enabled,
                timeout: Duration::from_secs(e.timeout_secs.filter(|&s| s > 0).unwrap_or(30)),
                headers: resolve_headers(&e.headers),
            });
        }
    } else if !declarations.is_empty() {
        cfg.enabled = !env_off;
    }

    // Declarations fill names the config section does not define.
    for decl in declarations {
        for d in &decl.servers {
            if !d.enabled || specs.iter().any(|s| s.name == d.name) {
                continue;
            }
            let transport = match d.transport.as_str() {
                "stdio" => McpTransport::Stdio,
                "sse" => McpTransport::Sse,
                "http" => McpTransport::Http,
                other => {
                    eprintln!(
                        "mcp: declared server '{}': unknown transport {other:?}, skipped",
                        d.name
                    );
                    continue;
                }
            };
            let command = d.command.clone().filter(|c| !c.trim().is_empty());
            let url = d.url.clone().filter(|u| !u.trim().is_empty());
            let ready = match transport {
                McpTransport::Stdio => command.is_some(),
                _ => url.is_some(),
            };
            if !ready {
                eprintln!(
                    "mcp: declared server '{}': incomplete (no command/url), skipped",
                    d.name
                );
                continue;
            }
            let env = d
                .requires_env
                .iter()
                .map(|v| (v.clone(), format!("env:{v}")))
                .collect();
            specs.push(McpServerSpec {
                name: d.name.clone(),
                transport,
                command,
                args: d.args.clone(),
                env,
                url,
                enabled: d.enabled,
                timeout: Duration::from_secs(30),
                headers: Default::default(),
            });
        }
    }

    cfg.servers = specs;
    cfg
}
