//! config_group: config, config_schema, config_verb, migrate, reset, uninstall, update, worktree, richtext, gateway, mcp, notify, fallback, init, pipeline, plugin_toggle, plugins_verb, logs in one file.

pub mod config {
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

    /// Resolve config `[mcp.servers.<name>]` headers. Literal values pass
    /// through; `env:NAME` refs are kept as refs, NOT resolved here, so
    /// the manager can resolve them at connect time through its
    /// broker-first env resolver (a vault-only token never reaches the
    /// server if it is resolved from process env only at load time, and
    /// a header whose variable is unset must not be silently dropped -
    /// that surfaces only as a later 401/backoff). An unset process-env
    /// variable still earns a warning: the broker may supply it at
    /// connect, but if it does not, connect fails loudly there.
    fn resolve_headers(
        headers: &std::collections::HashMap<String, String>,
    ) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        for (k, v) in headers {
            match v.strip_prefix("env:") {
                Some(name) => {
                    if std::env::var(name).is_err() {
                        eprintln!(
                            "mcp: header '{k}' references env '{name}' (not set in process env; resolved at connect time via the secrets broker if available)"
                        );
                    }
                    out.insert(k.clone(), v.clone());
                }
                None => {
                    if !v.is_empty() {
                        out.insert(k.clone(), v.clone());
                    }
                }
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
    pub fn apply_tool_enablement(
        session: &pantheon_runtime::session::Session,
        cfg: Option<&Config>,
    ) {
        session.set_tool_enablement(tool_enablement(cfg));
    }

    /// Whether a tool group is enabled in a loaded config. Used for
    /// out-of-registry gates: the `/voice` status command and the
    /// vision/video auxiliary entries.
    pub fn tool_group_enabled(
        cfg: Option<&Config>,
        group: pantheon_api::config::ToolGroup,
    ) -> bool {
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

    /// Thread the `[memory]` section into a freshly built session, so the
    /// user-facing config is authoritative: on load, `[memory] backend`
    /// wins over `memory-backend.toml`, and the resolved selection is
    /// synced back to that file so the two never drift silently. A
    /// conflict logs a warning (the message names both backends). Call
    /// BEFORE `Session::new`: the session reads the selection file at
    /// construction, so syncing after it would take effect one session
    /// late. Absent/blank backend = no opinion, the file wins untouched.
    /// Call alongside `apply_budget_tiers` on every `Session::new` that
    /// serves turns.
    pub fn sync_memory_selection(data_dir: &std::path::Path, cfg: Option<&Config>) {
        if let Some(section) = cfg.and_then(|c| c.memory.as_ref()) {
            if let Some(warning) = pantheon_memory::sync_selection_from_config(
                data_dir,
                &section.backend,
                &section.options,
            ) {
                eprintln!("{warning}");
            }
        }
    }
    /// so the `delegate` tool resolves its optional `route` argument.
    /// Absent section = no routes: routed calls are refused, direct
    /// calls work as before. Call alongside `apply_budget_tiers` on
    /// every `Session::new` that serves turns, for the same drift reason.
    pub fn apply_delegation_section(
        session: &pantheon_runtime::session::Session,
        cfg: Option<&Config>,
    ) {
        if let Some(section) = cfg.and_then(|c| c.delegation.clone()) {
            session.set_delegation_section(section);
        }
    }

    /// All three config wirings for a fresh turn-serving session, as one
    /// call: [tools] enablement, [budget] tiers, and the [delegation]
    /// section. Every `Session::new` that serves turns must call this
    /// before the first turn; calling the three applies piecemeal is how
    /// headless paths silently drifted from the interactive session (the
    /// delivered `run` path applied none of them, swarm children missed
    /// the budget tiers, pipeline sessions missed budget + delegation).
    pub fn apply_session_config(
        session: &pantheon_runtime::session::Session,
        cfg: Option<&Config>,
    ) {
        apply_tool_enablement(session, cfg);
        apply_budget_tiers(session, cfg);
        apply_delegation_section(session, cfg);
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

            // No config key for the warning threshold (out of scope for the
            // nudge patch); the default pairs with the default cap. The
            // predicate suppresses the warning when warn_at >= cap.
            warn_consecutive_tool_failures: 3,
            max_cost_cents: s.max_cost_cents.filter(|&v| v > 0),
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
    pub fn resolve_websearch_section(
        s: &WebsearchSection,
    ) -> pantheon_runtime::WebsearchToolConfig {
        let mut cfg = pantheon_runtime::WebsearchToolConfig::default();
        // Same parsing as the browser flag: `v != "0"` meant
        // PANTHEON_WEBSEARCH_ENABLED=false still enabled search. Unknown
        // values fall back to the file value instead of silently enabling.
        cfg.enabled = std::env::var("PANTHEON_WEBSEARCH_ENABLED")
            .ok()
            .and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            })
            .unwrap_or_else(|| s.enabled.unwrap_or(cfg.enabled));
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
        // Per-provider endpoint overrides travel as a map and are resolved
        // by provider id at the use site, so one provider's URL can never
        // follow a provider switch into another provider's requests.
        cfg.base_urls = s.base_urls.clone();
        // SearXNG chain (env-wins is the project convention for overrides):
        // `SEARXNG_URL` > `[websearch.base_urls.searxng]` > legacy global
        // `base_url` as a searxng-only compatibility shim. The env var is
        // never applied to other providers, so a stray SEARXNG_URL cannot
        // redirect a hosted provider's keyed calls; and the legacy shim is
        // scoped to searxng alone, so it cannot leak anywhere else either.
        // The session warns on any legacy value so operators move it to
        // `[websearch.base_urls.searxng]`; the shim only keeps self-hosted
        // instances working until they do.
        if cfg.provider.eq_ignore_ascii_case("searxng") {
            if let Ok(u) = std::env::var(pantheon_web::websearch::searxng_url_env()) {
                if !u.trim().is_empty() {
                    cfg.base_urls.insert("searxng".to_string(), u);
                }
            } else if cfg
                .base_urls
                .get("searxng")
                .map(|v| v.trim().is_empty())
                .unwrap_or(true)
            {
                if let Some(u) = s.base_url.clone().filter(|u| !u.trim().is_empty()) {
                    cfg.base_urls.insert("searxng".to_string(), u);
                }
            }
        }
        // Legacy global retained ONLY so the session can warn about it; it
        // is never passed to build_provider (see base_url_for).
        cfg.base_url = s.base_url.clone().filter(|u| !u.trim().is_empty());
        cfg
    }

    /// Default-model aux inheritance: `provider = "default"` (or absent/empty)
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
    /// Inheritance against the default model. `PANTHEON_*` env
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
        // Resolution order: env > [nightly.model] pin > [reflect] > auto.
        // A [nightly.model] pin means "use this model for the whole nightly
        // pass" - it is the primary pin, and [reflect] is a finer-grained
        // override for the reflection step specifically.
        let nightly = cfg
            .and_then(|c| c.nightly.as_ref())
            .and_then(|n| n.model.as_ref());
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
                .or_else(|| nightly.as_ref().map(|m| m.provider.as_str()))
                .or_else(|| section.and_then(|r| r.provider.as_deref())),
            env_model
                .as_deref()
                .or_else(|| nightly.as_ref().map(|m| m.model.as_str()))
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
        // Same resolution order as reflect_aux_model: env > [nightly.model]
        // pin > [consolidation] > auto. The [nightly.model] pin is the
        // primary pin for the whole nightly pass; [consolidation] is a
        // finer-grained override for the consolidation step specifically.
        let nightly = cfg
            .and_then(|c| c.nightly.as_ref())
            .and_then(|n| n.model.as_ref());
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
                .or_else(|| nightly.as_ref().map(|m| m.provider.as_str()))
                .or_else(|| section.and_then(|s| s.provider.as_deref())),
            env_model
                .as_deref()
                .or_else(|| nightly.as_ref().map(|m| m.model.as_str()))
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
        // A partial [nightly] table (cron only) no longer switches a
        // previously enabled legacy pass off: see
        // `nightly_enabled_with_legacy` - only an explicit flag or pin in
        // [nightly] overrides the legacy flags.
        let legacy = cfg.is_some_and(|c| {
            c.reflect.as_ref().is_some_and(|s| s.enabled)
                || c.consolidation.as_ref().is_some_and(|s| s.enabled)
        });
        match cfg.and_then(|c| c.nightly.as_ref()) {
            Some(n) => pantheon_api::config::nightly_enabled_with_legacy(n, legacy),
            None => legacy,
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
        // Authoritative-when-present, except for enablement: an explicit
        // `[nightly]` flag or pin overrides legacy, but a `[nightly]`
        // table that decides nothing (cron only) defers to the legacy
        // flags instead of silently switching a previously enabled pass off.
        // A legacy `enabled = true` still never overrides an explicit
        // `[nightly] enabled = false`.
        let legacy_llm =
            r.map(|s| s.enabled).unwrap_or(false) || c.map(|s| s.enabled).unwrap_or(false);
        let llm_enabled = match n {
            // The single enable rule, with the partial-table legacy fallback:
            // explicit flag wins, else the model pin implies on, else a
            // [nightly] table that decides nothing defers to legacy.
            Some(s) => pantheon_api::config::nightly_enabled_with_legacy(s, legacy_llm),
            None => legacy_llm,
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
        let mut cfg = Config::load_for_save(data_dir).map_err(|e| e.to_string())?;
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
                    Mode::Gemini => "gemini".into(),
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
        let mut cfg = Config::load_for_save(data_dir).map_err(|e| e.to_string())?;
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
            vision: false,
            tools: true,
            streaming: true,
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
            register_plugin_providers(data_dir, &cfg);
        }
    }

    /// Register providers declared in plugin manifests. A plugin with a
    /// provider block registers an OpenAI compatible endpoint without
    /// recompiling Pantheon. Bundled plugins skip approval, third party
    /// plugins need approval recorded in the approval store.
    pub fn register_plugin_providers(data_dir: &std::path::Path, cfg: &Config) {
        let project_root = std::env::current_dir().unwrap_or_else(|_| data_dir.to_path_buf());
        let discovered = pantheon_exec::plugins::discover_plugins(data_dir, &project_root);
        for plugin in &discovered {
            let Some(reg) = &plugin.manifest.provider else {
                continue;
            };
            if reg.id.trim().is_empty() || reg.base_url.trim().is_empty() {
                continue;
            }
            // Bundled plugins are first party, third party need approval.
            let approved = pantheon_exec::plugin_approval::is_canonical_bundled(plugin)
                || pantheon_exec::plugin_approval::is_approved(plugin);
            if !approved {
                continue;
            }
            let api_mode = match reg.api_mode.trim().to_ascii_lowercase().as_str() {
                "anthropic" => pantheon_providers::catalog::ApiMode::Anthropic,
                "gemini" => pantheon_providers::catalog::ApiMode::Gemini,
                _ => pantheon_providers::catalog::ApiMode::OpenAi,
            };
            let models: Vec<pantheon_providers::catalog::ModelMeta> = reg
                .models
                .iter()
                .filter(|m| !m.model.trim().is_empty())
                .map(|m| {
                    let mut meta = pantheon_providers::catalog::model_meta(&reg.id, m.model.trim());
                    if let Some(c) = m.context_limit {
                        meta.context_limit = Some(c);
                    }
                    meta
                })
                .collect();
            let label = if reg.label.trim().is_empty() {
                reg.id.clone()
            } else {
                reg.label.clone()
            };
            pantheon_providers::catalog::register_custom_provider(
                pantheon_providers::catalog::ProviderMeta {
                    id: reg.id.clone(),
                    label,
                    base_url: reg.base_url.trim().trim_end_matches('/').to_string(),
                    api_mode,
                    base_env: String::new(),
                    key_env: reg.key_env.clone(),
                    key_header: "Authorization".into(),
                    models,
                    prominent: true,
                    recommended: false,
                    dev: false,
                    tag: "plugin".into(),
                    auth_command: None,
                    auth_refresh_ms: None,
                    reserved: false,
                    allowed_params: vec![],
                    oauth_apps: vec![],
                    sigv4: false,
                    reasoning_map: Default::default(),
                },
            );
            let _ = cfg;
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
                    // Same trust model for the other capabilities: without
                    // these, a vision-capable custom model kept
                    // `vision=false` and the chain vision gate refused its
                    // image turns. Defaults (vision off, tools/streaming
                    // on) match the conservative `model_meta` defaults, so
                    // a row that declares nothing behaves as before.
                    if m.vision {
                        meta.vision = true;
                    }
                    meta.tools = m.tools;
                    meta.streaming = m.streaming;
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
                    auth_command: None,
                    auth_refresh_ms: None,
                    reserved: false,
                    allowed_params: vec![],
                    oauth_apps: vec![],
                    sigv4: false,
                    reasoning_map: Default::default(),
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

    /// Build model policy with phase-aware routing applied.
    ///
    /// If `[routing]` has a pin for `task_type`, it overrides the default
    /// provider/model. Explicit `provider`/`model` args still win.
    pub fn build_routed_model_policy(
        cfg: Option<&Config>,
        provider: Option<String>,
        model: Option<String>,
        task_type: Option<&str>,
    ) -> pantheon_api::model::ModelPolicy {
        let mut policy = build_model_policy(cfg, provider.clone(), model.clone());
        if provider.is_none() && model.is_none() {
            if let Some(pin) = cfg
                .and_then(|c| c.routing.as_ref())
                .and_then(|r| task_type.and_then(|t| r.resolve(t)))
            {
                policy.default = pantheon_api::model::DefaultModel {
                    provider: pin.provider.clone(),
                    model: pin.model.clone(),
                    key_env: None,
                };
            }
        }
        policy
    }

    /// Effective serve port for a `[server]` section: the configured port,
    /// except 0 (the documented "auto") and absence both resolve to the
    /// 18789 default. A literal 0 used to bind an ephemeral port nobody
    /// reported - remotes refused it (`port 0 => None`), GenUI blob URLs
    /// baked in `:0`, and nothing printed the bound port - so "auto" now
    /// means "the default port", addressable by everything downstream.
    pub fn server_port(server: Option<&ServerSection>) -> u16 {
        match server.map(|s| s.port).unwrap_or(0) {
            0 => 18789,
            p => p,
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
        // Canonical precedence, delegated to the single resolver in
        // pantheon-api (`config_resolve`): explicit argument > environment
        // variable > `[model]` section > hardcoded local default, with
        // empty/whitespace values ignored at every level. This used to do
        // explicit > config > env with raw `.or()` chains, so an empty
        // config value (e.g. a `[model]` table created by `fallback add`
        // with no default) shadowed the environment and the local default.
        // The fallback chain is resolved by the same single resolver, so
        // the TUI, the dashboard, and the runtime can no longer disagree.
        let default = pantheon_api::config_resolve::resolve_default_model(cfg, provider, model);
        let chain = pantheon_api::config_resolve::resolve_fallback_chain(cfg);
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
                    key_env: None,
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
                let auth = match pantheon_mcp::McpAuth::parse(e.auth.as_deref()) {
                    Some(a) => a,
                    None => {
                        // Unknown auth must not silently downgrade to None:
                        // a typo like auth="oauth2" would otherwise run
                        // headerless instead of erroring. Skip like an
                        // unknown transport; `collect` surfaces the skip.
                        eprintln!(
                            "mcp: server '{name}': unknown auth {:?}, skipped",
                            e.auth.as_deref().unwrap_or("")
                        );
                        continue;
                    }
                };
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
                    auth,
                    scopes: e.scopes.clone(),
                    client_id: e.client_id.clone(),
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
                // Honour readiness blockers that cannot be satisfied: a
                // declaration whose credential was dropped at import with
                // no env indirection to supply it (e.g. a literal header)
                // must not be configured headerless - it would fail 401
                // with no warning. When requires_env names exist, the
                // env resolver may still supply them, so those configure
                // as before and `collect` shows their blocker.
                if d.needs_credentials && d.requires_env.is_empty() {
                    eprintln!(
                        "mcp: declared server '{}': needs a credential (value not in the source), skipped",
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
                    auth: pantheon_mcp::McpAuth::None,
                    scopes: Vec::new(),
                    client_id: None,
                });
            }
        }

        cfg.servers = specs;
        cfg
    }

    #[cfg(test)]
    mod build_model_policy_tests {
        use super::*;

        // These tests mutate process-global env; serialize them.
        static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn cfg_with_model(provider: &str, model: &str) -> Config {
            Config {
                model: Some(ModelSection {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        fn clear_env() {
            std::env::remove_var("PANTHEON_PROVIDER");
            std::env::remove_var("PANTHEON_MODEL");
        }

        #[test]
        fn canonical_precedence_env_beats_config_empty_env_does_not_shadow() {
            let _g = ENV_GUARD.lock().expect("env guard");
            clear_env();
            let cfg = cfg_with_model("cfg-provider", "cfg-model");

            // Config beats default.
            let policy = build_model_policy(Some(&cfg), None, None);
            assert_eq!(policy.default.provider, "cfg-provider");
            assert_eq!(policy.default.model, "cfg-model");

            // Env beats config (the pre-migration order was config > env).
            std::env::set_var("PANTHEON_PROVIDER", "env-provider");
            std::env::set_var("PANTHEON_MODEL", "env-model");
            let policy = build_model_policy(Some(&cfg), None, None);
            assert_eq!(policy.default.provider, "env-provider");
            assert_eq!(policy.default.model, "env-model");

            // Explicit beats env.
            let policy = build_model_policy(
                Some(&cfg),
                Some("flag-provider".into()),
                Some("flag-model".into()),
            );
            assert_eq!(policy.default.provider, "flag-provider");
            assert_eq!(policy.default.model, "flag-model");

            // Empty env reads as unset: config still wins.
            std::env::set_var("PANTHEON_PROVIDER", "");
            std::env::remove_var("PANTHEON_MODEL");
            let policy = build_model_policy(Some(&cfg), None, None);
            assert_eq!(policy.default.provider, "cfg-provider");
            assert_eq!(policy.default.model, "cfg-model");
            clear_env();
        }
    }

    #[cfg(test)]
    mod apply_session_config_tests {
        use super::*;

        #[test]
        fn apply_session_config_threads_budget_tiers() {
            // The regression this pins: headless turn paths built a Session
            // and never applied the config, so a [budget] cap the TUI
            // honored was silently ignored by `run --deliver`, swarm
            // children, and pipeline sessions.
            let dir = std::env::temp_dir().join(format!(
                "pantheon-apply-session-config-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let cfg = Config {
                budget: Some(BudgetSection {
                    max_turns: Some(3),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let session = pantheon_runtime::session::Session::new(
                dir.clone(),
                pantheon_api::capability::Policy::coder(),
                build_model_policy(Some(&cfg), None, None),
                chat_secrets(None),
            )
            .expect("session must open on a scratch dir");
            assert_eq!(
                session.budget_snapshot().max_turns,
                16,
                "precondition: a fresh session carries the runtime default"
            );
            apply_session_config(&session, Some(&cfg));
            assert_eq!(
                session.budget_snapshot().max_turns,
                3,
                "the [budget] tier must reach the session before its first turn"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod config_schema {
    //! Pantheon config schema (`config.toml` in the data dir).
    //!
    //! The schema primitives ([`PolicyPreset`], and [`SecretRef`](pantheon_api::config_schema::SecretRef))
    //! live in `pantheon_api::config_schema` so non-TUI crates (dashboard,
    //! doctor) can validate against them. This module re-exports the preset
    //! and keeps the TUI-local resolver that needs [`crate::config::Config`].

    pub use pantheon_api::config_schema::PolicyPreset;

    /// Resolve the policy for a session.
    ///
    /// Config wins. With no config, `PANTHEON_ALLOW_MEMORY` only decides between
    /// the two coder policies, and `PANTHEON_POLICY` picks the preset outright.
    pub fn policy_for_config(
        file_cfg: &Option<crate::config::Config>,
    ) -> pantheon_api::capability::Policy {
        if let Some(preset) = file_cfg.as_ref().and_then(|c| c.policy) {
            return preset.to_policy();
        }
        if let Ok(p) = std::env::var("PANTHEON_POLICY") {
            if let Some(preset) = PolicyPreset::parse(&p) {
                return preset.to_policy();
            }
            // Unknown values used to fall through silently to the coder
            // default, so a typo (`PANTHEON_POLICY=coder_memroy`) looked
            // like it worked while doing nothing. Warn loudly instead.
            eprintln!(
                "[pantheon] unknown PANTHEON_POLICY value '{p}' (expected reader, coder, or coder_memory); falling back to the coder default"
            );
        }
        let allow_memory = std::env::var("PANTHEON_ALLOW_MEMORY")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);
        if allow_memory {
            pantheon_api::capability::Policy::coder_with_memory()
        } else {
            pantheon_api::capability::Policy::coder()
        }
    }
}

pub mod config_verb {
    //! `pantheon config` verbs: read and write `config.toml` from the terminal.
    //!
    //! - `pantheon config set <dotted.key> <value>` writes one value, creating
    //!   intermediate tables as needed. Edits are format-preserving: comments and
    //!   layout elsewhere in the file are left alone.
    //! - `pantheon config get <dotted.key>` prints one value.
    //! - `pantheon config edit` opens the file in `$VISUAL`/`$EDITOR` (falling
    //!   back to `vi`, then `nano`), the way you would edit any file by hand.
    //! - `pantheon config path` prints the file location.
    //!
    //! Values are type-inferred: `true`/`false` become booleans, `100` an
    //! integer, `1.5` a float, anything else a string. Surround with quotes to
    //! force a string (`'"100"'` stays the text "100"). Arrays and tables cannot
    //! be set from the command line; the error tells you to use `config edit`.
    //!
    //! Every key is validated against the real [`pantheon_api::config::Config`]
    //! schema before anything is written: a typo like `goal.max_iterationz`
    //! fails loudly instead of persisting a dead key. Writes are atomic
    //! (temp file + fsync + rename), and `config edit` validates the edited
    //! text before replacing the file, keeping a `config.toml.bak` backup.

    use std::path::{Path, PathBuf};
    use std::process::Command;
    use toml_edit::{DocumentMut, Item, Value};

    pub fn config_path() -> PathBuf {
        crate::terminal::data_dir().join("config.toml")
    }

    fn read_doc_at(path: &Path) -> Result<DocumentMut, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => text
                .parse::<DocumentMut>()
                .map_err(|e| format!("config: cannot parse {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
            Err(e) => Err(format!("config: cannot read {}: {e}", path.display())),
        }
    }

    /// Unique temp path in the same directory as `path`, so the eventual rename
    /// is atomic. Includes pid + nanos so concurrent writers never collide.
    fn tmp_path_for(path: &Path, tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let file_name = format!(
            "{}.{tag}-{}-{}.tmp",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "config".to_string()),
            std::process::id(),
            nanos
        );
        path.with_file_name(file_name)
    }

    /// fsync `src`, then atomically rename it over `dst`, then best-effort
    /// fsync the parent directory so the rename survives a crash.
    fn fsync_and_rename(src: &Path, dst: &Path) -> Result<(), String> {
        std::fs::File::open(src)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("config: cannot fsync {}: {e}", src.display()))?;
        std::fs::rename(src, dst)
            .map_err(|e| format!("config: cannot replace {}: {e}", dst.display()))?;
        if let Some(parent) = dst.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }

    /// Atomically replace `path` with `doc`: write to a temp file in the same
    /// directory, fsync, rename. A crash mid-write leaves either the old file
    /// or the new one, never a torn file. Temp leftovers are cleaned up on
    /// every error path.
    fn write_doc_atomic_at(path: &Path, doc: &DocumentMut) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("config: cannot create {}: {e}", parent.display()))?;
        }
        let tmp = tmp_path_for(path, "write");
        let failed = |e: String| {
            let _ = std::fs::remove_file(&tmp);
            e
        };
        if let Err(e) = std::fs::write(&tmp, doc.to_string()) {
            return Err(failed(format!(
                "config: cannot write {}: {e}",
                tmp.display()
            )));
        }
        fsync_and_rename(&tmp, path).map_err(failed)
    }

    /// Split `a.b.c` into key segments, rejecting empty or malformed segments.
    fn split_key(key: &str) -> Result<Vec<&str>, String> {
        if key.is_empty() {
            return Err("config: empty key".to_string());
        }
        let parts: Vec<&str> = key.split('.').collect();
        for p in &parts {
            if p.is_empty() {
                return Err(format!("config: bad key {key:?}: empty segment"));
            }
            if !p
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            {
                return Err(format!(
                    "config: bad key {key:?}: segment {p:?} is not a bare key"
                ));
            }
        }
        Ok(parts)
    }

    /// `key=value` single-argument form, or `None` when there is no `=`.
    fn split_key_eq(arg: &str) -> Option<(&str, &str)> {
        let (k, v) = arg.split_once('=')?;
        if k.is_empty() {
            return None;
        }
        Some((k, v))
    }

    /// Infer a TOML value from shell text. Quoted text is always a string.
    /// Arrays and tables are rejected: `config set` handles scalars only.
    fn infer_value(raw: &str) -> Result<Value, String> {
        let t = raw.trim();
        if t.len() >= 2
            && ((t.starts_with('"') && t.ends_with('"'))
                || (t.starts_with('\'') && t.ends_with('\'')))
        {
            return Ok(Value::from(&t[1..t.len() - 1]));
        }
        if t.starts_with('[') || t.starts_with('{') {
            return Err(
                "config: arrays and tables cannot be set with `config set`; use `pantheon config edit` instead"
                    .to_string(),
            );
        }
        match t {
            "true" => return Ok(Value::from(true)),
            "false" => return Ok(Value::from(false)),
            _ => {}
        }
        if let Ok(n) = t.parse::<i64>() {
            return Ok(Value::from(n));
        }
        if let Ok(n) = t.parse::<f64>() {
            return Ok(Value::from(n));
        }
        Ok(Value::from(t))
    }

    /// Check that `key` names a real field of the [`pantheon_api::config::Config`]
    /// schema, without hardcoding the field list.
    ///
    /// The edited document is round-tripped through the `Config` type: TOML
    /// text -> `toml::Value` -> `Config` -> `toml::Value`. Serde silently drops
    /// unknown keys on deserialize, so if a path segment present in the input
    /// is missing from the output, that segment is not a real field. Type
    /// mismatches (e.g. a string where the schema wants an integer) fail the
    /// deserialization itself, so they fail loudly too.
    fn validate_key_against_schema(
        doc: &DocumentMut,
        key: &str,
        parts: &[&str],
    ) -> Result<(), String> {
        let input: toml::Value = toml::from_str(&doc.to_string())
            .map_err(|e| format!("config: cannot parse edited config as TOML: {e}"))?;
        let cfg: pantheon_api::config::Config = input
            .clone()
            .try_into()
            .map_err(|e| format!("config: cannot set {key:?}: {e}"))?;
        let output = toml::Value::try_from(&cfg)
            .map_err(|e| format!("config: internal error re-serializing config: {e}"))?;
        let mut in_node = &input;
        let mut out_node = &output;
        for (i, part) in parts.iter().enumerate() {
            match (in_node.get(part), out_node.get(part)) {
                (Some(next_in), Some(next_out)) => {
                    in_node = next_in;
                    out_node = next_out;
                }
                (Some(_), None) => {
                    let parent = parts[..i].join(".");
                    let location = if parent.is_empty() {
                        "the top level of the config".to_string()
                    } else {
                        format!("{parent:?}")
                    };
                    return Err(format!(
                        "config: unknown key {key:?}: segment {part:?} is not a field of {location}"
                    ));
                }
                (None, _) => {
                    return Err(format!("config: internal error validating {key:?}"));
                }
            }
        }
        Ok(())
    }

    /// Parse + deserialize + validate candidate config text. Used by `edit`
    /// before the edited file is allowed to replace the real one.
    fn validate_config_text(text: &str) -> Result<pantheon_api::config::Config, String> {
        let cfg: pantheon_api::config::Config = toml::from_str(text)
            .map_err(|e| format!("config: edited file is not valid config: {e}"))?;
        let problems = cfg.validate();
        if !problems.is_empty() {
            return Err(format!(
                "config: edited file failed validation:\n  {}",
                problems.join("\n  ")
            ));
        }
        Ok(cfg)
    }

    fn set_dotted(doc: &mut DocumentMut, key: &str, value: Value) -> Result<(), String> {
        let parts = split_key(key)?;
        let mut table = doc.as_table_mut();
        for part in &parts[..parts.len() - 1] {
            let entry = table
                .entry(part)
                .or_insert_with(|| Item::Table(toml_edit::Table::new()));
            match entry {
                Item::Table(t) => table = t,
                other => {
                    return Err(format!(
                        "config: cannot set {key:?}: {part:?} is already a {}",
                        item_kind(other)
                    ));
                }
            }
        }
        table.insert(parts[parts.len() - 1], Item::Value(value));
        Ok(())
    }

    fn get_dotted<'d>(doc: &'d DocumentMut, key: &str) -> Result<Option<&'d Item>, String> {
        let parts = split_key(key)?;
        let mut item: &Item = doc.as_item();
        for part in &parts {
            item = match item {
                Item::Table(t) => t.get(part),
                _ => None,
            }
            .ok_or_else(|| format!("config: {key:?} is not set"))?;
        }
        Ok(Some(item))
    }

    fn item_kind(item: &Item) -> &'static str {
        match item {
            Item::Value(_) => "value",
            Item::Table(_) => "table",
            Item::ArrayOfTables(_) => "array of tables",
            Item::None => "nothing",
        }
    }

    /// Print a value the way a person would read it: strings bare, scalars
    /// without their TOML decor whitespace, everything else as TOML.
    fn display_value(item: &Item) -> String {
        match item {
            Item::Value(Value::String(s)) => s.value().to_string(),
            Item::Value(Value::Integer(i)) => i.value().to_string(),
            Item::Value(Value::Float(f)) => f.value().to_string(),
            Item::Value(Value::Boolean(b)) => b.value().to_string(),
            Item::Value(Value::Datetime(d)) => d.value().to_string(),
            Item::Value(Value::Array(a)) => a.to_string(),
            Item::Value(Value::InlineTable(t)) => t.to_string(),
            Item::Table(t) => t.to_string(),
            Item::ArrayOfTables(a) => a.to_string(),
            Item::None => String::new(),
        }
    }

    /// Set one key in the config file at `path`. Pure core of `config set`:
    /// no process exits, so tests can drive it against a temp dir.
    /// Does a [`Config::validate`] problem name exactly the key being set?
    /// Problems are sentences starting with the dotted path (`model.reasoning
    /// ... is not ...`, `model.api_key_env: ...`), so a prefix match on the
    /// key plus a separator is precise. Only the set key gates the write:
    /// a full-document rejection would block e.g. `config set
    /// goal.max_iterations` on every config that lacks a `[model]` section.
    fn problem_names_key(problem: &str, key: &str) -> bool {
        problem == key
            || problem.starts_with(&format!("{key} "))
            || problem.starts_with(&format!("{key}:"))
            || problem.starts_with(&format!("{key}."))
    }

    fn set_config_value(path: &Path, key: &str, raw: &str) -> Result<String, String> {
        let value = infer_value(raw)?;
        let mut doc = read_doc_at(path)?;
        let parts = split_key(key)?;
        set_dotted(&mut doc, key, value.clone())?;
        validate_key_against_schema(&doc, key, &parts)?;
        // `config set` used to run no `Config::validate()` at all, so a value
        // the schema accepts but the document rejects (bad reasoning level,
        // empty provider, zero aux timeout) was written and only surfaced
        // later in doctor. Reject when validation implicates the key itself.
        let cfg: pantheon_api::config::Config = toml::from_str(&doc.to_string())
            .map_err(|e| format!("config: cannot parse edited config as TOML: {e}"))?;
        let problems: Vec<String> = cfg
            .validate()
            .into_iter()
            .filter(|p| problem_names_key(p, key))
            .collect();
        if !problems.is_empty() {
            return Err(format!(
                "config: cannot set {key:?}: {}",
                problems.join("; ")
            ));
        }
        write_doc_atomic_at(path, &doc)?;
        let shown = match &value {
            Value::String(s) => format!("{:?}", s.value()),
            v => v.to_string(),
        };
        Ok(format!("set {key} = {shown} ({})", path.display()))
    }

    /// Read one key from the config file at `path`.
    fn get_config_value(path: &Path, key: &str) -> Result<String, String> {
        let doc = read_doc_at(path)?;
        match get_dotted(&doc, key) {
            Ok(Some(item)) => Ok(display_value(item)),
            Ok(None) => Err(format!("config: {key:?} is not set")),
            Err(e) => Err(e),
        }
    }

    fn cmd_set(rest: &[String]) {
        // Extra arguments used to be silently ignored (`[k, v, ..]`), so a
        // typo'd extra word vanished instead of failing the set.
        let (key, raw) = match rest {
            [k, v] => (k.as_str(), v.as_str()),
            [k, v, extra, ..] => {
                eprintln!(
                    "config set takes exactly one key and one value; unexpected extra argument {extra:?} after {k:?} {v:?}"
                );
                std::process::exit(2);
            }
            [kv] => match split_key_eq(kv) {
                Some((k, v)) => (k, v),
                None => {
                    eprintln!("usage: pantheon config set <dotted.key> <value>");
                    std::process::exit(2);
                }
            },
            [] => {
                eprintln!("usage: pantheon config set <dotted.key> <value>");
                std::process::exit(2);
            }
        };
        match set_config_value(&config_path(), key, raw) {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    fn cmd_get(rest: &[String]) {
        let key = rest.first().map(String::as_str).unwrap_or("");
        if key.is_empty() {
            eprintln!("usage: pantheon config get <dotted.key>");
            std::process::exit(2);
        }
        match get_config_value(&config_path(), key) {
            Ok(v) => println!("{v}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    /// The editor cascade: `$VISUAL`, then `$EDITOR`, then `vi`, then `nano`.
    /// The first binary found on PATH wins; a spawn failure moves to the next.
    fn pick_editor() -> Option<String> {
        for var in ["PANTHEON_EDITOR", "VISUAL", "EDITOR"] {
            if let Ok(e) = std::env::var(var) {
                let e = e.trim().to_string();
                if !e.is_empty() {
                    return Some(e);
                }
            }
        }
        None
    }

    fn editor_exists(editor: &str) -> bool {
        let prog = editor.split_whitespace().next().unwrap_or(editor);
        // Absolute path: check directly. Otherwise search PATH.
        if prog.contains('/') {
            return std::path::Path::new(prog).is_file();
        }
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|d| d.join(prog).is_file()))
            .unwrap_or(false)
    }

    /// Launch `editor` (a `code --wait`-style command line) on `file`.
    /// Returns an error when the editor cannot be launched or exits nonzero.
    fn launch_editor(editor: &str, file: &Path) -> Result<(), String> {
        let mut parts = editor.split_whitespace();
        let prog = parts.next().unwrap_or(editor);
        let status = Command::new(prog)
            .args(parts)
            .arg(file)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .map_err(|e| format!("config: could not launch {editor:?}: {e}"))?;
        if !status.success() {
            return Err(format!("config: editor exited with status {status}"));
        }
        Ok(())
    }

    /// The testable core of `config edit`: the editor works on a staging file,
    /// never the live config. After the editor exits, the staged text is
    /// validated; only then does it replace the real file (atomically), with
    /// the pre-edit contents kept as `config.toml.bak`. A failed validation
    /// deletes the stage and leaves the original untouched.
    fn run_edit_flow(path: &Path, editor: &str) -> Result<String, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("config: cannot create {}: {e}", parent.display()))?;
        }
        let stage = tmp_path_for(path, "edit");
        if path.exists() {
            std::fs::copy(path, &stage)
                .map_err(|e| format!("config: cannot stage {}: {e}", path.display()))?;
        } else {
            std::fs::write(&stage, "")
                .map_err(|e| format!("config: cannot stage {}: {e}", path.display()))?;
        }
        let cleanup_stage = |e: String| {
            let _ = std::fs::remove_file(&stage);
            e
        };
        if let Err(e) = launch_editor(editor, &stage) {
            return Err(cleanup_stage(e));
        }
        let text = std::fs::read_to_string(&stage)
            .map_err(|e| cleanup_stage(format!("config: cannot read edited file: {e}")))?;
        if let Err(e) = validate_config_text(&text) {
            return Err(cleanup_stage(format!(
                "{e}\nconfig: original file left unchanged"
            )));
        }
        let backup = path.with_extension("toml.bak");
        if path.exists() {
            std::fs::copy(path, &backup).map_err(|e| {
                cleanup_stage(format!("config: cannot back up {}: {e}", path.display()))
            })?;
        }
        fsync_and_rename(&stage, path).map_err(cleanup_stage)?;
        Ok(format!(
            "saved {} (backup: {})",
            path.display(),
            backup.display()
        ))
    }

    fn cmd_edit() {
        let path = config_path();
        let mut candidates: Vec<String> = Vec::new();
        if let Some(e) = pick_editor() {
            candidates.push(e);
        }
        candidates.push("vi".to_string());
        candidates.push("nano".to_string());

        for editor in &candidates {
            if !editor_exists(editor) {
                continue;
            }
            match run_edit_flow(&path, editor) {
                Ok(msg) => {
                    println!("{msg}");
                    return;
                }
                Err(e) if e.starts_with("config: could not launch") => continue,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        eprintln!("config: no editor found (set $EDITOR, or install vi/nano)");
        std::process::exit(1);
    }

    /// Print the config usage text. `to_stdout` selects the stream:
    /// `--help` is not an error, so its text goes to stdout; the usage-error
    /// branch still goes to stderr (see [`cmd_config`]).
    fn config_help(to_stdout: bool) {
        // One body, two streams: duplicating the lines for stdout/stderr
        // would let them drift apart.
        let lines = [
            "usage: pantheon config <set|get|edit|path> ...",
            "  set <dotted.key> <value>   set one value (creates tables as needed)",
            "  set <dotted.key>=<value>   single-argument form",
            "  get <dotted.key>           print one value",
            "  edit                       open config.toml in your editor",
            "  path                       print the config file path",
            "",
            "values: true/false -> boolean, 100 -> integer, 1.5 -> float,",
            "anything else -> string. Quote to force a string: '\"100\"'.",
            "arrays and tables need `pantheon config edit`.",
            "",
            "keys are checked against the real config schema: unknown",
            "keys are rejected instead of silently written.",
        ];
        for line in lines {
            if to_stdout {
                println!("{line}");
            } else {
                eprintln!("{line}");
            }
        }
    }

    /// Entry point called from the verb dispatch in `terminal::run`.
    pub fn cmd_config(args: &[String]) {
        match args.get(2).map(String::as_str) {
            Some("set") => cmd_set(&args[3..]),
            Some("get") => cmd_get(&args[3..]),
            Some("edit") => cmd_edit(),
            Some("path") => println!("{}", config_path().display()),
            // `--help` used to fall through to the usage-error branch (exit
            // 2); help is not an error. It also used to print to stderr;
            // help text goes to stdout so `pantheon config --help | head`
            // and friends work.
            Some("--help") | Some("-h") => {
                config_help(true);
                std::process::exit(0);
            }
            _ => {
                config_help(false);
                std::process::exit(2);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Scratch dir for one test. Unique per call so tests stay parallel-safe.
        fn scratch_dir(tag: &str) -> PathBuf {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "pantheon-config-verb-{}-{}-{}",
                tag,
                std::process::id(),
                nanos
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn scratch_config(tag: &str) -> PathBuf {
            scratch_dir(tag).join("config.toml")
        }

        #[test]
        fn infer_value_types() {
            assert!(matches!(infer_value("true"), Ok(Value::Boolean(_))));
            assert!(matches!(infer_value("false"), Ok(Value::Boolean(_))));
            assert!(matches!(infer_value("100"), Ok(Value::Integer(_))));
            assert!(matches!(infer_value("-7"), Ok(Value::Integer(_))));
            assert!(matches!(infer_value("1.5"), Ok(Value::Float(_))));
            assert!(matches!(infer_value("openai/gpt-4o"), Ok(Value::String(_))));
            // Quoted numbers stay strings.
            match infer_value("\"100\"") {
                Ok(Value::String(s)) => assert_eq!(s.value(), "100"),
                v => panic!("expected string, got {v:?}"),
            }
            match infer_value("'true'") {
                Ok(Value::String(s)) => assert_eq!(s.value(), "true"),
                v => panic!("expected string, got {v:?}"),
            }
        }

        #[test]
        fn infer_value_rejects_structured_values() {
            for raw in ["[1, 2]", "[]", "{a = 1}", "[[x]]", "[unclosed"] {
                let err = infer_value(raw).unwrap_err();
                assert!(
                    err.contains("config edit"),
                    "expected edit direction for {raw:?}, got: {err}"
                );
            }
            // ...but quoted structured text is a legitimate string.
            match infer_value("\"[1, 2]\"") {
                Ok(Value::String(s)) => assert_eq!(s.value(), "[1, 2]"),
                v => panic!("expected string, got {v:?}"),
            }
        }

        #[test]
        fn split_key_rejects_bad_segments() {
            assert!(split_key("budget.max_turns").is_ok());
            assert!(split_key("a-b_c9").is_ok());
            assert!(split_key("").is_err());
            assert!(split_key("a..b").is_err());
            assert!(split_key(".a").is_err());
            assert!(split_key("a b").is_err());
        }

        #[test]
        fn set_creates_nested_tables() {
            let mut doc = DocumentMut::new();
            set_dotted(&mut doc, "budget.max_turns", Value::from(100)).unwrap();
            let item = get_dotted(&doc, "budget.max_turns").unwrap().unwrap();
            assert_eq!(display_value(item), "100");
        }

        #[test]
        fn set_preserves_comments_and_layout() {
            let mut doc = "# keep me\n[budget]\nmax_turns = 50\n"
                .parse::<DocumentMut>()
                .unwrap();
            set_dotted(&mut doc, "budget.max_turns", Value::from(100)).unwrap();
            set_dotted(&mut doc, "goal.max_iterations", Value::from(12)).unwrap();
            let out = doc.to_string();
            assert!(out.contains("# keep me"), "comment lost: {out}");
            assert!(out.contains("max_turns = 100"), "value not set: {out}");
            assert!(out.contains("[goal]"), "new table missing: {out}");
        }

        #[test]
        fn set_fails_when_intermediate_is_a_value() {
            let mut doc = "budget = 5\n".parse::<DocumentMut>().unwrap();
            let err = set_dotted(&mut doc, "budget.max_turns", Value::from(1)).unwrap_err();
            assert!(err.contains("already a value"), "unexpected: {err}");
        }

        #[test]
        fn get_missing_key_errors() {
            let doc = DocumentMut::new();
            let err = get_dotted(&doc, "nope.nothing").unwrap_err();
            assert!(err.contains("not set"), "unexpected: {err}");
        }

        #[test]
        fn key_eq_form_splits() {
            assert_eq!(split_key_eq("a.b=100"), Some(("a.b", "100")));
            assert_eq!(split_key_eq("novalue"), None);
            assert_eq!(split_key_eq("=x"), None);
        }

        #[test]
        fn set_rejects_unknown_top_level_key() {
            let path = scratch_config("unknown-top");
            let err = set_config_value(&path, "frobnicator.max_turns", "5").unwrap_err();
            assert!(err.contains("unknown key"), "unexpected: {err}");
            assert!(
                err.contains("\"frobnicator\""),
                "bad segment not named: {err}"
            );
            // Nothing was written: a rejected set leaves no file behind.
            assert!(!path.exists(), "rejected set wrote a file");
        }

        #[test]
        fn set_rejects_typo_in_leaf_segment() {
            let path = scratch_config("unknown-leaf");
            let err = set_config_value(&path, "goal.max_iterationz", "5").unwrap_err();
            assert!(err.contains("unknown key"), "unexpected: {err}");
            assert!(err.contains("\"max_iterationz\""), "typo not named: {err}");
            assert!(!path.exists(), "rejected set wrote a file");
        }

        #[test]
        fn set_accepts_real_keys_and_round_trips() {
            let path = scratch_config("real-keys");
            set_config_value(&path, "goal.max_iterations", "25").unwrap();
            assert_eq!(
                get_config_value(&path, "goal.max_iterations").unwrap(),
                "25"
            );
            // A second, unrelated real key in another section.
            set_config_value(&path, "budget.max_turns", "100").unwrap();
            assert_eq!(get_config_value(&path, "budget.max_turns").unwrap(), "100");
            // Profile-scoped keys (map entries) validate too.
            set_config_value(&path, "agents.coder.model", "openai/gpt-4o").unwrap();
            assert_eq!(
                get_config_value(&path, "agents.coder.model").unwrap(),
                "openai/gpt-4o"
            );
        }

        #[test]
        fn set_model_provider_on_fresh_config_works() {
            // Used to fail with "missing field 'model'": the `[model]`
            // table was created but its sibling field was absent, so the
            // schema round-trip died on a field the user never touched.
            let path = scratch_config("model-fresh");
            set_config_value(&path, "model.provider", "openai").unwrap();
            assert_eq!(get_config_value(&path, "model.provider").unwrap(), "openai");
            // The sibling stays unset (empty), not invented.
            let cfg: pantheon_api::config::Config =
                toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(cfg.model.as_ref().unwrap().model, "");
        }

        #[test]
        fn set_rejects_value_that_fails_document_validation() {
            let path = scratch_config("model-reasoning");
            set_config_value(&path, "model.provider", "openai").unwrap();
            set_config_value(&path, "model.model", "gpt-4o").unwrap();
            let err = set_config_value(&path, "model.reasoning", "bogus-level").unwrap_err();
            assert!(err.contains("model.reasoning"), "unexpected: {err}");
            assert!(!std::fs::read_to_string(&path)
                .unwrap()
                .contains("bogus-level"));
        }

        #[test]
        fn set_rejects_type_mismatch() {
            let path = scratch_config("type-mismatch");
            // goal.max_iterations is a u32; a bare string must fail loudly.
            let err = set_config_value(&path, "goal.max_iterations", "many").unwrap_err();
            assert!(!path.exists(), "type-mismatched set wrote a file");
            assert!(err.contains("goal.max_iterations"), "key not named: {err}");
        }

        #[test]
        fn set_rejects_structured_value_with_edit_direction() {
            let path = scratch_config("structured");
            let err = set_config_value(&path, "goal.max_iterations", "[1, 2]").unwrap_err();
            assert!(err.contains("config edit"), "unexpected: {err}");
            assert!(!path.exists(), "rejected set wrote a file");
        }

        #[test]
        fn atomic_write_leaves_no_torn_file_or_temp() {
            let path = scratch_config("atomic");
            let parent = path.parent().unwrap();
            set_config_value(&path, "goal.max_iterations", "42").unwrap();
            // The written file parses as clean TOML with the value in place.
            let text = std::fs::read_to_string(&path).unwrap();
            let parsed: toml::Value = toml::from_str(&text).expect("torn config file");
            assert_eq!(parsed["goal"]["max_iterations"].as_integer(), Some(42));
            // No temp leftovers in the directory.
            let leftovers: Vec<_> = std::fs::read_dir(parent)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".tmp"))
                .collect();
            assert!(
                leftovers.is_empty(),
                "temp files left behind: {leftovers:?}"
            );
        }

        #[test]
        fn atomic_write_cleans_up_on_error() {
            // Pointing the "file" at a directory makes every write fail.
            let dir = scratch_dir("atomic-err");
            let err = write_doc_atomic_at(&dir, &DocumentMut::new()).unwrap_err();
            assert!(!err.is_empty());
            let leftovers: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".tmp"))
                .collect();
            assert!(
                leftovers.is_empty(),
                "temp files left behind: {leftovers:?}"
            );
        }

        #[test]
        fn validate_config_text_accepts_good_and_rejects_bad() {
            // Note: Config::validate requires a [model] section.
            let good =
                "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n[goal]\nmax_iterations = 25\n";
            assert!(validate_config_text(good).is_ok());
            assert!(validate_config_text("").is_err()); // no [model]
                                                        // TOML type error.
            let err = validate_config_text("[goal]\nmax_iterations = \"many\"\n").unwrap_err();
            assert!(err.contains("not valid config"), "unexpected: {err}");
            // Syntactically invalid TOML.
            let err = validate_config_text("[[[").unwrap_err();
            assert!(err.contains("not valid config"), "unexpected: {err}");
        }

        /// Fake editor: a shell script that overwrites its argument with `body`.
        fn fake_editor(tag: &str, body: &str) -> PathBuf {
            let dir = scratch_dir(tag);
            let script = dir.join("fake-editor.sh");
            std::fs::write(
                &script,
                format!("#!/bin/sh\ncat > \"$1\" <<'EOF'\n{body}\nEOF\n"),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&script).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&script, perms).unwrap();
            }
            script
        }

        #[test]
        fn edit_rejects_invalid_text_and_keeps_original() {
            let path = scratch_config("edit-reject");
            std::fs::write(&path, "[goal]\nmax_iterations = 10\n").unwrap();
            let editor = fake_editor("edit-reject", "[goal]\nmax_iterations = \"many\"\n");
            let err = run_edit_flow(&path, editor.to_str().unwrap()).unwrap_err();
            assert!(err.contains("not valid config"), "unexpected: {err}");
            assert!(err.contains("left unchanged"), "unexpected: {err}");
            // Original untouched, no backup created, no stage left behind.
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "[goal]\nmax_iterations = 10\n"
            );
            assert!(!path.with_extension("toml.bak").exists());
            let parent = path.parent().unwrap();
            let leftovers: Vec<_> = std::fs::read_dir(parent)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".tmp"))
                .collect();
            assert!(leftovers.is_empty(), "stage left behind: {leftovers:?}");
        }

        #[test]
        fn edit_accepts_valid_text_and_keeps_backup() {
            let path = scratch_config("edit-accept");
            let before =
                "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n[goal]\nmax_iterations = 10\n";
            std::fs::write(&path, before).unwrap();
            let after =
                "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n[goal]\nmax_iterations = 30\n";
            let editor = fake_editor("edit-accept", after);
            let msg = run_edit_flow(&path, editor.to_str().unwrap()).unwrap();
            assert!(msg.contains("saved"), "unexpected: {msg}");
            // (The fake editor's heredoc appends one trailing newline.)
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                format!("{after}\n")
            );
            // Pre-edit contents kept next to the file.
            assert_eq!(
                std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
                before
            );
        }

        #[test]
        fn edit_validation_failure_reports_schema_problems() {
            // A config that parses and deserializes but fails Config::validate.
            let err =
                validate_config_text("[model]\nprovider = \"\"\nmodel = \"x\"\n").unwrap_err();
            assert!(err.contains("failed validation"), "unexpected: {err}");
        }
    }
}

pub mod migrate {
    //! `pantheon migrate`: the section 23 pipeline, exposed.
    //!
    //!   detect    - which sources are on this machine, and where
    //!   plan      - dry run: what would import, what archives, what is a secret
    //!   apply     - backup, then import, then validate (the only writer)
    //!   validate  - re-check the current targets without writing
    //!
    //! The default for every verb is read-only. `apply` is the single verb that
    //! touches the data dir, and it refuses to run without either an explicit
    //! `--yes` or a typed confirmation, because it overwrites existing targets.
    //!
    //! Secrets are never a candidate for import. A `.env`, `auth.json`,
    //! `models.yml`, `agent.db`, `broker.token`, or any `*.pem` / `*.key` is
    //! reported and skipped - archiving a credential would turn this verb into
    //! an exfil path, so it does not.

    use crate::terminal::data_dir;
    use pantheon_migration::{
        analyze, apply_with, backup, detect, ensure_sessions_indexed, plan, reconcile_keys, render,
        validate, Action, ItemKind, KeyMatch, MigrationCategory, MigrationFilter, MigrationPlan,
        SourceKind, Targets,
    };
    use std::path::{Path, PathBuf};

    /// Where each source lives by default, in `detect` order.
    fn default_roots() -> Vec<(SourceKind, PathBuf)> {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        vec![
            (SourceKind::Hermes, home.join(".hermes")),
            (SourceKind::OpenClaw, home.join(".openclaw")),
            (SourceKind::Omp, home.join(".omp")),
            (SourceKind::ClaudeCode, home.join(".claude")),
        ]
    }

    fn ext_dir() -> PathBuf {
        std::env::var("PANTHEON_EXT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir().join("extensions"))
    }

    fn targets() -> Targets {
        Targets::new(data_dir(), ext_dir())
    }

    fn flag_value(args: &[String], flag: &str) -> Option<String> {
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == flag {
                return it.next().cloned();
            }
        }
        None
    }

    fn has_flag(args: &[String], flag: &str) -> bool {
        args.iter().any(|a| a == flag)
    }

    /// Resolve the source root: an explicit `--path`, else the conventional home.
    fn resolve_root(kind: SourceKind, args: &[String]) -> PathBuf {
        if let Some(p) = flag_value(args, "--path") {
            return PathBuf::from(p);
        }
        if let Some(p) = args.iter().rev().find(|a| !a.starts_with('-')) {
            // A bare trailing path is accepted, so `migrate plan hermes ~/.hermes`
            // works without the flag.
            let cand = PathBuf::from(p);
            if cand.is_dir() {
                return cand;
            }
        }
        default_roots()
            .into_iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, p)| p)
            .unwrap_or_default()
    }

    /// `--kind skill,agent` - restrict the plan to these item kinds.
    fn kind_filter(args: &[String]) -> Option<Vec<ItemKind>> {
        let raw = flag_value(args, "--kind")?;
        let mut out = Vec::new();
        for part in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let found = [
                ItemKind::Skill,
                ItemKind::Agent,
                ItemKind::Rule,
                ItemKind::Command,
                ItemKind::Prompt,
                ItemKind::Extension,
                ItemKind::Persona,
                ItemKind::Memory,
                ItemKind::Provider,
                ItemKind::Mcp,
                ItemKind::Schedule,
                ItemKind::Channel,
                ItemKind::Session,
                ItemKind::Credentials,
                ItemKind::Provider,
                ItemKind::Opaque,
            ]
            .into_iter()
            .find(|k| k.name() == part);
            match found {
                Some(k) => out.push(k),
                None => {
                    eprintln!("migrate: unknown kind '{part}'");
                    std::process::exit(2);
                }
            }
        }
        Some(out)
    }

    /// `--categories sessions,skills,config` - restrict the plan to these
    /// user-facing categories. This is the high-level filter; `--kind` is the
    /// low-level one. When both are given, the intersection is used.
    fn category_filter(args: &[String]) -> Option<MigrationFilter> {
        let raw = flag_value(args, "--categories")?;
        let mut cats = Vec::new();
        for part in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            match MigrationCategory::parse(part) {
                Some(c) => cats.push(c),
                None => {
                    eprintln!("migrate: unknown category '{part}'");
                    eprintln!(
                        "  known: {}",
                        MigrationCategory::all()
                            .iter()
                            .map(|c| c.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    std::process::exit(2);
                }
            }
        }
        Some(MigrationFilter::only(cats))
    }

    fn filtered(
        plan: &MigrationPlan,
        kinds: &Option<Vec<ItemKind>>,
        cats: &Option<MigrationFilter>,
    ) -> MigrationPlan {
        let mut plan = match kinds {
            Some(kinds) => MigrationPlan {
                source: plan.source.clone(),
                root: plan.root.clone(),
                source_version: plan.source_version.clone(),
                items: plan
                    .items
                    .iter()
                    .filter(|i| kinds.contains(&i.kind))
                    .cloned()
                    .collect(),
            },
            None => plan.clone(),
        };
        if let Some(cats) = cats {
            plan = cats.apply(&plan);
        }
        plan
    }

    fn print_json<T: serde::Serialize>(v: &T) {
        match serde_json::to_string_pretty(v) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("migrate: encode failed: {e}"),
        }
    }

    /// `pantheon migrate detect [path]`
    pub fn cmd_migrate_detect(args: &[String]) {
        let roots: Vec<PathBuf> = match flag_value(args, "--path") {
            Some(p) => vec![PathBuf::from(p)],
            None => default_roots().into_iter().map(|(_, p)| p).collect(),
        };
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut any = false;
        for root in &roots {
            if !root.is_dir() {
                continue;
            }
            let kinds = detect(root);
            if kinds.is_empty() {
                continue;
            }
            any = true;
            for k in kinds {
                let version = pantheon_migration::source_version(root, k);
                rows.push(serde_json::json!({
                    "source": k.name(),
                    "root": root.to_string_lossy(),
                    "version": version,
                    "items": analyze(root, k).len(),
                }));
            }
        }
        if has_flag(args, "--json") {
            print_json(&rows);
            return;
        }
        if !any {
            println!("no known source detected under {}", roots[0].display());
            return;
        }
        println!("{:<10} {:<7} ROOT", "SOURCE", "ITEMS");
        for r in &rows {
            println!(
                "{:<10} {:<7} {}",
                r["source"].as_str().unwrap_or("?"),
                r["items"].as_i64().unwrap_or(0),
                r["root"].as_str().unwrap_or("?")
            );
        }
    }

    /// `pantheon migrate plan <source> [path] [--kind K] [--json]`
    pub fn cmd_migrate_plan(args: &[String]) {
        let Some(kind) = source_arg(args) else {
            return;
        };
        let root = resolve_root(kind, args);
        if !root.is_dir() {
            eprintln!("migrate: {} root not found: {}", kind, root.display());
            std::process::exit(1);
        }
        let t = targets();
        let full = plan(&root, kind, &t);
        let selected = filtered(&full, &kind_filter(args), &category_filter(args));
        if has_flag(args, "--json") {
            print_json(&selected);
        } else {
            print!("{}", render(&selected));
            println!("  target root: {}", t.data_dir.display());
            println!("  nothing written; re-run with `--apply` to import");
        }
    }

    /// `pantheon migrate apply <source> [path] [--kind K] [--yes] [--json]`
    ///
    /// The approval gate. `--yes` skips the prompt; otherwise the operator has
    /// to type the source name, because this overwrites existing targets.
    pub fn cmd_migrate_apply(args: &[String]) {
        let Some(kind) = source_arg(args) else {
            return;
        };
        let root = resolve_root(kind, args);
        if !root.is_dir() {
            eprintln!("migrate: {} root not found: {}", kind, root.display());
            std::process::exit(1);
        }
        let t = targets();
        let full = plan(&root, kind, &t);
        let selected = filtered(&full, &kind_filter(args), &category_filter(args));

        if selected.imports() == 0 {
            println!("nothing to import from {}", kind);
            if selected.skipped() > 0 {
                println!(
                    "  {} credential path(s) skipped by policy",
                    selected.skipped()
                );
            }
            return;
        }

        if !has_flag(args, "--yes") {
            print!("{}", render(&selected));
            println!("about to import {} item(s):", selected.imports());
            for i in &selected.items {
                if let Some(target) = i.target() {
                    println!("  {:<10} {}", i.kind, target);
                }
            }
            if selected.archived() > 0 || selected.skipped() > 0 {
                println!(
                    "  ({} archived, {} credential paths skipped)",
                    selected.archived(),
                    selected.skipped()
                );
            }
            println!(
                "existing targets are backed up first, under {}",
                targets().backup_root().display()
            );
            print!("type '{}' to confirm: ", kind);
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let mut buf = String::new();
            let _ = std::io::stdin().read_line(&mut buf);
            if buf.trim() != kind.name() {
                println!("aborted");
                return;
            }
        }

        // backup -> apply -> validate, in that order, each reported.
        let manifest = match backup(&selected, &t) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("migrate: backup failed: {e}");
                std::process::exit(1);
            }
        };
        if !manifest.is_empty() {
            println!(
                "backup: {} pre-image(s) under {}",
                manifest.len(),
                t.backup_root().join(&manifest.id).display()
            );
        }

        let merge_providers = has_flag(args, "--merge-providers");
        let report = match apply_with(&selected, &t, &manifest, merge_providers) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("migrate: apply failed: {e}");
                std::process::exit(1);
            }
        };

        let v = validate(&selected);
        if has_flag(args, "--json") {
            print_json(&serde_json::json!({
                "backup": manifest,
                "apply": report,
                "validate": v,
            }));
        } else {
            for o in &report.outcomes {
                let mark = match o.status {
                    pantheon_migration::ApplyStatus::Created => "+",
                    pantheon_migration::ApplyStatus::Replaced => "~",
                    pantheon_migration::ApplyStatus::Unchanged => "=",
                    pantheon_migration::ApplyStatus::MemoryPlane => ">",
                    _ => "!",
                };
                println!("  {} {:<10} {}", mark, o.kind, o.target);
                if !o.detail.is_empty() {
                    println!("      {}", o.detail);
                }
                for s in &o.skipped {
                    println!("      skipped: {s}");
                }
            }
            print!("{}", v.render());
        }

        if !report.complete || !v.complete {
            eprintln!(
                "migrate: import incomplete - {} failure(s), {} validation problem(s)",
                report.failures(),
                v.problems()
            );
            eprintln!("  nothing was half-written: the apply is transactional, so targets are untouched or were rolled back; the backup is at {}",
                t.backup_root().join(&manifest.id).display());
            std::process::exit(1);
        }
        println!(
            "migrated {} from {} - {} imported, {} archived, {} credentials skipped",
            kind,
            root.display(),
            report.ok(),
            selected.archived(),
            selected.skipped()
        );
        print_key_reconciliation(&t);
        import_session_runs(&t, &selected, kind);
        index_imported_sessions(&t, &root, kind);
    }

    /// Turn the selected session transcripts into real, resumable ledger runs.
    ///
    /// Runs after the file apply so the quarantine copy lands first. Each
    /// transcript becomes one run with a deterministic
    /// `imported:<source>:<dir>:<session>` id - re-running converges instead of
    /// duplicating. The runs show up in `/resume` and continue like any native
    /// conversation; tool traffic is dropped with a note on the run.
    fn import_session_runs(t: &Targets, plan: &MigrationPlan, kind: SourceKind) {
        use pantheon_migration::session_import::{import_session_dirs, ImportStatus};
        let dirs: Vec<PathBuf> = plan
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Session && matches!(i.action, Action::Import { .. }))
            .map(|i| PathBuf::from(&i.path))
            .collect();
        if dirs.is_empty() {
            return;
        }
        let ledger = match pantheon_storage::Ledger::open(&t.data_dir.join("ledger.db")) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("migrate: session import failed: {e}");
                std::process::exit(1);
            }
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let batch = import_session_dirs(&ledger, kind.name(), &dirs, now_ms);
        if !batch.failures.is_empty() {
            for (path, cause) in &batch.failures {
                eprintln!(
                    "migrate: session import failed for {}: {cause}",
                    path.display()
                );
            }
            std::process::exit(1);
        }
        let imported: Vec<_> = batch
            .reports
            .iter()
            .filter(|r| r.status == ImportStatus::Imported)
            .collect();
        let already: Vec<_> = batch
            .reports
            .iter()
            .filter(|r| r.status == ImportStatus::SkippedExists)
            .collect();
        let empty = batch.reports.len() - imported.len() - already.len();
        if imported.is_empty() && already.is_empty() {
            return;
        }
        println!();
        println!(
            "imported {} session(s) as resumable runs ({} already imported, {} empty)",
            imported.len(),
            already.len(),
            empty
        );
        for r in &imported {
            println!(
                "  + {} - {} ({} turn(s), {} reasoning trace(s))",
                r.run_id,
                r.title,
                r.turns(),
                r.reasoning_traces
            );
            if r.dropped_tool_records > 0 {
                println!(
                    "      ({} tool record(s) dropped: not replayable)",
                    r.dropped_tool_records
                );
            }
        }
    }

    /// Make a migrated history actually searchable.
    ///
    /// The quarantine copy is otherwise a file nobody reads. This indexes it into
    /// the same `session_search` store the live runtime writes, under a
    /// `migrated:<source>:<session>` run id so a hit can never be mistaken for a
    /// real ledger event. Idempotent: `chunk_id` is derived from
    /// (source, file, line), so re-running converges instead of duplicating.
    ///
    /// Part of the migration contract: an import is not complete until the
    /// imported sessions are searchable. A failure here exits 1 - the
    /// transcripts are on disk but not findable, and the operator must see it.
    fn index_imported_sessions(t: &Targets, root: &Path, kind: SourceKind) {
        let index_path = t.data_dir.join("ledger.db");
        let index = match pantheon_storage::search::SessionSearch::open(&index_path) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("migrate: session indexing skipped: {e}");
                return;
            }
        };
        match ensure_sessions_indexed(t, kind.name(), &index) {
            Ok(r) if r.chunks > 0 => {
                println!();
                println!(
                    "indexed {} transcript file(s), {} chunk(s) into session_search (run_id prefix 'migrated:{}:')",
                    r.files,
                    r.chunks,
                    kind.name()
                );
                if !r.empty_files.is_empty() {
                    println!(
                        "  {} file(s) produced nothing and were named in the manifest",
                        r.empty_files.len()
                    );
                }
            }
            Ok(_) => {}
            Err(e) => {
                // Part of the migration contract now: the import is not
                // complete until the imported sessions are searchable. This
                // exits 1 so automation and the operator both see it.
                eprintln!("migrate: session indexing failed: {e}");
                std::process::exit(1);
            }
        }
        let _ = root;
    }

    /// Which carried keys pantheon's catalog will actually pick up.
    ///
    /// The credential carry moves any provider/channel/mcp key whose name looks
    /// right, and pantheon's catalog names each provider's `key_env` after the
    /// same `<PROVIDER>_API_KEY` convention. That alignment usually works, but
    /// nothing verified it - so a key that matches no catalog entry used to look
    /// identical to one that does. This is that check.
    fn print_key_reconciliation(t: &Targets) {
        let Ok(text) = std::fs::read_to_string(pantheon_migration::pantheon_env_path(&t.data_dir))
        else {
            return;
        };
        let names: Vec<String> = pantheon_migration::parse_env_names(&text)
            .into_iter()
            .map(|e| e.name)
            .collect();
        if names.is_empty() {
            return;
        }
        let reports = reconcile_keys(&names);
        let catalogued: Vec<&pantheon_migration::KeyReport> = reports
            .iter()
            .filter(|r| r.match_kind == KeyMatch::Catalogued)
            .collect();
        let rename: Vec<&pantheon_migration::KeyReport> = reports
            .iter()
            .filter(|r| matches!(r.match_kind, KeyMatch::NeedsRename { .. }))
            .collect();
        if catalogued.is_empty() && rename.is_empty() {
            return;
        }
        println!();
        println!("key store vs pantheon's provider catalog:");
        if !catalogued.is_empty() {
            println!(
                "  {} key(s) already match a catalog provider and are usable as-is:",
                catalogued.len()
            );
            for r in &catalogued {
                println!(
                    "    {} -> provider {}",
                    r.env_var,
                    r.provider.as_deref().unwrap_or("?")
                );
            }
        }
        for r in &rename {
            if let KeyMatch::NeedsRename { catalog_env } = &r.match_kind {
                println!(
                    "    {} matches provider {} but pantheon reads {} - rename it to be picked up",
                    r.env_var,
                    r.provider.as_deref().unwrap_or("?"),
                    catalog_env
                );
            }
        }
    }

    /// `pantheon migrate validate <source> [path] [--kind K] [--json]`
    pub fn cmd_migrate_validate(args: &[String]) {
        let Some(kind) = source_arg(args) else {
            return;
        };
        let root = resolve_root(kind, args);
        let t = targets();
        let full = plan(&root, kind, &t);
        let selected = filtered(&full, &kind_filter(args), &category_filter(args));
        let v = validate(&selected);
        if has_flag(args, "--json") {
            print_json(&v);
        } else {
            print!("{}", v.render());
        }
        if !v.complete {
            std::process::exit(1);
        }
    }

    /// `pantheon migrate show <source> [path]` - the detected items with their
    /// disposition, without building import targets. Read-only, no plan.
    pub fn cmd_migrate_show(args: &[String]) {
        let Some(kind) = source_arg(args) else {
            return;
        };
        let root = resolve_root(kind, args);
        if !root.is_dir() {
            eprintln!("migrate: {} root not found: {}", kind, root.display());
            std::process::exit(1);
        }
        let detected = analyze(&root, kind);
        if has_flag(args, "--json") {
            print_json(&detected);
            return;
        }
        println!("{:<12} {:<5} PATH", "KIND", "MAP");
        for d in &detected {
            println!(
                "{:<12} {:<5} {}",
                d.kind,
                if d.mappable { "yes" } else { "no" },
                d.path
            );
            println!("             {}", d.note);
        }
        println!(
            "\n{} detected, {} mappable, {} credential path(s)",
            detected.len(),
            detected.iter().filter(|d| d.mappable).count(),
            detected
                .iter()
                .filter(|d| d.kind == ItemKind::Secret)
                .count()
        );
    }

    /// The first non-flag argument is the source name, unless it is a verb.
    fn source_arg(args: &[String]) -> Option<SourceKind> {
        for a in args {
            if a.starts_with('-') {
                continue;
            }
            if matches!(
                a.as_str(),
                "detect" | "plan" | "apply" | "validate" | "show"
            ) {
                continue;
            }
            match SourceKind::parse(a) {
                Some(k) => return Some(k),
                None => {
                    eprintln!("migrate: unknown source '{a}'");
                    eprintln!("  known: hermes, openclaw, omp (aliases: oh-my-pi, pi), claude");
                    std::process::exit(2);
                }
            }
        }
        eprintln!("migrate: name a source");
        eprintln!(
            "  usage: pantheon migrate <detect|show|plan|apply|validate> <hermes|openclaw|omp|claude>"
        );
        std::process::exit(2);
    }

    /// Dispatch. `verb` is args[2] from main's match arm.
    pub fn dispatch(verb: &str, args: &[String]) {
        match verb {
            "detect" => cmd_migrate_detect(args),
            "show" => cmd_migrate_show(args),
            "plan" => cmd_migrate_plan(args),
            "apply" => cmd_migrate_apply(args),
            "validate" => cmd_migrate_validate(args),
            other => {
                eprintln!("migrate: unknown verb '{other}'");
                eprintln!(
                    "  usage: pantheon migrate <detect|show|plan|apply|validate> <source> [path]"
                );
                std::process::exit(2);
            }
        }
    }

    /// `migrate` needs a verb before anything else, so this is the entry main
    /// calls with the full arg tail.
    pub fn cmd_migrate(args: &[String]) {
        let Some(verb) = args.first() else {
            eprint!("{MIGRATE_USAGE}");
            std::process::exit(2);
        };
        if matches!(verb.as_str(), "help" | "--help" | "-h") {
            print!("{MIGRATE_USAGE}");
            return;
        }
        dispatch(verb, &args[1..]);
    }

    const MIGRATE_USAGE: &str = "\
    usage: pantheon setup migrate <detect|show|plan|apply|validate> <hermes|openclaw|omp|claude> [path]
      detect    which sources are installed here
      show      every detected item and its disposition
      plan      dry run: import / archive / skip (read-only)
      apply     backup, import, validate (the only writer)
      validate  re-check current targets, no writes

      item kinds for --kind:
        skill, agent, rule, command, prompt, extension, persona, memory
        provider, mcp, schedule, channel, session, credentials

      credentials are carried into <data_dir>/.env, pantheon's own key
      store. An existing key is never overwritten; it is reported instead.
      custom providers are written to <data_dir>/providers/imported.toml
      for review; add --merge-providers to also merge them into config.toml.
    ";
}

pub mod reset {
    //! `pantheon reset`: scoped destructive cleanup with confirmation.
    //!
    //! --config   delete config.toml + memory-backend.toml (fast to redo)
    //! --state    delete ledger.db, memory.db, gateway cursors (run history)
    //! --everything   everything --config and --state remove, and nothing more
    //!
    //! The three flags are mutually exclusive; passing two is an error rather
    //! than a silent precedence rule.
    //!
    //! State resets are refused while any active lease exists, and also refused
    //! when the lease store cannot be read: reset under a live session would
    //! corrupt the durability guarantees, and "cannot prove it is safe" is not
    //! the same as "it is safe". Confirmation is interactive by default; --yes
    //! skips it for scripts.
    //!
    //! Safewrite staging is NOT removed by --everything. It is per-path working
    //! state, not configuration, and blowing it away alongside a config reset
    //! would discard in-flight edits the user may still want to apply.
    //!
    //! The gateway outbox IS removed by --state. A queued message is a
    //! deliverable: leaving it behind means `reset --state` still sends it, to a
    //! conversation whose ledger has just been deleted.

    use pantheon_api::error::PantheonError;
    use pantheon_storage::RunLeaseStore;
    use std::path::{Path, PathBuf};

    /// Files a reset scope removes. Directories (the gateway outbox) are listed
    /// too and removed recursively by the caller.
    fn target_files(data_dir: &Path, scope: &str) -> Vec<PathBuf> {
        let config = [
            data_dir.join("config.toml"),
            data_dir.join("memory-backend.toml"),
        ];
        let state = [
            data_dir.join("ledger.db"),
            data_dir.join("memory.db"),
            data_dir.join("gateway").join("tg-cursor"),
            data_dir.join("gateway").join("discord-cursor"),
            // Replies queued by `pantheon run --deliver` that the gateway has not
            // sent yet. Leaving them behind means a state reset still delivers
            // them, into a conversation whose ledger was just deleted.
            data_dir.join("gateway").join("outbox"),
        ];
        let mut v: Vec<PathBuf> = match scope {
            "config" => config.to_vec(),
            "state" => state.to_vec(),
            _ => config.iter().chain(state.iter()).cloned().collect(),
        };
        v.sort();
        v.dedup();
        v.retain(|p| p.exists());
        v
    }

    /// Whether a run lease is still active, i.e. whether a destructive state
    /// reset must be refused.
    ///
    /// Returns `Err` when the lease store cannot be read. The caller must treat
    /// that as "refuse", not "proceed": an unreadable ledger is exactly the case
    /// where we cannot prove no session is live, and deleting `ledger.db` under a
    /// running session destroys its recovery guarantees. Failing open here means
    /// the one case that most needs the guard is the one case that skips it.
    pub fn active_lease_exists(data_dir: &Path) -> Result<bool, PantheonError> {
        let path = data_dir.join("ledger.db");
        if !path.exists() {
            return Ok(false);
        }
        let store = RunLeaseStore::open(&path)?;
        Ok(!store.list_active()?.is_empty())
    }

    pub fn cmd_reset(args: &[String]) {
        if args.iter().any(|a| a == "--help" || a == "-h") {
            println!("usage: pantheon reset --config|--state|--everything [--yes]");
            println!("  --config      delete config.toml + memory-backend.toml (keeps ledger, memory, gateway state)");
            println!(
                "  --state       delete ledger, memory, gateway cursors/outbox (keeps config)"
            );
            println!("  --everything  delete config and state");
            println!("  --yes         skip the 'type reset to confirm' prompt");
            return;
        }
        // An unknown flag used to be swallowed whenever a scope flag was
        // also present, so `reset --config --bogus` deleted config while
        // looking like it had honored something else.
        for a in args.iter().skip(2) {
            if a.starts_with('-')
                && !matches!(
                    a.as_str(),
                    "--config" | "--state" | "--everything" | "--yes"
                )
            {
                eprintln!("reset: unknown flag '{a}' (want --config|--state|--everything [--yes])");
                std::process::exit(2);
            }
            if !a.starts_with('-') {
                eprintln!("reset: unexpected argument '{a}'");
                std::process::exit(2);
            }
        }
        // Collect every scope flag first. Chaining `if/else if` meant
        // `--config --state` silently reset only the config and said nothing
        // about the state the user also asked to clear.
        let wanted: Vec<&str> = ["config", "state", "everything"]
            .into_iter()
            .filter(|s| args.iter().any(|a| a == &format!("--{s}")))
            .collect();
        let scope = match wanted.as_slice() {
            [] => {
                eprintln!("usage: pantheon reset --config|--state|--everything [--yes]");
                std::process::exit(2);
            }
            // `--everything` is a superset, so it wins over the narrower flags
            // rather than being silently ignored behind them.
            [s] => *s,
            many => {
                eprintln!(
                    "reset: {} are mutually exclusive; pass exactly one",
                    many.iter()
                        .map(|s| format!("--{s}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                std::process::exit(2);
            }
        };
        let data_dir = crate::terminal::data_dir();
        let targets = target_files(&data_dir, scope);
        if targets.is_empty() {
            println!("nothing to reset for scope {scope}");
            return;
        }
        if scope != "config" {
            match active_lease_exists(&data_dir) {
                Ok(true) => {
                    eprintln!("reset: a run lease is active; stop the session first");
                    std::process::exit(1);
                }
                Ok(false) => {}
                Err(e) => {
                    eprintln!(
                        "reset: cannot read the lease store, refusing to delete state: {e}\n\
                         hint: if no session is running, remove the ledger yourself"
                    );
                    std::process::exit(1);
                }
            }
        }
        let assume_yes = args.iter().any(|a| a == "--yes");
        if !assume_yes {
            println!("about to delete:");
            for t in &targets {
                println!("  {}", t.display());
            }
            print!("type 'reset' to confirm: ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let mut buf = String::new();
            let _ = std::io::stdin().read_line(&mut buf);
            if buf.trim() != "reset" {
                println!("aborted");
                return;
            }
        }
        let mut deleted = 0;
        for t in &targets {
            // A target can be a directory (the gateway outbox). `remove_file` on
            // one fails with EISDIR, which would print a scary error and leave
            // queued messages queued.
            let r = if t.is_dir() {
                std::fs::remove_dir_all(t)
            } else {
                std::fs::remove_file(t)
            };
            match r {
                Ok(()) => deleted += 1,
                Err(e) => eprintln!("reset: could not delete {}: {e}", t.display()),
            }
        }
        println!("deleted {deleted} files ({scope})");
    }
}

pub mod uninstall {
    //! `pantheon uninstall [--yes] [--include-secrets]`.
    //!
    //! Removes Pantheon's config and data, and tells the operator exactly how
    //! to finish the job by hand. This verb never edits shell rc files: PATH
    //! lines are printed as instructions, because a program rewriting the
    //! user's shell config on the way out is how uninstalls go wrong.
    //!
    //! What is removed with `--yes`:
    //! - `<data_dir>/config.toml` and every database (`*.db`)
    //! - every subdirectory of the data dir (backups, gateway state, skills,
    //!   extensions, mcp state, logs)
    //! - `<data_dir>/.env` and `<data_dir>/.secrets.key` ONLY with
    //!   `--include-secrets` (default: kept - secrets are the one thing an
    //!   uninstall must not surprise-delete)
    //!
    //! What it does NOT do (printed as instructions instead):
    //! - delete the `pantheon` binary (a running binary removing itself is
    //!   platform-dependent; the path is printed)
    //! - remove the PATH line from shell rc files (printed verbatim)

    use std::path::PathBuf;

    pub fn usage() -> &'static str {
        "usage: pantheon uninstall [--yes] [--include-secrets]\n\
         \n\
         Remove Pantheon's config and data from this machine.\n\
         Without --yes this only lists what would be removed.\n\
         Secrets (.env, .secrets.key) are kept unless --include-secrets is passed.\n\
         The binary and shell PATH lines are never touched; the exact\n\
         removal steps are printed instead."
    }

    /// What uninstall would delete, for the dry-run listing and the --yes
    /// confirmation. Secret files (`.env`, `.secrets.key`) are excluded unless
    /// `include_secrets`.
    fn planned_removals(data_dir: &std::path::Path, include_secrets: bool) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(data_dir).map(|r| r.flatten().collect::<Vec<_>>());
        for e in entries.unwrap_or_default() {
            let p = e.path();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Secrets are the one thing an uninstall must not surprise-delete.
            if (name == ".env" || name == ".secrets.key") && !include_secrets {
                continue;
            }
            out.push(p);
        }
        out.sort();
        out
    }

    fn shell_rc_candidates() -> Vec<PathBuf> {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return Vec::new();
        }
        let shell = std::env::var("SHELL").unwrap_or_default();
        let base = std::path::Path::new(&shell)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut out = Vec::new();
        match base.as_str() {
            "zsh" => out.push(format!("{home}/.zshrc")),
            "fish" => out.push(format!("{home}/.config/fish/config.fish")),
            _ => out.push(format!("{home}/.bashrc")),
        }
        // Always mention the other two: SHELL can be wrong (login vs
        // interactive shell), and the installer may have written elsewhere.
        for extra in [".bashrc", ".zshrc", ".config/fish/config.fish"] {
            let p = format!("{home}/{extra}");
            if !out.iter().any(|q| q == &p) {
                out.push(p);
            }
        }
        out.into_iter().map(PathBuf::from).collect()
    }

    pub fn cmd_uninstall(args: &[String]) {
        if args.iter().any(|a| a == "--help" || a == "-h") {
            println!("{}", usage());
            return;
        }
        if let Some(other) = args
            .iter()
            .skip(2)
            .find(|a| a.starts_with("--") && *a != "--yes" && *a != "--include-secrets")
        {
            eprintln!("uninstall: unknown flag '{other}'");
            eprintln!("{usage}", usage = usage());
            std::process::exit(2);
        }
        let yes = args.iter().any(|a| a == "--yes");
        let include_secrets = args.iter().any(|a| a == "--include-secrets");
        let data_dir = crate::terminal::data_dir();
        let removals = planned_removals(&data_dir, include_secrets);

        let bin = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<could not locate the pantheon binary>".to_string());
        let bin_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.display().to_string()))
            .unwrap_or_else(|| "$HOME/.local/bin".to_string());

        if !yes {
            println!("would remove (re-run with --yes to do it):");
            if removals.is_empty() {
                println!("  (nothing: {} is empty or missing)", data_dir.display());
            }
            for p in &removals {
                println!("  {}", p.display());
            }
            if !include_secrets {
                for secret in [".env", ".secrets.key"] {
                    if data_dir.join(secret).is_file() {
                        println!(
                            "  (keeping {} - pass --include-secrets to remove it)",
                            data_dir.join(secret).display()
                        );
                    }
                }
            }
            println!();
            println!("manual steps (never done automatically):");
            println!("  1. delete the binary: rm {bin}");
            println!("  2. remove the PATH line the installer added. Look for this line");
            println!("     (and the '# added by the pantheon installer' marker above it):");
            println!("       export PATH=\"{bin_dir}:$PATH\"");
            println!("     in:");
            for rc in shell_rc_candidates() {
                println!("       {}", rc.display());
            }
            return;
        }

        // --yes: data removal. The operator was told to stop every Pantheon
        // process first (see the dry-run output); deleting databases out from
        // under a running gateway corrupts in ways a restore cannot fix, so
        // the instruction is repeated, not just implied.
        if removals
            .iter()
            .any(|p| p.file_name().map(|n| n == "ledger.db").unwrap_or(false))
        {
            eprintln!("note: make sure no Pantheon process is running (serve, gateway, sessions)");
            eprintln!("before its databases are deleted from under it.");
        }
        let mut removed = 0;
        for p in &removals {
            let res = if p.is_dir() {
                std::fs::remove_dir_all(p)
            } else {
                std::fs::remove_file(p)
            };
            match res {
                Ok(()) => {
                    removed += 1;
                }
                Err(e) => {
                    eprintln!("uninstall: could not remove {}: {e}", p.display());
                    std::process::exit(1);
                }
            }
        }
        println!("removed {removed} item(s) from {}", data_dir.display());
        for secret in [".env", ".secrets.key"] {
            if !include_secrets && data_dir.join(secret).is_file() {
                println!(
                    "kept {} (secrets); re-run with --include-secrets to remove it",
                    data_dir.join(secret).display()
                );
            }
        }
        println!();
        println!("manual steps to finish:");
        println!("  1. delete the binary: rm {bin}");
        println!("  2. remove this line (and its '# added by the pantheon installer' marker)");
        println!("     from your shell rc file:");
        println!("       export PATH=\"{bin_dir}:$PATH\"");
        for rc in shell_rc_candidates() {
            println!("       check {}", rc.display());
        }
    }
}

pub mod update {
    //! `pantheon update`: replace this binary with the latest GitHub release.
    //!
    //! The installer downloads a prebuilt binary; this verb is the same path
    //! going forward. No toolchain, no source build: resolve the release,
    //! download the matching asset for this OS/arch, verify the checksum when
    //! published, and swap the running binary.
    //!
    //! Failures name the exact stage (resolve, download, verify, replace) and
    //! a timeout names the probe that timed out - never a generic "update
    //! failed".

    use std::io::Read;
    use std::path::PathBuf;
    use std::time::Duration;

    const DEFAULT_REPO: &str = "k1ng0mar/pantheon";
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

    fn repo() -> String {
        std::env::var("PANTHEON_REPO").unwrap_or_else(|_| DEFAULT_REPO.into())
    }

    fn current_version() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// Asset OS id for this build.
    fn os_id() -> Option<&'static str> {
        match std::env::consts::OS {
            "linux" => Some("linux"),
            "macos" => Some("darwin"),
            "windows" => Some("windows"),
            _ => None,
        }
    }

    /// Asset arch id for this build.
    fn arch_id() -> Option<&'static str> {
        match std::env::consts::ARCH {
            "x86_64" => Some("x86_64"),
            "aarch64" => Some("aarch64"),
            _ => None,
        }
    }

    /// Asset file name for a release tag, e.g.
    /// `pantheon-v0.1.0-linux-x86_64.tar.gz`.
    fn asset_name(tag: &str, os: &str, arch: &str) -> String {
        let ext = if os == "windows" { "zip" } else { "tar.gz" };
        format!("pantheon-{tag}-{os}-{arch}.{ext}")
    }

    fn download_url(repo: &str, tag: &str, asset: &str) -> String {
        format!("https://github.com/{repo}/releases/download/{tag}/{asset}")
    }

    /// True when `latest` is newer than `current`. Tags carry a leading `v`;
    /// comparison is numeric per dot-separated component, falling back to
    /// "different means newer" for anything non-numeric.
    fn is_newer(current: &str, latest: &str) -> bool {
        let norm = |v: &str| v.trim().trim_start_matches('v').to_string();
        let (c, l) = (norm(current), norm(latest));
        if c == l {
            return false;
        }
        let parse = |v: &str| {
            v.split('.')
                .map(|p| p.parse::<u64>().unwrap_or(0))
                .collect::<Vec<_>>()
        };
        parse(&l) > parse(&c)
    }

    /// Agent for update traffic: bounded, platform roots, default redirect
    /// following (release downloads redirect to a CDN).
    fn update_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent()
    }

    /// Resolve the latest release tag via the GitHub API.
    fn resolve_latest_tag(repo: &str) -> Result<String, String> {
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        let resp = update_agent()
            .get(&url)
            .header("User-Agent", "pantheon-update")
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| match e {
                ureq::Error::StatusCode(code) => {
                    format!("resolve failed: GitHub API returned HTTP {code} for {repo}")
                }
                e => format!("resolve failed: could not reach api.github.com ({e})"),
            })?;
        let text = resp
            .into_body()
            .read_to_string()
            .map_err(|e| format!("resolve failed: could not read GitHub response ({e})"))?;
        // Minimal parse: find "tag_name": "vX.Y.Z" without serde_json::Value
        // plumbing (serde_json is available, but a full struct is overkill).
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("resolve failed: bad JSON ({e})"))?;
        v.get("tag_name")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "resolve failed: GitHub response had no tag_name".to_string())
    }

    fn download_bytes(url: &str) -> Result<Vec<u8>, String> {
        let resp = update_agent()
            .get(url)
            .header("User-Agent", "pantheon-update")
            .call()
            .map_err(|e| match e {
                ureq::Error::StatusCode(code) => {
                    format!("download failed: HTTP {code} for {url}")
                }
                e => format!("download failed: could not fetch {url} ({e})"),
            })?;
        let mut buf = Vec::new();
        resp.into_body()
            .into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| format!("download failed: could not read {url} ({e})"))?;
        Ok(buf)
    }

    /// Extract the `pantheon` binary from `archive` into `dest_dir` using the
    /// system `tar` (the installer guarantees it; Windows 10+ ships one that
    /// handles zip). Returns the extracted binary path.
    fn extract_with_tar(
        archive: &std::path::Path,
        dest_dir: &std::path::Path,
    ) -> Result<PathBuf, String> {
        // List first and reject traversal before extracting anything: a
        // hostile archive must not carry absolute paths or `..` members
        // out of the temp dir. Then extract ONLY the single member whose
        // basename is exactly the wanted binary (fail on zero or on
        // ambiguity) and require it to be a regular file, never a symlink.
        // The old code extracted the whole archive and walked the tree,
        // which let a symlink member become the installed binary.
        let list = std::process::Command::new("tar")
            .arg("-tf")
            .arg(archive)
            .output()
            .map_err(|e| {
                format!("install failed: could not run `tar` ({e}); install tar and re-run")
            })?;
        if !list.status.success() {
            return Err("install failed: `tar` could not list the release archive".into());
        }
        let names: &[&str] = if cfg!(windows) {
            &["pantheon.exe"]
        } else {
            &["pantheon"]
        };
        let mut candidates: Vec<String> = Vec::new();
        for line in String::from_utf8_lossy(&list.stdout).lines() {
            let entry = line.trim();
            if entry.is_empty() {
                continue;
            }
            if entry.starts_with('/') || entry.starts_with('\\') {
                return Err(format!("install failed: absolute archive member {entry:?}"));
            }
            if entry
                .split(|c| c == '/' || c == '\\')
                .any(|seg| seg == "..")
            {
                return Err(format!(
                    "install failed: archive member escapes its directory: {entry:?}"
                ));
            }
            let base = entry
                .rsplit(|c| c == '/' || c == '\\')
                .next()
                .unwrap_or(entry);
            if names.contains(&base) {
                candidates.push(entry.to_string());
            }
        }
        if candidates.is_empty() {
            return Err("install failed: archive did not contain a pantheon binary".into());
        }
        if candidates.len() > 1 {
            return Err(format!(
                "install failed: ambiguous archive ({} members named like the binary)",
                candidates.len()
            ));
        }
        let member = candidates.into_iter().next().unwrap();
        // NOTE: `-C dest` must precede the member name: tar applies -C
        // to the arguments that follow it, so `-C` after the member
        // would extract into the process cwd instead of dest.
        let st = std::process::Command::new("tar")
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(dest_dir)
            .arg(&member)
            .status()
            .map_err(|e| {
                format!("install failed: could not run `tar` ({e}); install tar and re-run")
            })?;
        if !st.success() {
            return Err("install failed: `tar` could not extract the release archive".into());
        }
        let extracted = dest_dir.join(member.strip_prefix("./").unwrap_or(member.as_str()));
        match std::fs::symlink_metadata(&extracted) {
            Ok(md) if md.file_type().is_file() => Ok(extracted),
            Ok(_) => Err("install failed: extracted binary is not a regular file".into()),
            Err(e) => Err(format!(
                "install failed: cannot read {} ({e})",
                extracted.display()
            )),
        }
    }

    /// Pure checksum comparison, extracted for testing: find the digest
    /// for `asset` in the published `.sha256` text (plain `<hex>` lines,
    /// `<hex>  <file>` sha256sum lines, and `SHA256 (file) = <hex>` BSD
    /// lines all work) and compare it against `actual_hex`.
    ///
    /// The digest is bound to the asset when the file names it: if any
    /// line names the asset, that line's digest wins. Otherwise the file
    /// must carry exactly one digest; a multi-digest file that never
    /// names the asset is ambiguous and counts as "no usable digest"
    /// (fail closed) rather than the first token, which might belong to
    /// another file.
    ///
    /// NOTE: a checksum only proves the bytes match the publisher's
    /// published digest when the digest itself is authentic. The
    /// `.sha256` file is fetched from the same origin as the binary, so
    /// a compromised release can serve a matching pair. Real
    /// authentication needs signatures (see NEEDS DECISION in the
    /// security fix report); until then this is a corruption check, not
    /// a trust anchor.
    ///
    /// Returns Ok(true) on match, Ok(false) when the text carries no usable
    /// digest, Err on mismatch.
    fn checksum_line_matches(text: &str, actual_hex: &str, asset: &str) -> Result<bool, String> {
        let tokens = |line: &str| -> Vec<String> {
            line.split_whitespace()
                .map(|t| {
                    t.trim_matches(|c: char| c == '(' || c == ')' || c == '=' || c == '*')
                        .to_lowercase()
                })
                .filter(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
                .collect()
        };
        let mut all: Vec<String> = Vec::new();
        for line in text.lines() {
            let found = tokens(line);
            if found.is_empty() {
                continue;
            }
            if !asset.is_empty() && line.contains(asset) {
                return check_digest(&found[0], actual_hex);
            }
            all.extend(found);
        }
        if all.len() == 1 {
            return check_digest(&all[0], actual_hex);
        }
        Ok(false)
    }

    fn check_digest(expected: &str, actual_hex: &str) -> Result<bool, String> {
        if actual_hex.to_lowercase() != expected {
            return Err("verify failed: checksum mismatch (download may be corrupt)".into());
        }
        Ok(true)
    }

    /// Gate a verification outcome: only a digest match installs.
    /// Ok(false) — checksum file exists but carries no usable digest — is
    /// an error here, not a silent skip: the caller must not treat "no
    /// digest found" as "verified". Pure, for testing.
    fn require_verified(verified: Result<bool, String>) -> Result<(), String> {
        match verified {
            Ok(true) => Ok(()),
            Ok(false) => Err("verify failed: checksum file carries no usable digest".into()),
            Err(e) => Err(e),
        }
    }

    /// Verify `asset_bytes` against the published `.sha256` file.
    ///
    /// CORRUPTION CHECK ONLY: the checksum file comes from the same
    /// origin as the asset, so it can never authenticate the publisher.
    /// Callers must also pass [`verify_signature`] before installing
    /// anything; the checksum proves the bytes are intact, the signature
    /// proves who published them.
    ///
    /// Fail-closed: a missing checksum file (404), any network error, or an
    /// unreadable checksum file is an error, not a silent skip. Returns
    /// Ok(true) only when the digest matches; Ok(false) only when the
    /// checksum file exists but carries no usable digest (callers must
    /// pass this through [`require_verified`], never install on it).
    fn verify_checksum(asset_url: &str, asset_bytes: &[u8]) -> Result<bool, String> {
        let sha_url = format!("{asset_url}.sha256");
        let resp = update_agent()
            .get(&sha_url)
            .header("User-Agent", "pantheon-update")
            .call()
            .map_err(|e| match e {
                ureq::Error::StatusCode(code) => {
                    format!("verify failed: checksum file returned HTTP {code} ({sha_url})")
                }
                _ => format!("verify failed: could not fetch the checksum file ({e})"),
            })?;
        let text = resp
            .into_body()
            .read_to_string()
            .map_err(|_| "verify failed: could not read the checksum file".to_string())?;
        // Hash via the system: sha256sum, shasum, or certutil on Windows.
        let actual = hash_via_system(asset_bytes)?;
        let asset = asset_url.rsplit('/').next().unwrap_or("");
        checksum_line_matches(&text, &actual, asset)
    }

    /// Authenticate `asset_bytes` against the pinned release signing key:
    /// fetch the `{asset_url}.minisig` published alongside the asset and
    /// verify it offline. Fail-closed like the checksum path: a missing
    /// signature, an absent pinned key, or a mismatch aborts the install.
    /// The helper-bootstrap download (the only asset this binary installs
    /// directly - the main update is handed to the helper, which verifies
    /// both archives itself) must pass this before extraction.
    fn verify_signature(asset_url: &str, asset_bytes: &[u8]) -> Result<(), String> {
        let sig_url = format!("{asset_url}.minisig");
        let sig_bytes = download_bytes(&sig_url)
            .map_err(|e| format!("signature verification failed: cannot fetch {sig_url} ({e})"))?;
        let sig_text = String::from_utf8(sig_bytes).map_err(|_| {
            "signature verification failed: .minisig is not valid UTF-8".to_string()
        })?;
        pantheon_release_verify::verify_signature(asset_bytes, &sig_text)
    }

    fn hash_via_system(bytes: &[u8]) -> Result<String, String> {
        use std::io::Write;
        for (prog, args) in [
            ("sha256sum", vec![] as Vec<&str>),
            ("shasum", vec!["-a", "256"]),
        ] {
            if let Ok(mut child) = std::process::Command::new(prog)
                .args(&args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(bytes);
                }
                if let Ok(out) = child.wait_with_output() {
                    if out.status.success() {
                        let s = String::from_utf8_lossy(&out.stdout);
                        if let Some(hex) = s.split_whitespace().next() {
                            return Ok(hex.to_string());
                        }
                    }
                }
            }
        }
        if cfg!(windows) {
            // certutil needs a file; write to temp.
            let tmp =
                std::env::temp_dir().join(format!("pantheon-hash-{}.bin", std::process::id()));
            std::fs::write(&tmp, bytes).map_err(|e| format!("verify failed: {e}"))?;
            let out = std::process::Command::new("certutil")
                .args(["-hashfile", &tmp.to_string_lossy(), "SHA256"])
                .output()
                .map_err(|e| format!("verify failed: {e}"))?;
            let _ = std::fs::remove_file(&tmp);
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                if t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Ok(t);
                }
            }
        }
        Err("verify failed: no sha256 tool found (install sha256sum)".into())
    }

    /// Splash hint probe: return the newer release tag when one exists,
    /// `None` when up to date or unreachable. Network failures are silent
    /// the splash simply shows no hint.
    pub fn check_for_update() -> Option<String> {
        let latest = resolve_latest_tag(&repo()).ok()?;
        if is_newer(&current_version(), &latest) {
            Some(latest)
        } else {
            None
        }
    }

    pub fn usage() -> &'static str {
        "usage: pantheon update [--check] [--version TAG] [--repo OWNER/REPO] [--rollback]\n  \n  Fetches the latest GitHub release and replaces this binary.\n  --check reports without changing anything.\n  --rollback restores the previous binary (pantheon.prev, kept next to\n  the binary by every update) without touching the network.\n  Verification is fail-closed: a missing, unreadable, or mismatched\n  minisign signature aborts the update (the checksum is a corruption\n  check only, never authentication). There is no --allow-unverified flag: an unverified binary is never installed."
    }

    /// `pantheon update --rollback`: restore the previous binary from the
    /// `pantheon.prev` backup the last update left next to this binary. No
    /// network, no version resolution - the backup is the rollback. Also
    /// restarts any systemd units that were stopped during the update.
    fn cmd_rollback() {
        let current_exe = std::env::current_exe().unwrap_or_else(|e| {
            eprintln!("rollback: cannot locate the running binary ({e})");
            std::process::exit(1);
        });
        let backup = current_exe.with_extension("prev");
        if !backup.is_file() {
            eprintln!(
                "rollback: no backup at {} (updates keep one; none has run yet)",
                backup.display()
            );
            std::process::exit(1);
        }
        if std::fs::copy(&backup, &current_exe).is_err() {
            eprintln!(
                "rollback: cannot restore {} over {}",
                backup.display(),
                current_exe.display()
            );
            std::process::exit(1);
        }
        println!("✓ Rolled back to {}", backup.display());
        println!("  Binary: {}", current_exe.display());
        // Roll the helper back too: the update backs it up to
        // `pantheon-helper.prev` before swapping. A rollback that restores
        // only the main binary leaves a new helper driving an old main.
        // No helper backup (updates from before this fix) is a note, not
        // a failure; a failed helper restore is a warning, since the main
        // binary - the thing that runs - is restored and the helper
        // self-heals on the next update.
        let helper_exe = current_exe.with_file_name("pantheon-helper");
        let helper_backup = helper_exe.with_extension("prev");
        if helper_backup.is_file() {
            match std::fs::copy(&helper_backup, &helper_exe) {
                Ok(_) => println!("✓ Rolled helper back to {}", helper_backup.display()),
                Err(e) => eprintln!("rollback: ⚠ could not restore helper ({e})"),
            }
        }
        // Restart any systemd units that were stopped during the update.
        // The state file is removed only when every restart succeeded:
        // deleting it after a failed restart would lose the only record
        // of which units are still down.
        let state_path = current_exe.with_extension("update-state");
        if state_path.is_file() {
            let mut restarts_ok = true;
            if let Ok(text) = std::fs::read_to_string(&state_path) {
                for unit in text.lines() {
                    if !unit.trim().is_empty() {
                        if restart_systemd_unit_ok(unit.trim()) {
                            println!("  Restarted {}", unit.trim());
                        } else {
                            eprintln!(
                                "rollback: ⚠ could not restart {}; restart it manually",
                                unit.trim()
                            );
                            restarts_ok = false;
                        }
                    }
                }
            }
            if restarts_ok {
                let _ = std::fs::remove_file(&state_path);
            } else {
                eprintln!(
                    "rollback: ⚠ state file kept at {} for a retry",
                    state_path.display()
                );
            }
        }
    }

    pub fn cmd_update(args: &[String]) {
        if args.iter().any(|a| a == "--help" || a == "-h") {
            println!("{}", usage());
            return;
        }
        // Unknown flags used to fall through to a network fetch: `update
        // --bogus` cheerfully replaced the binary while ignoring whatever
        // the operator thought they had asked for.
        // --version/--repo take a value (`--version v1.2.3` or
        // `--version=v1.2.3`, as `flag_val` reads them), so the value
        // token is consumed here, not mistaken for a stray positional.
        // --check/--rollback are boolean switches.
        let rest: &[String] = args.get(2..).unwrap_or(&[]);
        let mut i = 0;
        while i < rest.len() {
            let a = &rest[i];
            if a.starts_with("--") {
                let name = a.split('=').next().unwrap_or(a.as_str());
                if !matches!(name, "--check" | "--version" | "--repo" | "--rollback") {
                    eprintln!("update: unknown flag '{a}'");
                    eprintln!("{}", usage());
                    std::process::exit(2);
                }
                if matches!(name, "--version" | "--repo") && !a.contains('=') {
                    match rest.get(i + 1) {
                        Some(v) if !v.starts_with("--") => i += 1,
                        _ => {
                            eprintln!("update: {name} needs a value");
                            eprintln!("{}", usage());
                            std::process::exit(2);
                        }
                    }
                }
            } else if a.starts_with('-') {
                eprintln!("update: unknown flag '{a}'");
                eprintln!("{}", usage());
                std::process::exit(2);
            } else {
                eprintln!("update: unexpected argument '{a}'");
                eprintln!("{}", usage());
                std::process::exit(2);
            }
            i += 1;
        }
        let flag_val = |name: &str| {
            let mut it = args.iter().peekable();
            while let Some(a) = it.next() {
                if a == name {
                    return it.next().cloned();
                }
                if let Some(v) = a.strip_prefix(&format!("{name}=")) {
                    return Some(v.to_string());
                }
            }
            None
        };
        let check_only = args.iter().any(|a| a == "--check");
        if args.iter().any(|a| a == "--rollback") {
            return cmd_rollback();
        }
        let repo = flag_val("--repo").unwrap_or_else(repo);
        let os = match os_id() {
            Some(o) => o,
            None => {
                eprintln!(
                    "update: unsupported OS {} - download a release by hand from https://github.com/{repo}/releases",
                    std::env::consts::OS
                );
                std::process::exit(1);
            }
        };
        let arch = match arch_id() {
            Some(a) => a,
            None => {
                eprintln!(
                    "update: unsupported architecture {} - build from source instead",
                    std::env::consts::ARCH
                );
                std::process::exit(1);
            }
        };

        let tag = match flag_val("--version") {
            Some(v) => v,
            None => resolve_latest_tag(&repo).unwrap_or_else(|e| {
                eprintln!("update: {e}");
                std::process::exit(1);
            }),
        };

        let current = current_version();
        if !is_newer(&current, &tag) && flag_val("--version").is_none() {
            println!("pantheon {current} is already the latest ({tag})");
            return;
        }

        if check_only {
            if is_newer(&current, &tag) {
                println!("update available: pantheon {current} → {tag}");
                println!("run `pantheon update` to install it");
            } else {
                println!("pantheon {current} is already the latest ({tag})");
            }
            return;
        }

        // Handoff pattern: spawn pantheon-helper detached, then exit.
        // The helper does the actual download, verify, swap, and restart.
        // This lets even old binaries self-update, because the helper survives
        // the binary swap (it has its own copy in memory).
        let current_exe = std::env::current_exe().unwrap_or_else(|e| {
            eprintln!("update: install failed: cannot locate the running binary ({e})");
            std::process::exit(1);
        });
        let helper_exe = current_exe.with_file_name("pantheon-helper");

        // Download the helper binary if it's missing or outdated.
        if !helper_exe.is_file() {
            println!("· Downloading pantheon-helper");
            let helper_asset = asset_name(&tag, os, arch).replace("pantheon-", "pantheon-helper-");
            let helper_url = download_url(&repo, &tag, &helper_asset);
            let helper_bytes = match download_bytes(&helper_url) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("update: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = require_verified(verify_checksum(&helper_url, &helper_bytes)) {
                eprintln!("update: {e}");
                std::process::exit(1);
            }
            println!("✓ Checksum matches (corruption check only — not authentication)");
            if let Err(e) = verify_signature(&helper_url, &helper_bytes) {
                eprintln!("update: {e}");
                std::process::exit(1);
            }
            println!("✓ Signature verified (minisign, pinned release key)");
            let tmp = std::env::temp_dir().join(format!("pantheon-helper-{}", std::process::id()));
            let helper_archive = tmp.join(&helper_asset);
            let _ = std::fs::create_dir_all(&tmp);
            let _ = std::fs::write(&helper_archive, &helper_bytes);
            let extracted = match extract_with_tar(&helper_archive, &tmp) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("update: {e}");
                    std::process::exit(1);
                }
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(md) = std::fs::metadata(&extracted) {
                    let mut perm = md.permissions();
                    perm.set_mode(0o755);
                    let _ = std::fs::set_permissions(&extracted, perm);
                }
            }
            if let Err(e) = std::fs::rename(&extracted, &helper_exe) {
                eprintln!("update: cannot install helper ({e})");
                std::process::exit(1);
            }
            let _ = std::fs::remove_dir_all(&tmp);
        }

        // Spawn the helper detached and exit.
        println!("· Spawning update helper");
        let mut cmd = std::process::Command::new(&helper_exe);
        cmd.arg("update")
            .arg("--current")
            .arg(&current_exe)
            .arg("--tag")
            .arg(&tag)
            .arg("--repo")
            .arg(&repo);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                let _ = child.wait();
                // Exit so the helper can swap the binary.
                println!("✓ Update in progress (helper running)");
            }
            Err(e) => {
                eprintln!("update: cannot spawn helper ({e})");
                std::process::exit(1);
            }
        }
    }

    /// Restart a systemd unit (Unix) or Windows service, reporting
    /// success so rollback can keep the state file when a restart fails
    /// instead of deleting the only record of which units are still down.
    fn restart_systemd_unit_ok(unit: &str) -> bool {
        #[cfg(unix)]
        {
            std::process::Command::new("systemctl")
                .args(["--user", "start", unit])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(windows)]
        {
            std::process::Command::new("sc")
                .args(["start", unit])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::require_verified;

        #[test]
        fn state_file_round_trips_units_for_rollback() {
            let exe = std::env::current_exe().unwrap();
            let state_path = exe.with_extension("update-state");
            let units = ["pantheon-gateway.service".to_string()];
            std::fs::write(&state_path, units.join("\n")).unwrap();
            let text = std::fs::read_to_string(&state_path).unwrap();
            let parsed: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            assert_eq!(parsed, vec!["pantheon-gateway.service"]);
            let _ = std::fs::remove_file(&state_path);
        }

        #[test]
        fn verification_gate_rejects_digest_less_and_failed_checks() {
            // Only a digest match installs. A checksum file with no usable
            // digest used to pass through as Ok(false) in the helper
            // download path; require_verified closes that hole.
            assert!(require_verified(Ok(true)).is_ok());
            assert!(require_verified(Ok(false)).is_err());
            assert_eq!(
                require_verified(Err("verify failed: checksum mismatch".to_string())).unwrap_err(),
                "verify failed: checksum mismatch"
            );
        }

        #[test]
        fn find_running_excludes_self() {
            let pids = find_running_pantheon_processes();
            assert!(!pids.contains(&std::process::id()));
        }

        #[cfg(test)]
        fn find_running_pantheon_processes() -> Vec<u32> {
            let current_pid = std::process::id();
            let current_exe = std::env::current_exe().unwrap_or_default();
            let exe_name = current_exe
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("pantheon")
                .to_string();
            let mut pids = Vec::new();

            #[cfg(unix)]
            {
                if let Ok(out) = std::process::Command::new("pgrep")
                    .arg("-x")
                    .arg(&exe_name)
                    .output()
                {
                    if out.status.success() {
                        let text = String::from_utf8_lossy(&out.stdout);
                        for line in text.lines() {
                            if let Ok(pid) = line.trim().parse::<u32>() {
                                if pid != current_pid {
                                    pids.push(pid);
                                }
                            }
                        }
                    }
                }
            }

            #[cfg(windows)]
            {
                if let Ok(out) = std::process::Command::new("tasklist")
                    .args([
                        "/FI",
                        &format!("IMAGENAME eq {exe_name}"),
                        "/NH",
                        "/FO",
                        "CSV",
                    ])
                    .output()
                {
                    if out.status.success() {
                        let text = String::from_utf8_lossy(&out.stdout);
                        for line in text.lines() {
                            let parts: Vec<&str> = line.split(',').collect();
                            if parts.len() >= 2 {
                                if let Ok(pid) = parts[1].trim().parse::<u32>() {
                                    if pid != current_pid {
                                        pids.push(pid);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            pids
        }

        #[test]
        fn permission_mode_resolve_behavior_verified() {
            use pantheon_api::capability::Decision;
            use pantheon_api::model::GateVerdict;
            use pantheon_api::permission_mode::PermissionMode;
            for mode in [
                PermissionMode::Ask,
                PermissionMode::Smart,
                PermissionMode::AllowAll,
            ] {
                assert_eq!(
                    mode.resolve(Decision::Deny, Some(GateVerdict::Allow)),
                    Decision::Deny
                );
            }
            assert_eq!(
                PermissionMode::Ask.resolve(Decision::Approval, None),
                Decision::Approval
            );
            assert_eq!(
                PermissionMode::AllowAll.resolve(Decision::Approval, None),
                Decision::Allow
            );
        }
    }
}

pub mod worktree {
    //! `pantheon worktree`: first-class git worktrees for parallel work.
    //!
    //! Managed worktrees live under `<data_dir>/worktrees/<repo>-<hash>/`
    //! (not inside the repository), so a worktree never becomes part of the
    //! checkout it was created from. Branches are created in the
    //! `pantheon/<name>` namespace. `path` prints the managed path so a
    //! shell can `cd "$(pantheon worktree path review)"`; `create` also
    //! accepts `--path` for an explicit location.

    use crate::terminal::data_dir;
    use pantheon_exec::worktree::WorktreeManager;
    use std::path::PathBuf;

    pub fn cmd_worktree(args: &[String]) {
        let verb = args.first().map(|s| s.as_str()).unwrap_or("list");
        if matches!(verb, "help" | "--help" | "-h") {
            help();
            return;
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let dd = data_dir();
        let manager = match WorktreeManager::new(&cwd, &dd) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("worktree: {e}");
                std::process::exit(1);
            }
        };
        match verb {
            "list" | "" => list(&manager, args),
            "create" => create(&manager, args),
            "remove" | "rm" => remove(&manager, args),
            "path" => paths(&manager, args),
            "enter" => enter(&manager, args),
            "root" => println!("{}", manager.root().display()),
            other => {
                eprintln!("worktree: unknown verb '{other}'");
                eprintln!(
                    "  usage: pantheon workspace worktree <list|create|remove|path|enter|root>"
                );
                std::process::exit(2);
            }
        }
    }

    fn help() {
        println!(concat!(
            "pantheon workspace worktree <command> [args]\n\n",
            "Commands:\n",
            "  list [--json]                  list main and linked worktrees\n",
            "  create <name> [--base REF] [--path DIR]\n",
            "                                  create pantheon/<name> and check it out\n",
            "  remove <name|path> [--force]   remove a linked worktree\n",
            "  path <name>                    print the worktree path\n",
            "  enter <name> [-- args...]      launch a session rooted in the worktree\n",
            "  root                           print the managed worktree root"
        ));
    }

    fn has_json(args: &[String]) -> bool {
        args.iter().any(|a| a == "--json")
    }

    fn list(manager: &WorktreeManager, args: &[String]) {
        match manager.list() {
            Ok(items) => {
                if has_json(args) {
                    match serde_json::to_string_pretty(&items) {
                        Ok(s) => println!("{s}"),
                        Err(e) => eprintln!("worktree: encode failed: {e}"),
                    }
                    return;
                }
                if items.is_empty() {
                    println!("no worktrees");
                    return;
                }
                for w in items {
                    let branch = w.branch.as_deref().unwrap_or("(detached)");
                    let tag = if w.main { " main" } else { "" };
                    println!(
                        "{:<8} {:<32} {}{}",
                        w.head.chars().take(8).collect::<String>(),
                        branch,
                        w.path.display(),
                        tag
                    );
                    if w.locked {
                        println!("  (locked)");
                    }
                    if w.prunable {
                        println!("  (prunable)");
                    }
                }
            }
            Err(e) => {
                eprintln!("worktree: list failed: {e}");
                std::process::exit(1);
            }
        }
    }

    fn need_name(args: &[String], verb: &str) -> String {
        match args.get(1) {
            Some(n) if !n.starts_with('-') => n.clone(),
            _ => {
                eprintln!("worktree {verb}: needs a name");
                std::process::exit(2);
            }
        }
    }

    fn create(manager: &WorktreeManager, args: &[String]) {
        let name = need_name(args, "create");
        let mut base: Option<&str> = None;
        let mut path: Option<PathBuf> = None;
        let mut i = 2;
        while i < args.len() {
            match args[i].as_str() {
                "--base" => {
                    base = args.get(i + 1).map(String::as_str);
                    if base.is_none() {
                        eprintln!("worktree create: --base needs a revision");
                        std::process::exit(2);
                    }
                    i += 2;
                }
                "--path" => {
                    path = args.get(i + 1).map(PathBuf::from);
                    if path.is_none() {
                        eprintln!("worktree create: --path needs a directory");
                        std::process::exit(2);
                    }
                    i += 2;
                }
                "--json" => i += 1,
                other => {
                    eprintln!("worktree create: unknown option '{other}'");
                    eprintln!("  usage: pantheon worktree create <name> [--base REV] [--path DIR]");
                    std::process::exit(2);
                }
            }
        }
        match manager.create(&name, base, path.as_deref()) {
            Ok(w) => {
                if has_json(args) {
                    match serde_json::to_string_pretty(&w) {
                        Ok(s) => println!("{s}"),
                        Err(e) => eprintln!("worktree: encode failed: {e}"),
                    }
                } else {
                    println!("created {}", w.path.display());
                    if let Some(b) = w.branch {
                        println!("branch  {b}");
                    }
                    println!("next    cd \"$(pantheon worktree path {name})\"");
                }
            }
            Err(e) => {
                eprintln!("worktree: create failed: {e}");
                std::process::exit(1);
            }
        }
    }

    fn remove(manager: &WorktreeManager, args: &[String]) {
        let target = need_name(args, "remove");
        let force = args.iter().any(|a| a == "--force" || a == "-f");
        match manager.remove(&target, force) {
            Ok(w) => println!("removed {}", w.path.display()),
            Err(e) => {
                eprintln!("worktree: remove failed: {e}");
                std::process::exit(1);
            }
        }
    }

    /// `enter <name> [-- <args>...]`: launch a fresh Pantheon session with
    /// the worktree as its process cwd. Process cwd is Pantheon's workspace
    /// boundary, so this is session relocation without mutating the running
    /// process or silently changing another session's relative paths.
    fn enter(manager: &WorktreeManager, args: &[String]) {
        let name = need_name(args, "enter");
        let path = match manager.find(&name) {
            Ok(Some(w)) => w.path,
            Ok(None) => match manager.path_for(&name) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("worktree: {e}");
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("worktree: {e}");
                std::process::exit(1);
            }
        };
        if !path.is_dir() {
            eprintln!(
                "worktree: {} does not exist; create it with `pantheon worktree create {}`",
                path.display(),
                name
            );
            std::process::exit(1);
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("worktree: locate current executable: {e}");
                std::process::exit(1);
            }
        };
        println!("entering {}", path.display());
        let mut cmd = std::process::Command::new(exe);
        cmd.current_dir(&path);
        if let Some(pos) = args.iter().position(|a| a == "--") {
            cmd.args(&args[pos + 1..]);
        }
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(e) => {
                eprintln!("worktree: launch session: {e}");
                std::process::exit(1);
            }
        }
    }

    fn paths(manager: &WorktreeManager, args: &[String]) {
        let name = need_name(args, "path");
        let path = match manager.find(&name) {
            Ok(Some(w)) => Ok(w.path),
            Ok(None) => manager.path_for(&name),
            Err(e) => Err(e),
        };
        match path {
            Ok(p) => println!("{}", p.display()),
            Err(e) => {
                eprintln!("worktree: {e}");
                std::process::exit(1);
            }
        }
    }
}

pub mod richtext {
    //! Rich transcript rendering: inline images, mermaid diagrams, LaTeX math.
    //!
    //! The transcript is line-oriented ([`crate::session::render_block`] builds
    //! `Vec<Line>`), so every feature here degrades to styled text lines. The
    //! actual terminal graphics (Kitty/Sixel) are emitted post-draw by
    //! [`ImagePaintState::paint`]: ratatui owns the cells, graphics are placed
    //! absolutely, and the two must never interleave inside one draw.
    //!
    //! Everything in the `*_impl` sections is pure and covered from `pantheon-eval`;
    //! only `paint`/`clear` touch the terminal.

    use std::io::Write;
    use std::path::{Path, PathBuf};

    // ===========================================================================
    // Image references
    // ===========================================================================

    /// An image file referenced from a transcript message.
    #[derive(Debug, Clone)]
    pub struct ImageRef {
        /// Resolved absolute path.
        pub path: PathBuf,
        /// File name for display.
        pub name: String,
        pub width: Option<u32>,
        pub height: Option<u32>,
    }

    const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

    fn is_image_path(s: &str) -> bool {
        let lower = s.to_ascii_lowercase();
        IMAGE_EXTS.iter().any(|e| lower.ends_with(e))
    }

    /// Scan message text for image file references: `@path`, markdown
    /// `![alt](path)`, and bare path tokens. Only files that exist on disk are
    /// returned, resolved to absolute paths, deduplicated in first-seen order.
    pub fn find_images(text: &str) -> Vec<ImageRef> {
        let mut out: Vec<ImageRef> = Vec::new();
        let mut seen: Vec<PathBuf> = Vec::new();
        let mut push = |raw: &str| {
            let raw = raw.trim().trim_matches(|c| c == '"' || c == '\'');
            if raw.is_empty() || !is_image_path(raw) {
                return;
            }
            // Expand a leading ~ the way shells do; leave everything else alone.
            let expanded = if let Some(rest) = raw.strip_prefix("~/") {
                match std::env::var("HOME") {
                    Ok(h) => format!("{h}/{rest}"),
                    Err(_) => raw.to_string(),
                }
            } else {
                raw.to_string()
            };
            let path = PathBuf::from(&expanded);
            if !path.is_file() {
                return;
            }
            let abs = std::fs::canonicalize(&path).unwrap_or(path);
            if seen.contains(&abs) {
                return;
            }
            seen.push(abs.clone());
            let (width, height) = image_dimensions(&abs).unwrap_or((None, None));
            let (width, height) = (width, height);
            out.push(ImageRef {
                name: abs
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| raw.to_string()),
                path: abs,
                width,
                height,
            });
        };

        // Markdown images first: ![alt](path).
        let mut rest = text;
        while let Some(bang) = rest.find("![") {
            let after = &rest[bang + 2..];
            if let Some(paren) = after.find("](") {
                let url = &after[paren + 2..];
                if let Some(end) = url.find(')') {
                    push(&url[..end]);
                    rest = &url[end + 1..];
                    continue;
                }
            }
            rest = after;
        }
        // @path and bare tokens.
        for tok in text.split_whitespace() {
            let tok = tok.trim_matches(|c: char| ".,;:!?()[]{}".contains(c));
            let tok = tok.strip_prefix('@').unwrap_or(tok);
            push(tok);
        }
        out
    }

    fn read_u32_be(b: &[u8]) -> u32 {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }

    /// Image dimensions without decoding pixels: PNG (IHDR), JPEG (SOF), GIF
    /// (logical screen descriptor). Returns `(width, height)` options so a
    /// caller can still name a file it cannot measure.
    pub fn image_dimensions(path: &Path) -> Option<(Option<u32>, Option<u32>)> {
        let bytes = std::fs::read(path).ok()?;
        // PNG: 8-byte signature, then IHDR width/height at offset 16.
        if bytes.len() > 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR"
        {
            return Some((
                Some(read_u32_be(&bytes[16..20])),
                Some(read_u32_be(&bytes[20..24])),
            ));
        }
        // GIF: "GIF8" then width/height u16 LE at offset 6.
        if bytes.len() > 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
            let w = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
            let h = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
            return Some((Some(w), Some(h)));
        }
        // JPEG: scan markers for a Start-Of-Frame segment.
        if bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
            let mut i = 2;
            while i + 4 < bytes.len() {
                if bytes[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = bytes[i + 1];
                i += 2;
                if marker == 0xD8
                    || marker == 0xD9
                    || (0xD0..=0xD7).contains(&marker)
                    || marker == 0x01
                {
                    continue;
                }
                if i + 2 > bytes.len() {
                    break;
                }
                let len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
                if (0xC0..=0xC3).contains(&marker) && i + 7 < bytes.len() {
                    let h = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as u32;
                    let w = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                    return Some((Some(w), Some(h)));
                }
                i += len;
            }
        }
        None
    }

    // ===========================================================================
    // Graphics support detection
    // ===========================================================================

    /// Terminal inline-graphics capability, probed once at startup.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum GraphicsSupport {
        /// Kitty graphics protocol (also WezTerm, Ghostty, Konsole 23+).
        Kitty,
        /// Sixel (mlterm, foot with sixel, xterm -ti 340, iTerm2...).
        Sixel,
        /// No inline graphics: transcript shows a text placeholder.
        #[default]
        None,
    }

    impl GraphicsSupport {
        /// Probe once. Kitty advertises `KITTY_WINDOW_ID`; Sixel has no query
        /// that works everywhere, so it is opt-in via `PANTHEON_SIXEL=1` or a
        /// TERM that names it. Never blocks: no terminal queries are sent.
        pub fn detect() -> Self {
            if std::env::var_os("KITTY_WINDOW_ID").is_some()
                || std::env::var("TERM_PROGRAM")
                    .map(|t| t.eq_ignore_ascii_case("wezterm") || t.eq_ignore_ascii_case("ghostty"))
                    .unwrap_or(false)
            {
                return Self::Kitty;
            }
            if std::env::var("PANTHEON_SIXEL")
                .map(|v| v == "1")
                .unwrap_or(false)
                || std::env::var("TERM")
                    .map(|t| t.to_ascii_lowercase().contains("sixel") || t == "mlterm")
                    .unwrap_or(false)
            {
                return Self::Sixel;
            }
            Self::None
        }
    }

    // ===========================================================================
    // Kitty graphics protocol
    // ===========================================================================

    const ESC: &str = "\x1b";
    const APC_START: &str = "\x1b_G";
    const APC_END: &str = "\x1b\\";

    fn b64(data: &[u8]) -> String {
        const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(data.len() * 4 / 3 + 4);
        for chunk in data.chunks(3) {
            let mut n: u32 = 0;
            for (i, &b) in chunk.iter().enumerate() {
                n |= (b as u32) << (16 - 8 * i);
            }
            let pad = 3 - chunk.len();
            for i in 0..4 - pad {
                out.push(ALPH[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
            for _ in 0..pad {
                out.push('=');
            }
        }
        out
    }

    /// Build the byte stream that transmits a PNG (`f=100`: the terminal takes
    /// PNG directly, no pixel decoding needed) and displays it `cols` cells
    /// wide at image id `id`. Data is chunked at 4096 base64 chars per the
    /// protocol; the first chunk carries the control payload.
    pub fn kitty_image_payload(png: &[u8], id: u32, cols: u16) -> Vec<u8> {
        let encoded = b64(png);
        let chunks: Vec<&str> = encoded
            .as_bytes()
            .chunks(4096)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect();
        let mut out = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let last = i + 1 == chunks.len();
            if i == 0 {
                out.extend_from_slice(
                    format!(
                        "{APC_START}a=T,f=100,i={id},c={cols},m={};",
                        i32::from(!last)
                    )
                    .as_bytes(),
                );
            } else {
                out.extend_from_slice(format!("{APC_START}m={};", i32::from(!last)).as_bytes());
            }
            out.extend_from_slice(chunk.as_bytes());
            out.extend_from_slice(APC_END.as_bytes());
        }
        if chunks.is_empty() {
            out.extend_from_slice(
                format!("{APC_START}a=T,f=100,i={id},c={cols},m=0;{APC_END}").as_bytes(),
            );
        }
        out
    }

    /// Delete one placed image by id (`a=d,d=I`).
    pub fn kitty_delete(id: u32) -> Vec<u8> {
        format!("{APC_START}a=d,d=I,i={id}{APC_END}").into_bytes()
    }

    // ===========================================================================
    // Sixel
    // ===========================================================================

    /// Decode a PNG to raw RGB. Minimal on purpose: 8-bit RGB/RGBA,
    /// non-interlaced only - enough for screenshots and pasted images, which is
    /// what the transcript ever shows. Returns `(rgb, width, height)`.
    pub fn png_to_rgb(png: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return None;
        }
        let mut width = 0u32;
        let mut height = 0u32;
        let mut bit_depth = 0u8;
        let mut color_type = 0u8;
        let mut interlace = 1u8;
        let mut idat: Vec<u8> = Vec::new();
        let mut i = 8;
        while i + 8 <= png.len() {
            let len = read_u32_be(&png[i..]) as usize;
            let kind = &png[i + 4..i + 8];
            let data = png.get(i + 8..i + 8 + len)?;
            match kind {
                b"IHDR" => {
                    width = read_u32_be(&data[0..]);
                    height = read_u32_be(&data[4..]);
                    bit_depth = data[8];
                    color_type = data[9];
                    interlace = data[12];
                }
                b"IDAT" => idat.extend_from_slice(data),
                b"IEND" => break,
                _ => {}
            }
            i += 12 + len;
        }
        if width == 0 || height == 0 || bit_depth != 8 || interlace != 0 {
            return None;
        }
        let channels: usize = match color_type {
            2 => 3, // RGB
            6 => 4, // RGBA
            _ => return None,
        };
        // Inflate the zlib stream (2-byte header + Adler32 trailer).
        if idat.len() < 6 {
            return None;
        }
        let mut decoder = flate2::Decompress::new(false);
        let mut raw = vec![0u8; (width as usize) * (height as usize) * channels + height as usize];
        let status = decoder
            .decompress(&idat[2..], &mut raw, flate2::FlushDecompress::Finish)
            .ok()?;
        let out_len = decoder.total_out() as usize;
        let _ = status;
        let stride = width as usize * channels;
        if out_len < (stride + 1) * height as usize {
            return None;
        }
        // Unfilter scanlines (None/Sub/Up/Average/Paeth).
        let bpp = channels;
        let mut rgb = vec![0u8; width as usize * height as usize * 3];
        let mut prev = vec![0u8; stride];
        for y in 0..height as usize {
            let row_start = y * (stride + 1);
            let filter = raw[row_start];
            let cur = &raw[row_start + 1..row_start + 1 + stride];
            let mut recon = vec![0u8; stride];
            for x in 0..stride {
                let a = if x >= bpp { recon[x - bpp] } else { 0 };
                let b = prev[x];
                let c = if x >= bpp { prev[x - bpp] } else { 0 };
                let filt = cur[x];
                recon[x] = match filter {
                    0 => filt,
                    1 => filt.wrapping_add(a),
                    2 => filt.wrapping_add(b),
                    3 => filt.wrapping_add(((a as u16 + b as u16) / 2) as u8),
                    4 => {
                        let p = a as i16 + b as i16 - c as i16;
                        let pa = (p - a as i16).abs();
                        let pb = (p - b as i16).abs();
                        let pc = (p - c as i16).abs();
                        let pr = if pa <= pb && pa <= pc {
                            a
                        } else if pb <= pc {
                            b
                        } else {
                            c
                        };
                        filt.wrapping_add(pr)
                    }
                    _ => return None,
                };
            }
            for x in 0..width as usize {
                let dst = (y * width as usize + x) * 3;
                rgb[dst] = recon[x * channels];
                rgb[dst + 1] = recon[x * channels + 1];
                rgb[dst + 2] = recon[x * channels + 2];
            }
            prev = recon;
        }
        Some((rgb, width, height))
    }

    /// Nearest-neighbor downscale of RGB data to fit within `max_w`.
    fn downscale_rgb(rgb: &[u8], w: u32, h: u32, max_w: u32) -> (Vec<u8>, u32, u32) {
        if w <= max_w {
            return (rgb.to_vec(), w, h);
        }
        let nw = max_w;
        let nh = ((h as f64 * max_w as f64 / w as f64).round() as u32).max(1);
        let mut out = vec![0u8; (nw * nh * 3) as usize];
        for y in 0..nh {
            for x in 0..nw {
                let sx = (x as f64 * w as f64 / nw as f64) as u32;
                let sy = (y as f64 * h as f64 / nh as f64) as u32;
                let s = ((sy * w + sx) * 3) as usize;
                let d = ((y * nw + x) * 3) as usize;
                out[d..d + 3].copy_from_slice(&rgb[s..s + 3]);
            }
        }
        (out, nw, nh)
    }

    /// Encode raw RGB as Sixel. Palette is uniform 3-3-2 (256 entries max, only
    /// the used ones are defined); each band is 6 pixel rows tall.
    pub fn encode_sixel(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
        let w = w as usize;
        let h = h as usize;
        let quant = |r: u8, g: u8, b: u8| -> usize {
            ((r >> 5) as usize) << 5 | ((g >> 5) as usize) << 2 | ((b >> 6) as usize)
        };
        let mut used = [false; 256];
        for px in rgb.as_chunks::<3>().0 {
            used[quant(px[0], px[1], px[2])] = true;
        }
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(format!("{ESC}Pq\"1;1;{w};{h}").as_bytes());
        for (i, u) in used.iter().enumerate() {
            if *u {
                let r = (((i >> 5) & 7) * 100 / 7) as u8;
                let g = (((i >> 2) & 7) * 100 / 7) as u8;
                let b = ((i & 3) * 100 / 3) as u8;
                out.extend_from_slice(format!("#{i};2;{r};{g};{b}").as_bytes());
            }
        }
        let bands = h.div_ceil(6);
        for band in 0..bands {
            for (i, u) in used.iter().enumerate() {
                if !*u {
                    continue;
                }
                // Collect the sixel column bytes for this color in this band.
                let mut cols: Vec<u8> = Vec::with_capacity(w);
                for x in 0..w {
                    let mut bits = 0u8;
                    for row in 0..6 {
                        let y = band * 6 + row;
                        if y >= h {
                            break;
                        }
                        let px = &rgb[(y * w + x) * 3..][..3];
                        if quant(px[0], px[1], px[2]) == i {
                            bits |= 1 << row;
                        }
                    }
                    cols.push(bits + 63);
                }
                if cols.iter().all(|&c| c == 63) {
                    continue; // color absent in this band
                }
                out.extend_from_slice(format!("#{i}").as_bytes());
                // Run-length encode repeats.
                let mut j = 0;
                while j < cols.len() {
                    let mut k = j + 1;
                    while k < cols.len() && cols[k] == cols[j] && k - j < 255 {
                        k += 1;
                    }
                    if k - j > 3 {
                        out.extend_from_slice(format!("!{}{}", k - j, cols[j] as char).as_bytes());
                    } else {
                        out.extend_from_slice(&cols[j..k]);
                    }
                    j = k;
                }
                out.push(b'$');
            }
            out.push(b'-');
        }
        out.extend_from_slice(format!("{ESC}\\").as_bytes());
        out
    }

    // ===========================================================================

    // ===========================================================================
    // Mermaid flowcharts → Unicode box diagrams
    // ===========================================================================

    #[derive(Debug, Clone)]
    struct MermaidNode {
        label: String,
    }

    #[derive(Debug, Clone)]
    struct MermaidEdge {
        from: String,
        to: String,
    }

    /// Common subset: `graph TD` / `graph LR` (`flowchart` alias accepted),
    /// `A-->B`, `A --> B`, `A---B`, `A==>B`, `A-.->B`,
    /// and node labels `A[text]`, `A{text}`, `A((text))`, `A([text])`, `A[[text]]`.
    /// Edge labels (`A-->|label|B`) are NOT supported: a labeled edge makes the
    /// whole block return `None` and the caller shows the raw fence instead of
    /// a diagram that silently drops the label.
    /// Anything outside the subset makes the whole block return `None` and the
    /// caller shows the raw fence instead of a wrong diagram.
    pub fn render_mermaid(src: &str) -> Option<Vec<String>> {
        let mut lines = src.lines().map(str::trim).filter(|l| !l.is_empty());
        let first = lines.next()?;
        let vertical = if first.starts_with("graph TD") || first.starts_with("flowchart TD") {
            true
        } else if first.starts_with("graph LR") || first.starts_with("flowchart LR") {
            false
        } else {
            return None;
        };

        let mut nodes: Vec<(String, MermaidNode)> = Vec::new();
        let mut edges: Vec<MermaidEdge> = Vec::new();

        fn node_id(s: &str) -> Option<String> {
            let s = s.trim();
            let end = s
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(s.len());
            let id = &s[..end];
            if id.is_empty() || !id.chars().next().is_some_and(|c| c.is_alphabetic()) {
                return None;
            }
            Some(id.to_string())
        }
        fn ensure(nodes: &mut Vec<(String, MermaidNode)>, id: &str) {
            if !nodes.iter().any(|(n, _)| n == id) {
                nodes.push((
                    id.to_string(),
                    MermaidNode {
                        label: id.to_string(),
                    },
                ));
            }
        }
        // Parse `A[label]` / `A{text}` / `A((text))` / `A([text])` / `A[[text]]`
        // or a bare `A` (label defaults to the id).
        fn parse_node_def(s: &str, nodes: &mut Vec<(String, MermaidNode)>) -> Option<String> {
            let s = s.trim();
            let id = node_id(s)?;
            let rest = s[id.len()..].trim_start();
            if rest.is_empty() {
                ensure(nodes, &id);
                return Some(id);
            }
            let (open, close) = if rest.starts_with("((") {
                ("((", "))")
            } else if rest.starts_with("[[") {
                ("[[", "]]")
            } else if rest.starts_with("([") {
                ("([", "])")
            } else if rest.starts_with('[') {
                ("[", "]")
            } else if rest.starts_with('{') {
                ("{", "}")
            } else if rest.starts_with('(') {
                ("(", ")")
            } else {
                return None;
            };
            let inner = rest[open.len()..].strip_suffix(close)?;
            if let Some(entry) = nodes.iter_mut().find(|(n, _)| n == &id) {
                entry.1.label = inner.trim().to_string();
            } else {
                nodes.push((
                    id.clone(),
                    MermaidNode {
                        label: inner.trim().to_string(),
                    },
                ));
            }
            Some(id)
        }

        for line in lines {
            let line = line.trim_end_matches(';').trim();
            if line.starts_with("%%") {
                continue;
            }
            // Edge operators, longest first.
            let mut op_at: Option<(usize, &str)> = None;
            for op in ["-->", "==>", "---", "-.->"] {
                if let Some(p) = line.find(op) {
                    op_at = Some((p, op));
                    break;
                }
            }
            if let Some((p, op)) = op_at {
                let left = line[..p].trim();
                let mut right = line[p + op.len()..].trim();
                // Strip an edge label: `A-->|yes|B` - the label is metadata,
                // not part of the target node.
                if let Some(stripped) = right.strip_prefix('|') {
                    if let Some(end) = stripped.find('|') {
                        right = stripped[end + 1..].trim();
                    }
                }
                // Each side may itself carry a node def: `A[x]-->B[y]`.
                let from = parse_node_def(left, &mut nodes)?;
                let to = parse_node_def(right, &mut nodes)?;
                edges.push(MermaidEdge { from, to });
                continue;
            }
            // Bare node definition line.
            parse_node_def(line, &mut nodes)?;
        }
        if nodes.is_empty() {
            return None;
        }

        // Longest-path layering: layer 0 has no incoming edges.
        let mut layer_of: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (id, _) in &nodes {
            layer_of.insert(id.as_str(), 0);
        }
        for _ in 0..nodes.len() + 1 {
            let mut changed = false;
            for e in &edges {
                let fl = layer_of[e.from.as_str()];
                let tl = layer_of[e.to.as_str()];
                if tl < fl + 1 {
                    layer_of.insert(e.to.as_str(), fl + 1);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let max_layer = layer_of.values().copied().max().unwrap_or(0);
        let mut layers: Vec<Vec<&str>> = vec![Vec::new(); max_layer + 1];
        for (id, _) in &nodes {
            layers[layer_of[id.as_str()]].push(id.as_str());
        }
        let labels: std::collections::HashMap<&str, &str> = nodes
            .iter()
            .map(|(id, n)| (id.as_str(), n.label.as_str()))
            .collect();

        if vertical {
            render_td(&layers, &edges, &labels)
        } else {
            render_lr(&layers, &edges, &labels)
        }
    }

    /// Box width for a label: label chars + padding, minimum 6.
    fn mbox_w(label: &str) -> usize {
        (label.chars().count() + 4).max(6)
    }

    fn mlabel<'a>(labels: &'a std::collections::HashMap<&str, &str>, id: &'a str) -> &'a str {
        labels.get(id).copied().unwrap_or(id)
    }

    /// One node box: `width` chars wide including borders, label padded left.
    fn draw_box(label: &str, width: usize) -> (String, String, String) {
        let inner = width.saturating_sub(2);
        let text: String = label.chars().take(inner.saturating_sub(2)).collect();
        let pad = inner.saturating_sub(2).saturating_sub(text.chars().count());
        let mid = format!("│ {text}{} │", " ".repeat(pad));
        (
            format!("╭{}╮", "─".repeat(width)),
            mid,
            format!("╰{}╯", "─".repeat(width)),
        )
    }

    /// Top-down layout: one layer per row, connectors as `▼` under each parent.
    fn render_td(
        layers: &[Vec<&str>],
        edges: &[MermaidEdge],
        labels: &std::collections::HashMap<&str, &str>,
    ) -> Option<Vec<String>> {
        let col_w = layers
            .iter()
            .flat_map(|l| l.iter())
            .map(|id| mbox_w(mlabel(labels, id)))
            .max()
            .unwrap_or(8)
            .max(8);
        let cols = layers.iter().map(|l| l.len()).max().unwrap_or(1);
        let total_w = cols * (col_w + 2);

        let mut pos: std::collections::HashMap<&str, (usize, usize)> =
            std::collections::HashMap::new();
        for (li, layer) in layers.iter().enumerate() {
            for (ii, id) in layer.iter().enumerate() {
                pos.insert(id, (li, ii));
            }
        }
        let center_x = |li: usize, ii: usize| -> usize {
            let start = total_w.saturating_sub(layers[li].len() * (col_w + 2)) / 2;
            start + ii * (col_w + 2) + col_w / 2
        };

        let mut out: Vec<String> = Vec::new();
        for (li, layer) in layers.iter().enumerate() {
            let mut top = String::new();
            let mut mid = String::new();
            let mut bot = String::new();
            for (ii, id) in layer.iter().enumerate() {
                let w = mbox_w(mlabel(labels, id));
                let (t, m, b) = draw_box(mlabel(labels, id), w);
                let x = center_x(li, ii).saturating_sub(w / 2);
                for row in [&mut top, &mut mid, &mut bot] {
                    while row.chars().count() < x {
                        row.push(' ');
                    }
                }
                top.push_str(&t);
                mid.push_str(&m);
                bot.push_str(&b);
            }
            out.push(top.trim_end().to_string());
            out.push(mid.trim_end().to_string());
            out.push(bot.trim_end().to_string());
            if li + 1 < layers.len() {
                let mut conn: Vec<char> = vec![' '; total_w.max(1)];
                for e in edges {
                    let (fl, fi) = pos.get(e.from.as_str()).copied().unwrap_or((0, 0));
                    let tl = pos.get(e.to.as_str()).copied().map(|(l, _)| l).unwrap_or(0);
                    if fl == li && tl == li + 1 {
                        let x = center_x(fl, fi);
                        if x < conn.len() {
                            conn[x] = '▼';
                        }
                    }
                }
                let s: String = conn.into_iter().collect();
                out.push(if s.trim().is_empty() {
                    String::new()
                } else {
                    s.trim_end().to_string()
                });
            }
        }
        Some(out)
    }

    /// Left-right layout: layers become columns, `──▶` on the middle row.
    fn render_lr(
        layers: &[Vec<&str>],
        edges: &[MermaidEdge],
        labels: &std::collections::HashMap<&str, &str>,
    ) -> Option<Vec<String>> {
        let rows = layers.iter().map(|l| l.len()).max().unwrap_or(1);
        let mut grid: Vec<Vec<Option<&str>>> = vec![vec![None; layers.len()]; rows];
        for (li, layer) in layers.iter().enumerate() {
            for (ri, id) in layer.iter().enumerate() {
                grid[ri][li] = Some(id);
            }
        }
        let col_w: Vec<usize> = layers
            .iter()
            .map(|l| {
                l.iter()
                    .map(|id| mbox_w(mlabel(labels, id)))
                    .max()
                    .unwrap_or(8)
            })
            .collect();

        let edge_between = |a: Option<&str>, b: Option<&str>| -> bool {
            match (a, b) {
                (Some(a), Some(b)) => edges.iter().any(|e| e.from == a && e.to == b),
                _ => false,
            }
        };

        let mut out: Vec<String> = Vec::new();
        for row in &grid {
            let mut top = String::new();
            let mut mid = String::new();
            let mut bot = String::new();
            for (li, cell) in row.iter().enumerate() {
                let w = col_w[li];
                if let Some(id) = cell {
                    let (t, m, b) = draw_box(mlabel(labels, id), w);
                    top.push_str(&t);
                    mid.push_str(&m);
                    bot.push_str(&b);
                } else {
                    top.push_str(&" ".repeat(w));
                    mid.push_str(&" ".repeat(w));
                    bot.push_str(&" ".repeat(w));
                }
                if li + 1 < row.len() {
                    let arrow = if edge_between(*cell, row[li + 1]) {
                        "──▶ "
                    } else {
                        "    "
                    };
                    top.push_str("    ");
                    mid.push_str(arrow);
                    bot.push_str("    ");
                }
            }
            out.push(top.trim_end().to_string());
            out.push(mid.trim_end().to_string());
            out.push(bot.trim_end().to_string());
            out.push(String::new());
        }
        Some(out)
    }

    /// Expand fenced ```mermaid blocks into Unicode diagrams. Spans lines, so it
    /// runs before line splitting. Unknown syntax keeps the raw fence verbatim.
    fn expand_mermaid_fences(text: &str) -> String {
        let mut expanded = String::new();
        let mut rest = text;
        loop {
            let start = match rest.find("```mermaid") {
                Some(p) => p,
                None => {
                    expanded.push_str(rest);
                    break;
                }
            };
            expanded.push_str(&rest[..start]);
            let body_start = start + "```mermaid".len();
            let after = &rest[body_start..];
            match after.find("```") {
                Some(end) => {
                    let body = &after[..end];
                    match render_mermaid(body) {
                        Some(diagram) => {
                            expanded.push('\n');
                            for line in diagram {
                                expanded.push_str(&line);
                                expanded.push('\n');
                            }
                        }
                        None => {
                            // Unknown syntax: keep the raw fence verbatim.
                            expanded.push_str(&rest[start..body_start + end + 3]);
                        }
                    }
                    rest = &after[end + 3..];
                }
                None => {
                    expanded.push_str(&rest[start..]);
                    break;
                }
            }
        }
        expanded
    }

    // ===========================================================================
    // LaTeX math → Unicode approximations
    // ===========================================================================

    /// Replace a LaTeX command name (without backslash) with its Unicode
    /// approximation. Unknown commands return `None` and pass through untouched.
    fn latex_command(name: &str) -> Option<&'static str> {
        Some(match name {
            "alpha" => "α",
            "beta" => "β",
            "gamma" => "γ",
            "delta" => "δ",
            "epsilon" => "ε",
            "zeta" => "ζ",
            "eta" => "η",
            "theta" => "θ",
            "lambda" => "λ",
            "mu" => "μ",
            "pi" => "π",
            "sigma" => "σ",
            "tau" => "τ",
            "phi" => "φ",
            "omega" => "ω",
            "Gamma" => "Γ",
            "Delta" => "Δ",
            "Theta" => "Θ",
            "Lambda" => "Λ",
            "Sigma" => "Σ",
            "Omega" => "Ω",
            "infty" => "∞",
            "sum" => "∑",
            "prod" => "∏",
            "int" => "∫",
            "sqrt" => "√",
            "times" => "×",
            "cdot" => "·",
            "div" => "÷",
            "pm" => "±",
            "leq" => "≤",
            "geq" => "≥",
            "neq" => "≠",
            "approx" => "≈",
            "equiv" => "≡",
            "rightarrow" => "→",
            "leftarrow" => "←",
            "leftrightarrow" => "↔",
            "Rightarrow" => "⇒",
            "to" => "→",
            "ldots" => "...",
            "cdots" => "⋯",
            "partial" => "∂",
            "forall" => "∀",
            "exists" => "∃",
            "in" => "∈",
            "notin" => "∉",
            "subset" => "⊂",
            "cup" => "∪",
            "cap" => "∩",
            "angle" => "∠",
            "degree" => "°",
            _ => return None,
        })
    }

    fn superscript(c: char) -> Option<char> {
        Some(match c {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '+' => '⁺',
            '-' => '⁻',
            '=' => '⁼',
            '(' => '⁽',
            ')' => '⁾',
            'n' => 'ⁿ',
            'i' => 'ⁱ',
            'a' => 'ᵃ',
            'b' => 'ᵇ',
            'c' => 'ᶜ',
            'd' => 'ᵈ',
            'e' => 'ᵉ',
            'f' => 'ᶠ',
            'g' => 'ᵍ',
            'h' => 'ʰ',
            'j' => 'ʲ',
            'k' => 'ᵏ',
            'l' => 'ˡ',
            'm' => 'ᵐ',
            'o' => 'ᵒ',
            'p' => 'ᵖ',
            'r' => 'ʳ',
            's' => 'ˢ',
            't' => 'ᵗ',
            'u' => 'ᵘ',
            'v' => 'ᵛ',
            'w' => 'ʷ',
            'x' => 'ˣ',
            'y' => 'ʸ',
            'z' => 'ᶻ',
            _ => return None,
        })
    }

    fn subscript(c: char) -> Option<char> {
        Some(match c {
            '0' => '₀',
            '1' => '₁',
            '2' => '₂',
            '3' => '₃',
            '4' => '₄',
            '5' => '₅',
            '6' => '₆',
            '7' => '₇',
            '8' => '₈',
            '9' => '₉',
            '+' => '₊',
            '-' => '₋',
            '=' => '₌',
            '(' => '₍',
            ')' => '₎',
            'a' => 'ₐ',
            'e' => 'ₑ',
            'i' => 'ᵢ',
            'j' => 'ⱼ',
            'n' => 'ₙ',
            'o' => 'ₒ',
            'r' => 'ᵣ',
            'u' => 'ᵤ',
            'v' => 'ᵥ',
            'x' => 'ₓ',
            _ => return None,
        })
    }

    /// Render one math span (without the surrounding `$`s) to Unicode.
    /// `display` stacks `\frac` vertically; inline uses the fraction slash.
    fn render_math_span(span: &str, display: bool) -> String {
        let chars: Vec<char> = span.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        // Extract a brace group starting at `i` (which points at `{`).
        let take_group = |i: &mut usize| -> Option<String> {
            if chars.get(*i) != Some(&'{') {
                return None;
            }
            let mut depth = 0;
            let mut s = String::new();
            for (k, &c) in chars.iter().enumerate().skip(*i) {
                if c == '{' {
                    depth += 1;
                    if depth > 1 {
                        s.push(c);
                    }
                } else if c == '}' {
                    depth -= 1;
                    if depth == 0 {
                        *i = k + 1;
                        return Some(s);
                    }
                    s.push(c);
                } else if depth >= 1 {
                    s.push(c);
                }
            }
            None
        };
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' {
                // \frac, \sqrt need brace args; try them first.
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_alphabetic() {
                    j += 1;
                }
                let name: String = chars[i + 1..j].iter().collect();
                if name == "frac" {
                    let mut k = j;
                    k += chars[j..].iter().take_while(|c| **c == ' ').count();
                    if let Some(num) = take_group(&mut k) {
                        let mut k2 = k;
                        k2 += chars[k..].iter().take_while(|c| **c == ' ').count();
                        if let Some(den) = take_group(&mut k2) {
                            let num = render_math_span(&num, false);
                            let den = render_math_span(&den, false);
                            if display {
                                let w = num.chars().count().max(den.chars().count()).max(1);
                                out.push('\n');
                                out.push_str(&format!("{:^w$}\n{}", num, "─".repeat(w), w = w));
                                out.push('\n');
                                out.push_str(&format!("{:^w$}", den, w = w));
                            } else {
                                out.push_str(&format!("{num}⁄{den}"));
                            }
                            i = k2;
                            continue;
                        }
                    }
                    // Malformed \frac: pass through.
                    out.push('\\');
                    out.push_str(&name);
                    i = j;
                    continue;
                }
                if name == "sqrt" {
                    let mut k = j;
                    k += chars[j..].iter().take_while(|c| **c == ' ').count();
                    if let Some(inner) = take_group(&mut k) {
                        out.push('√');
                        out.push('(');
                        out.push_str(&render_math_span(&inner, false));
                        out.push(')');
                        i = k;
                        continue;
                    }
                    out.push('\\');
                    out.push_str(&name);
                    i = j;
                    continue;
                }
                if let Some(rep) = latex_command(&name) {
                    out.push_str(rep);
                } else {
                    // Unknown command: pass through verbatim.
                    out.push('\\');
                    out.push_str(&name);
                }
                i = j;
                continue;
            }
            if c == '^' || c == '_' {
                let map = if c == '^' { superscript } else { subscript };
                let mut j = i + 1;
                if chars.get(j) == Some(&'{') {
                    j += 1;
                    while j < chars.len() && chars[j] != '}' {
                        if let Some(r) = map(chars[j]) {
                            out.push(r);
                        } else {
                            out.push(chars[j]);
                        }
                        j += 1;
                    }
                    i = j + 1;
                } else if let Some(&next) = chars.get(j) {
                    if let Some(r) = map(next) {
                        out.push(r);
                        i = j + 1;
                    } else {
                        out.push(c);
                        i += 1;
                    }
                } else {
                    out.push(c);
                    i += 1;
                }
                continue;
            }
            if c == '{' || c == '}' {
                i += 1; // grouping braces vanish after command handling
                continue;
            }
            out.push(c);
            i += 1;
        }
        out
    }

    /// Render LaTeX math in text: display `$$...$$` becomes its own indented
    /// block, inline `$...$` is replaced in place. A `$` needs a matching close
    /// on a later position; unmatched `$` passes through untouched.
    pub fn render_latex(text: &str) -> String {
        let mut out = String::new();
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '$' {
                let display = chars.get(i + 1) == Some(&'$');
                let open_len = if display { 2 } else { 1 };
                // Find the matching close.
                let mut j = i + open_len;
                let mut found = None;
                while j < chars.len() {
                    if chars[j] == '$' {
                        if display {
                            if chars.get(j + 1) == Some(&'$') {
                                found = Some(j);
                                break;
                            }
                            j += 1;
                        } else {
                            found = Some(j);
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                if let Some(close) = found {
                    let span: String = chars[i + open_len..close].iter().collect();
                    let rendered = render_math_span(span.trim(), display);
                    if display {
                        out.push('\n');
                        for line in rendered.lines() {
                            out.push_str("    ");
                            out.push_str(line);
                            out.push('\n');
                        }
                    } else {
                        out.push_str(&rendered);
                    }
                    i = close + open_len;
                    continue;
                }
                // Unmatched: literal.
                out.push('$');
                i += 1;
                continue;
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    // ===========================================================================
    // Message segmentation: text + images + rendered fences
    // ===========================================================================

    /// One renderable piece of a message.
    #[derive(Debug)]
    pub enum RichSegment {
        /// Plain (already LaTeX-expanded, mermaid-rendered) text, may be multiline.
        Text(String),
        /// An image reference; the renderer shows a thumbnail or placeholder.
        Image(ImageRef),
    }

    /// Split a message into text and image segments. Fenced ` ```mermaid `
    /// blocks become rendered diagrams (or pass through raw when the syntax is
    /// unknown); image references become [`RichSegment::Image`]; everything
    /// else gets [`render_latex`] applied.
    pub fn segment_message(text: &str) -> Vec<RichSegment> {
        // First pass: mermaid fences (they span lines, so handle before split).
        let expanded = expand_mermaid_fences(text);

        // Second pass: images line by line, latex on the text lines.
        let mut segments: Vec<RichSegment> = Vec::new();
        let mut pending_text = String::new();
        let flush_text = |pending: &mut String, segments: &mut Vec<RichSegment>| {
            if !pending.trim().is_empty() {
                segments.push(RichSegment::Text(render_latex(pending)));
            }
            pending.clear();
        };
        for line in expanded.lines() {
            let images = find_images(line);
            if images.is_empty() {
                pending_text.push_str(line);
                pending_text.push('\n');
            } else {
                flush_text(&mut pending_text, &mut segments);
                // Keep any non-path text on the line as its own text segment.
                let stripped = strip_image_tokens(line, &images);
                if !stripped.trim().is_empty() {
                    segments.push(RichSegment::Text(render_latex(&stripped)));
                }
                for img in images {
                    segments.push(RichSegment::Image(img));
                }
            }
        }
        flush_text(&mut pending_text, &mut segments);
        segments
    }

    /// Remove image path tokens (and their `@`/`![..](..)` wrappers) from a line
    /// so the prose that introduced the image still reads naturally.
    fn strip_image_tokens(line: &str, images: &[ImageRef]) -> String {
        let names: Vec<&str> = images.iter().map(|i| i.name.as_str()).collect();
        let mut s = line.to_string();
        // Markdown wrappers: ![alt](path).
        let mut idx = 0;
        while idx < s.len() {
            let bang = match s[idx..].find("![") {
                Some(p) => idx + p,
                None => break,
            };
            let after = &s[bang + 2..];
            let paren = match after.find("](") {
                Some(p) => p,
                None => break,
            };
            let url_start = bang + 2 + paren + 2;
            let end = match s[url_start..].find(')') {
                Some(p) => p,
                None => break,
            };
            let fname = s[url_start..url_start + end]
                .rsplit('/')
                .next()
                .unwrap_or("");
            if names.contains(&fname) {
                s.replace_range(bang..url_start + end + 1, "");
            } else {
                idx = url_start + end + 1;
            }
        }
        // Bare and @-prefixed tokens, matched on file name.
        let mut kept: Vec<&str> = Vec::new();
        for tok in s.split_whitespace() {
            let clean = tok
                .trim_matches(|c: char| ".,;:!?()[]{}\"'".contains(c))
                .strip_prefix('@')
                .unwrap_or_else(|| tok.trim_matches(|c: char| ".,;:!?()[]{}\"'".contains(c)));
            let fname = clean.rsplit('/').next().unwrap_or(clean);
            if names.contains(&fname) {
                continue;
            }
            kept.push(tok);
        }
        kept.join(" ")
    }

    // ===========================================================================
    // Inline painting: rows reserved at render time, pixels placed post-draw
    // ===========================================================================

    /// One image row reservation inside the transcript.
    #[derive(Debug, Clone)]
    pub struct ImagePlacement {
        /// Unique id for Kitty placement/deletion.
        pub id: u32,
        /// Transcript line index of the first reserved row.
        pub line_idx: usize,
        /// Resolved image path.
        pub path: PathBuf,
        /// File name for the placeholder text.
        pub name: String,
        /// Cells wide the thumbnail is painted.
        pub cols: u16,
        /// Transcript rows reserved (thumbnail height + label row).
        pub rows: u16,
    }

    /// Transcript viewport geometry captured during render.
    #[derive(Debug, Clone, Copy)]
    pub struct TranscriptView {
        pub x: u16,
        pub y: u16,
        pub width: u16,
        pub height: u16,
        pub scroll_off: usize,
    }

    /// Fullscreen image preview state.
    #[derive(Debug, Clone)]
    pub struct ImagePreview {
        /// All transcript images, so `[`/`]` can cycle.
        pub images: Vec<PathBuf>,
        pub sel: usize,
        pub zoom: f32,
        pub pan_x: i32,
        pub pan_y: i32,
    }

    /// Owns everything about inline images: placements recorded during render,
    /// what is currently painted on the terminal, capability, and the preview.
    #[derive(Debug, Default)]
    pub struct ImagePaintState {
        pub graphics: GraphicsSupport,
        pub placements: Vec<ImagePlacement>,
        pub view: Option<TranscriptView>,
        pub preview: Option<ImagePreview>,
        /// Where the preview box was drawn this frame (set by the renderer).
        pub preview_area: Option<ratatui::layout::Rect>,
        next_id: u32,
        painted: Vec<PaintedImage>,
        /// (path, sel, zoom_pct, pan_x, pan_y) of the preview image currently
        /// on the terminal: skip repaint when nothing changed.
        preview_painted: Option<(PathBuf, usize, u32, i32, i32)>,
    }

    #[derive(Debug, Clone)]
    struct PaintedImage {
        id: u32,
        row: u16,
        col: u16,
    }

    impl ImagePaintState {
        pub fn new() -> Self {
            Self {
                graphics: GraphicsSupport::detect(),
                ..Default::default()
            }
        }

        /// Reserve rows for an image and return its placement id. Called from
        /// `render_block`; rows are painted later by [`Self::paint`].
        pub fn reserve(&mut self, img: &ImageRef, line_idx: usize, term_width: u16) -> u32 {
            let id = self.next_id;
            self.next_id += 1;
            let cols = term_width.saturating_sub(12).clamp(16, 48);
            // Thumbnail rows at ~2:1 cell aspect + one label row.
            let rows = match (img.width, img.height) {
                (Some(w), Some(h)) if w > 0 && h > 0 => {
                    ((cols as f32 * h as f32 / w as f32 / 2.0).ceil() as u16).clamp(2, 12) + 1
                }
                _ => 5,
            };
            self.placements.push(ImagePlacement {
                id,
                line_idx,
                path: img.path.clone(),
                name: img.name.clone(),
                cols,
                rows,
            });
            id
        }

        /// Text shown when graphics are unavailable (or as the first reserved
        /// row under a painted thumbnail): `[1] 🖼 name (WxH) - v to view`.
        pub fn placeholder_text(
            p: &ImagePlacement,
            dims: Option<(u32, u32)>,
            index: usize,
        ) -> String {
            let dims = dims
                .map(|(w, h)| format!("{w}×{h}"))
                .unwrap_or_else(|| "unknown size".to_string());
            format!("[{}] 🖼 {} ({dims}) - press v to view", index + 1, p.name)
        }

        /// Diff desired placements against what is painted and emit Kitty/Sixel
        /// sequences. No-op unless Kitty is available: Sixel and text fallback
        /// need no per-frame painting (Sixel paints on demand in the preview).
        pub fn paint(&mut self) -> std::io::Result<()> {
            if self.graphics != GraphicsSupport::Kitty {
                return Ok(());
            }
            let Some(view) = self.view else {
                return self.clear_painted();
            };
            // Desired: visible placements with absolute rows. The label row
            // stays text; the thumbnail paints into the reserved rows below it.
            let mut desired: Vec<(u32, PathBuf, u16, u16, u16)> = Vec::new(); // id, path, row, col, cols
            for p in &self.placements {
                if p.line_idx < view.scroll_off {
                    continue;
                }
                let label_row = view.y + (p.line_idx - view.scroll_off) as u16;
                let row = label_row + 1;
                if row + p.rows.saturating_sub(1) > view.y + view.height.saturating_sub(1) {
                    continue;
                }
                desired.push((p.id, p.path.clone(), row, view.x, p.cols));
            }
            // Delete anything painted that is no longer desired (or moved).
            let mut keep: Vec<PaintedImage> = Vec::new();
            let mut stale: Vec<u32> = Vec::new();
            for painted in self.painted.drain(..) {
                let still_there = desired.iter().any(|(id, _, row, col, _)| {
                    *id == painted.id && *row == painted.row && *col == painted.col
                });
                if still_there {
                    keep.push(painted);
                } else {
                    stale.push(painted.id);
                }
            }
            self.painted = keep;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            for id in stale {
                lock.write_all(&kitty_delete(id))?;
            }
            // Paint new ones (PNG via f=100; anything else skipped - the
            // placeholder text already describes it).
            for (id, path, row, col, cols) in &desired {
                if self.painted.iter().any(|p| p.id == *id) {
                    continue;
                }
                let is_png = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("png"))
                    .unwrap_or(false);
                if !is_png {
                    continue;
                }
                if let Ok(png) = std::fs::read(path) {
                    crossterm::execute!(lock, crossterm::cursor::MoveTo(*col, *row))?;
                    lock.write_all(&kitty_image_payload(&png, *id, *cols))?;
                    self.painted.push(PaintedImage {
                        id: *id,
                        row: *row,
                        col: *col,
                    });
                }
            }
            lock.flush()?;
            Ok(())
        }

        /// Delete every painted image. Called on overlay open, scroll jumps,
        /// and shutdown.
        pub fn clear_painted(&mut self) -> std::io::Result<()> {
            if self.painted.is_empty() {
                return Ok(());
            }
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            for p in self.painted.drain(..) {
                lock.write_all(&kitty_delete(p.id))?;
            }
            lock.flush()?;
            Ok(())
        }

        /// Paint the fullscreen preview image centered in `area` (called
        /// post-draw, like [`Self::paint`]). Returns `Ok(false)` when there is
        /// nothing to paint (text fallback already rendered the box).
        pub fn paint_preview(&mut self, area: ratatui::layout::Rect) -> std::io::Result<bool> {
            let Some(preview) = self.preview.clone() else {
                return Ok(false);
            };
            let path = match preview.images.get(preview.sel) {
                Some(p) => p.clone(),
                None => return Ok(false),
            };
            // Skip repaint when nothing changed: Kitty/Sixel images persist
            // until deleted, so repainting every frame would just flicker.
            let key = (
                path.clone(),
                preview.sel,
                (preview.zoom * 100.0) as u32,
                preview.pan_x,
                preview.pan_y,
            );
            if self.preview_painted.as_ref() == Some(&key) {
                return Ok(true);
            }
            let png = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => return Ok(false),
            };
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            match self.graphics {
                GraphicsSupport::Kitty => {
                    // Scale by cells: base 60 cols * zoom, clamped to the box.
                    let cols = ((60.0 * preview.zoom) as u16)
                        .clamp(10, area.width.saturating_sub(4).max(10));
                    let col = (area.x as i32
                        + (area.width.saturating_sub(cols) as i32) / 2
                        + preview.pan_x)
                        .max(0) as u16;
                    let row = (area.y as i32 + 1 + preview.pan_y).max(0) as u16;
                    crossterm::execute!(lock, crossterm::cursor::MoveTo(col, row))?;
                    // Kitty id 0 is reserved for the preview; repaint deletes first.
                    lock.write_all(&kitty_delete(0))?;
                    // c= alone preserves aspect; zoom scales the cell width.
                    lock.write_all(&kitty_image_payload(&png, 0, cols))?;
                }
                GraphicsSupport::Sixel => {
                    if let Some((rgb, w, h)) = png_to_rgb(&png) {
                        let (rgb, w, h) = downscale_rgb(&rgb, w, h, 480);
                        let col = (area.x as i32 + 2 + preview.pan_x).max(0) as u16;
                        let row = (area.y as i32 + 2 + preview.pan_y).max(0) as u16;
                        crossterm::execute!(lock, crossterm::cursor::MoveTo(col, row))?;
                        lock.write_all(&encode_sixel(&rgb, w, h))?;
                    } else {
                        return Ok(false);
                    }
                }
                GraphicsSupport::None => return Ok(false),
            }
            lock.flush()?;
            self.preview_painted = Some(key);
            Ok(true)
        }

        /// Open the preview on the most recent transcript image.
        pub fn open_preview(&mut self) {
            let images: Vec<PathBuf> = self.placements.iter().map(|p| p.path.clone()).collect();
            if images.is_empty() {
                return;
            }
            self.preview = Some(ImagePreview {
                sel: images.len() - 1,
                images,
                zoom: 1.0,
                pan_x: 0,
                pan_y: 0,
            });
        }

        pub fn close_preview(&mut self) -> std::io::Result<()> {
            self.preview = None;
            self.preview_painted = None;
            // The preview used Kitty id 0; remove it.
            if self.graphics == GraphicsSupport::Kitty {
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                lock.write_all(&kitty_delete(0))?;
                lock.flush()?;
            }
            Ok(())
        }
    }
}

pub mod gateway {
    //! `pantheon gateway`: run the channel daemon against the live runtime.
    //!
    //! One thread per surface: Discord gateway websocket, Telegram long poll,
    //! each with its own outbound queue and feeding a shared `EventSink` that
    //! starts/resumes runs and answers approvals through the Supervisor.
    //! Replies flow back over the same surfaces via `TelegramChannel` and
    //! `DiscordChannel` (real REST transports) - never a memory buffer nobody
    //! reads.
    //!
    //! Tokens come from the environment only:
    //!   PANTHEON_DISCORD_TOKEN, PANTHEON_TELEGRAM_BOT_TOKEN
    //! Missing tokens disable that surface; at least one must be set.

    use pantheon_api::capability::Policy;
    use pantheon_extensions::{ExtensionManager, Hook, RunnerConfig};
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// A parked run that needs continuing after an approval grant.
    #[derive(Debug, Clone)]
    struct ResumeRequest {
        gateway: String,
        thread_id: String,
        run_id: String,
    }

    /// Continue a parked run after a grant. `Send + Sync` so the daemon thread
    /// can invoke it; the default implementation runs on a background thread
    /// because the resume blocks on the model. Tests swap in a recorder.
    type ResumeHook = Arc<dyn Fn(ResumeRequest) + Send + Sync>;

    /// The production resume hook: continue the parked run on a background
    /// thread and deliver the outcome text back to the originating thread.
    /// Errors are logged, never fatal - a failed resume must not take the
    /// gateway daemon down with it (unlike the CLI path, which exits).
    fn default_resume_hook(
        data_dir: PathBuf,
        queues: &HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>>,
    ) -> ResumeHook {
        let queues = queues.clone();
        Arc::new(move |req: ResumeRequest| {
            let queues = queues.clone();
            let data_dir = data_dir.clone();
            std::thread::spawn(move || match resume_parked_run(&data_dir, &req.run_id) {
                Ok(answer) => {
                    if !answer.is_empty() {
                        push_to_queue(&queues, &req.gateway, &req.thread_id, answer);
                    }
                }
                Err(e) => eprintln!("gateway: resume {} failed: {e}", req.run_id),
            });
        })
    }

    /// Continue a parked run after a grant, returning the outcome text for
    /// delivery. Same shape as `crate::agui::resume_after_grant`, but it
    /// returns errors instead of exiting the process: the CLI can afford to
    /// die on failure, a long-running gateway cannot.
    fn resume_parked_run(data_dir: &Path, run_id: &str) -> Result<String, String> {
        use crate::config;
        use crate::config::build_model_policy;
        let file_cfg = config::Config::load_or_report(data_dir);
        let model_policy = build_model_policy(file_cfg.as_ref(), None, None);
        // Policy preset honored verbatim: a Reader config must stay
        // read-only on resume, not silently widen to coder.
        let policy = crate::config_schema::policy_for_config(&file_cfg);
        let secrets = config::chat_secrets(file_cfg.as_ref());
        let session = pantheon_runtime::session::Session::new(
            data_dir.to_path_buf(),
            policy,
            model_policy,
            secrets,
        )
        .map_err(|e| e.to_string())?;
        config::apply_tool_enablement(&session, file_cfg.as_ref());
        config::apply_budget_tiers(&session, file_cfg.as_ref());
        config::apply_delegation_section(&session, file_cfg.as_ref());
        // An empty turn rebuilds the transcript from the ledger and settles
        // the granted call; never resend the user message (duplicate turn).
        session
            .chat_turn(run_id, "", "")
            .map(|outcome| crate::terminal::outcome_text(&outcome))
            .map_err(|e| e.to_string())
    }

    /// Push one reply into a surface's outbound queue. A message for an
    /// unknown surface is dead-lettered with a log rather than misdelivered.
    fn push_to_queue(
        queues: &HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>>,
        gateway: &str,
        thread: &str,
        text: String,
    ) {
        match queues.get(gateway) {
            Some(q) => q.lock().unwrap_or_else(|e| e.into_inner()).push(
                pantheon_gateway::OutboundMessage {
                    to_conversation: thread.to_string(),
                    text,
                    gateway: gateway.to_string(),
                    attempts: 0,
                },
            ),
            None => eprintln!(
                "gateway: no outbound queue for surface '{gateway}'; dropping reply to {thread}"
            ),
        }
    }

    /// The runtime-backed sink. Maps channel threads to run ids through a
    /// thread→run map so a conversation keeps its run across messages.
    ///
    /// `gateway` on every method is the *channel name* (`telegram`/`discord`
    /// in the legacy single-bot shape, any slug for explicit multi-bot
    /// channels) - never the platform. Thread ids are namespaced per channel
    /// before they touch the thread map: two bots on the same platform hand
    /// out overlapping thread ids, and the raw id would let bot B pick up bot
    /// A's conversation (and answer it from the wrong agent).
    struct RuntimeSink {
        data_dir: PathBuf,
        /// Config-default policy. A channel with an agent may narrow it to
        /// the agent's resolved preset (see [`RuntimeSink::policy_for`]).
        policy: Policy,
        threads: Mutex<HashMap<String, String>>,
        /// One outbound queue per surface ("telegram"/"discord"). Each
        /// surface's daemon drains only its own queue, so a Telegram reply
        /// can never be picked up by the Discord daemon and vice versa - the
        /// old single shared queue let each daemon grab the other's replies.
        queues: HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>>,
        /// Allowed platform sender ids (Telegram user id, Discord user id).
        /// None = allowlist disabled (local testing); Some(empty) denies
        /// everyone. The gateway is a remote shell, so the default is Some:
        /// cmd_gateway requires an explicit allowlist before it starts.
        allow: Option<HashSet<String>>,
        /// Runs after a grant is recorded so the parked run continues.
        resume: ResumeHook,
        /// Per-channel agent runtimes, by channel name. A message is answered
        /// by its channel's agent - own profile, own persona, own memory
        /// namespace. A channel with no entry here runs anonymous, exactly as
        /// before profiles existed.
        agents: HashMap<String, pantheon_runtime::AgentRuntime>,
        /// Extension manager for the `pre_gateway_dispatch` observer hook.
        /// Fired by the surface sink before any inbound event reaches the
        /// runtime, so plugins that log, tag, or metric inbound traffic
        /// actually run. `None` = no hook (tests / manager-less paths).
        hooks: Option<Arc<ExtensionManager>>,
    }

    /// Namespace a raw platform thread id under its channel so two bots on
    /// the same platform never share a thread-map entry.
    fn thread_key(channel: &str, thread_id: &str) -> String {
        format!("{channel}\u{1f}{thread_id}")
    }

    impl RuntimeSink {
        fn new(
            data_dir: PathBuf,
            policy: Policy,
            queues: HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>>,
            allow: Option<HashSet<String>>,
        ) -> Self {
            let resume = default_resume_hook(data_dir.clone(), &queues);
            Self {
                data_dir,
                policy,
                threads: Mutex::new(HashMap::new()),
                queues,
                allow,
                resume,
                agents: HashMap::new(),
                hooks: None,
            }
        }

        /// Attach the per-channel agent runtimes (built once at startup).
        /// Builder-style so the anonymous single-bot path keeps calling
        /// `new` unchanged.
        fn with_agents(mut self, agents: HashMap<String, pantheon_runtime::AgentRuntime>) -> Self {
            self.agents = agents;
            self
        }

        /// Attach the extension manager whose `pre_gateway_dispatch`
        /// hook fires on every inbound event before dispatch.
        fn with_hooks(mut self, hooks: Arc<ExtensionManager>) -> Self {
            self.hooks = Some(hooks);
            self
        }

        /// The policy for one channel's turn. `PANTHEON_GATEWAY_POLICY` wins
        /// when set (the historical override); otherwise the channel agent's
        /// resolved preset; otherwise the config default. Anonymous channels
        /// get exactly today's behavior.
        fn policy_for(&self, channel: &str) -> Policy {
            // The historical spellings ("researcher") plus the preset
            // spellings ("reader", "coder_memory"): the old match honoured
            // only researcher/coder, so coder_memory and typos silently
            // fell through to the channel default.
            if let Ok(v) = std::env::var("PANTHEON_GATEWAY_POLICY") {
                match v.as_str() {
                    "researcher" | "reader" => return Policy::researcher_readonly(),
                    "coder" => return Policy::coder(),
                    "coder_memory" => return Policy::coder_with_memory(),
                    other => {
                        eprintln!(
                            "[pantheon] unknown PANTHEON_GATEWAY_POLICY value '{other}' (expected researcher, reader, coder, or coder_memory); falling back to the channel default"
                        );
                    }
                }
            }
            if let Some(preset) = self
                .agents
                .get(channel)
                .and_then(|a| crate::config_schema::PolicyPreset::parse(a.policy_preset()))
            {
                return preset.to_policy();
            }
            self.policy.clone()
        }
    }

    impl RuntimeSink {
        /// Allowlist check. A sender not on the list is refused loudly on
        /// their channel and nothing reaches the runtime.
        fn allowed(&self, sender: Option<&str>) -> bool {
            match (&self.allow, sender) {
                (None, _) => true,
                (Some(set), Some(id)) => set.contains(id),
                // No sender identity and an allowlist is active: refuse.
                // A bridge that strips identity must be explicitly trusted
                // by leaving the allowlist unset.
                (Some(_), None) => false,
            }
        }
    }

    fn open_session(
        data_dir: &Path,
        policy: Policy,
    ) -> Result<pantheon_runtime::session::Session, pantheon_api::error::PantheonError> {
        let cfg = crate::config::Config::load_or_report(data_dir);
        let default = pantheon_api::model::DefaultModel {
            provider: std::env::var("PANTHEON_PROVIDER").unwrap_or_else(|_| "local".into()),
            model: std::env::var("PANTHEON_MODEL").unwrap_or_else(|_| "llama3.2".into()),
            key_env: None,
        };
        let model_policy = pantheon_api::model::ModelPolicy {
            reasoning_budget: Default::default(),
            reasoning: Default::default(),
            default: default.clone(),
            fallbacks: pantheon_api::model::FallbackChain::default(),
            auxiliaries: crate::config::auxiliaries(cfg.as_ref(), &default),
        };
        let secrets = crate::config::chat_secrets(cfg.as_ref());
        // `[memory]` wins over memory-backend.toml on load: sync BEFORE
        // the session reads the selection, or the sync lands a session late.
        crate::config::sync_memory_selection(data_dir, cfg.as_ref());
        pantheon_runtime::session::Session::new(
            data_dir.to_path_buf(),
            policy,
            model_policy,
            secrets,
        )
        .inspect(|s| {
            crate::config::apply_tool_enablement(s, cfg.as_ref());
            crate::config::apply_budget_tiers(s, cfg.as_ref());
            crate::config::apply_delegation_section(s, cfg.as_ref());
        })
    }

    impl RuntimeSink {
        fn push_outbound(&self, gateway: &str, thread: &str, text: String) {
            push_to_queue(&self.queues, gateway, thread, text);
        }
    }

    /// Tags every event with the channel it arrived on so the shared sink can
    /// route replies into the right per-channel queue. One of these wraps the
    /// `RuntimeSink` per daemon thread. The tag is the channel name (any slug
    /// for explicit multi-bot channels), not a 'static platform string.
    struct SurfaceSink<'a> {
        inner: &'a RuntimeSink,
        gateway: &'a str,
    }

    impl pantheon_gateway::EventSink for SurfaceSink<'_> {
        fn on_message(&self, thread_id: &str, sender: Option<&str>, text: &str) {
            // `pre_gateway_dispatch`: observer-class, fired before the
            // runtime sees the message. Its return is discarded by
            // contract; a broken plugin never blocks the dispatch (the
            // manager's fire path already fails open for observers).
            if let Some(mgr) = &self.inner.hooks {
                let mut extra = HashMap::new();
                extra.insert("thread_id".to_string(), thread_id.to_string());
                if let Some(sender) = sender {
                    extra.insert("sender".to_string(), sender.to_string());
                }
                extra.insert("text".to_string(), text.to_string());
                let _ = mgr.fire(Hook::PreGatewayDispatch, thread_id, self.gateway, extra);
            }
            self.inner.on_message(self.gateway, thread_id, sender, text);
        }
        fn on_approval(
            &self,
            thread_id: &str,
            sender: Option<&str>,
            run_id: Option<&str>,
            scope: &str,
            grant: bool,
        ) {
            if let Some(mgr) = &self.inner.hooks {
                let mut extra = HashMap::new();
                extra.insert("thread_id".to_string(), thread_id.to_string());
                if let Some(sender) = sender {
                    extra.insert("sender".to_string(), sender.to_string());
                }
                if let Some(run_id) = run_id {
                    extra.insert("run_id".to_string(), run_id.to_string());
                }
                extra.insert("scope".to_string(), scope.to_string());
                extra.insert("grant".to_string(), grant.to_string());
                let _ = mgr.fire(Hook::PreGatewayDispatch, thread_id, self.gateway, extra);
            }
            self.inner
                .on_approval(self.gateway, thread_id, sender, run_id, scope, grant);
        }
    }

    impl RuntimeSink {
        fn on_message(&self, gateway: &str, thread_id: &str, sender: Option<&str>, text: &str) {
            if !self.allowed(sender) {
                self.push_outbound(
                    gateway,
                    thread_id,
                    "not authorized: this bot only accepts messages from its allowlist".into(),
                );
                return;
            }
            let agent = self.agents.get(gateway).cloned();
            let session = match open_session(&self.data_dir, self.policy_for(gateway)) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("gateway: session open failed: {e}");
                    return;
                }
            };
            if let Some(agent) = agent {
                // Attach the channel's agent before chatting: `chat_turn`
                // binds the run to this agent, so a run id that another
                // channel's agent owns is refused instead of answered from
                // the wrong identity.
                if let Err(e) = session.with_agent(agent) {
                    self.push_outbound(gateway, thread_id, format!("error: {e}"));
                    return;
                }
            }
            let key = thread_key(gateway, thread_id);
            let run_id = self
                .threads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(key)
                .or_insert_with(pantheon_runtime::new_run_id)
                .clone();
            match session.chat(&run_id, text) {
                Ok(outcome) => {
                    let text = match outcome {
                        pantheon_agent::LoopOutcome::Answered { text: t, .. } => t,
                        pantheon_agent::LoopOutcome::AwaitingApproval { capability, scope } => {
                            // `is_git_push` is advisory detection (see its
                            // docs), not a security boundary: a push hidden in
                            // an opaque shell construction may never be flagged.
                            // Say so on the one approval the detector produces;
                            // the approval flow and message shape are untouched.
                            let advisory = if matches!(
                                &capability,
                                pantheon_api::capability::Capability::GitPush
                            ) {
                                " Note: git-push detection is advisory; a push \
                                 hidden in an opaque shell construction may not \
                                 be flagged."
                            } else {
                                ""
                            };
                            format!(
                                "approval needed: {capability:?}.{advisory} Reply 'grant {run_id} {scope}' to \
                                 allow it, or 'deny {run_id} {scope}' to refuse it."
                            )
                        }
                        pantheon_agent::LoopOutcome::Denied { capability } => {
                            format!("denied: {capability:?}")
                        }
                        pantheon_agent::LoopOutcome::BudgetExhausted { cap } => {
                            format!("stopped: {cap} budget exhausted")
                        }
                        other => format!("ended: {other:?}"),
                    };
                    self.push_outbound(gateway, thread_id, text);
                }
                Err(e) => {
                    self.push_outbound(gateway, thread_id, format!("error: {e}"));
                }
            }
        }

        fn on_approval(
            &self,
            gateway: &str,
            thread_id: &str,
            sender: Option<&str>,
            _callback_run_id: Option<&str>,
            scope: &str,
            grant: bool,
        ) {
            if !self.allowed(sender) {
                self.push_outbound(gateway, thread_id, "not authorized".into());
                return;
            }
            let sup = match pantheon_runtime::Supervisor::open(self.data_dir.clone()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("gateway: supervisor open failed: {e}");
                    return;
                }
            };
            // The daemon's thread map only knows runs it initiated itself:
            // button taps on approval prompts the gateway sent resolve here.
            // (Phone notifications about locally-started runs were removed;
            // nothing sends run-id-carrying callbacks anymore.) Either way the
            // supervisor validates the scope against actual pending approvals,
            // so a forged run id grants nothing.
            let run_id = self
                .threads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&thread_key(gateway, thread_id))
                .cloned();
            let Some(run_id) = run_id else {
                self.push_outbound(
                    gateway,
                    thread_id,
                    "error: no run for this conversation".into(),
                );
                return;
            };
            let result = if grant {
                sup.grant(&run_id, scope)
            } else {
                sup.deny(&run_id, scope)
            };
            match result {
                Ok(()) => self.after_approval(gateway, thread_id, &run_id, scope, grant),
                Err(e) => self.push_outbound(gateway, thread_id, format!("error: {e}")),
            }
        }

        /// The decision is recorded: ack the user, and on grant hand the
        /// parked run to the resume hook so it actually continues. (Granting
        /// without resuming left runs parked forever - the ack lied.)
        fn after_approval(
            &self,
            gateway: &str,
            thread_id: &str,
            run_id: &str,
            scope: &str,
            grant: bool,
        ) {
            self.push_outbound(
                gateway,
                thread_id,
                if grant {
                    format!("granted {scope}")
                } else {
                    format!("denied {scope}")
                },
            );
            if grant {
                self.resume_run(gateway, thread_id, run_id);
            }
        }

        /// Hand a grant to the resume hook. The hook - not the daemon thread
        /// decides how the resume runs; the default spawns a background thread
        /// so the poll loop never blocks on the model.
        fn resume_run(&self, gateway: &str, thread_id: &str, run_id: &str) {
            (self.resume)(ResumeRequest {
                gateway: gateway.to_string(),
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
            });
        }
    }

    /// `pantheon gateway [start|stop|restart|status|run]`
    ///
    /// `run` is the foreground process the other subcommands wrap in a service
    /// manager. It is the real gateway: threads, transports, and the outbox
    /// drain. `start` writes a unit and lets the OS supervise it so the bot
    /// survives a logout or reboot, which a bare backgrounded process does not.
    pub fn cmd_gateway(args: &[String]) {
        match args.get(2).map(String::as_str) {
            None | Some("run") | Some("foreground") => run_gateway_foreground(),
            Some("start") => service_ctl(ServiceAction::Start),
            Some("stop") => service_ctl(ServiceAction::Stop),
            Some("restart") => service_ctl(ServiceAction::Restart),
            Some("status") => service_ctl(ServiceAction::Status),
            Some("help") | Some("--help") | Some("-h") => {
                println!(
                    "pantheon gateway [start|stop|restart|status|run]\n\n\
                     Run the always-on channel daemon in the foreground, or control\n\
                     its installed service. With no subcommand, the daemon runs.\n\n\
                     Tokens resolve through the secrets broker: process env\n\
                     wins, then <data_dir>/gateway.env (per-channel\n\
                     `token_secret`, falling back to the platform's standard\n\
                     token env var). A missing token disables that surface.\n\
                     At least one surface must be configured."
                );
            }
            Some(other) => {
                eprintln!("gateway: unknown subcommand '{other}'");
                eprintln!("usage: pantheon gateway [start|stop|restart|status|run]");
                std::process::exit(2);
            }
        }
    }

    /// One gateway chat surface: a named channel bound to a platform, a bot
    /// token, and the agent profile that serves it.
    #[derive(Clone)]
    struct ChannelSpec {
        /// Channel name - keys outbound queues, cursor files, and thread ids.
        name: String,
        /// `"telegram"` or `"discord"`.
        platform: String,
        /// Bot token (never logged).
        token: String,
        /// Agent profile serving this channel; `None` = default resolution.
        profile: Option<String>,
        /// Voice replies on this channel.
        voice_replies: bool,
    }

    /// A `Channel` that answers to a configured name. Two bots on the same
    /// platform are otherwise indistinguishable to the daemon plumbing
    /// (`TelegramChannel::name()` is the hardcoded `"telegram"`); the wrapper
    /// gives each its channel name for routing, claiming, and logs while the
    /// inner channel keeps doing the real transport work.
    struct NamedChannel {
        name: String,
        inner: Arc<dyn pantheon_gateway::Channel>,
    }

    impl NamedChannel {
        fn new(name: String, inner: Arc<dyn pantheon_gateway::Channel>) -> Arc<Self> {
            Arc::new(Self { name, inner })
        }
    }

    impl pantheon_gateway::Channel for NamedChannel {
        fn name(&self) -> &str {
            &self.name
        }
        fn send(
            &self,
            envelope: pantheon_gateway::ChannelEnvelope,
        ) -> Result<(), pantheon_gateway::ChannelError> {
            self.inner.send(envelope)
        }
        fn poll(&self) -> Vec<pantheon_gateway::ChannelEvent> {
            self.inner.poll()
        }
    }

    /// Build the channel plan from config + secrets.
    ///
    /// Explicit mode: any `[gateway.channels.<name>]` entry that declares a
    /// platform becomes its own channel - one poller per entry, so two
    /// entries can both be `telegram` with different bot tokens and different
    /// agent profiles. The token comes from `token_secret` through the
    /// secrets broker (process env wins, then `<data_dir>/gateway.env`),
    /// falling back to the platform's standard token.
    ///
    /// Legacy mode: no entry declares a platform - today's behavior exactly:
    /// at most one `telegram` and one `discord` channel from the standard
    /// tokens, served by the default profile.
    ///
    /// A channel starts only when its token is present AND the allowlist is
    /// non-empty: a token without an allowlist would hand the host to anyone
    /// who finds the bot.
    fn channel_specs(
        cfg: &Option<crate::config::Config>,
        data_dir: &Path,
        discord_token: Option<String>,
        telegram_token: Option<String>,
        allow: &HashSet<String>,
    ) -> Vec<ChannelSpec> {
        let gated = !allow.is_empty();
        let mut explicit: Vec<(&String, &pantheon_api::config::GatewayChannelSection)> = cfg
            .as_ref()
            .and_then(|c| c.gateway.as_ref())
            .map(|g| {
                g.channels
                    .iter()
                    .filter(|(_, ch)| ch.is_explicit())
                    .collect()
            })
            .unwrap_or_default();
        explicit.sort_by(|a, b| a.0.cmp(b.0));
        if !explicit.is_empty() {
            let mut specs = Vec::new();
            for (name, ch) in explicit {
                let Some(platform) = ch.resolved_platform(name) else {
                    // validate() flags the bad platform; skip rather than guess.
                    continue;
                };
                let token = ch
                    .token_secret
                    .as_deref()
                    .and_then(|s| pantheon_secrets::gateway_token(data_dir, s))
                    .map(|v| v.expose().to_string())
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| match platform.as_str() {
                        "telegram" => telegram_token.clone(),
                        "discord" => discord_token.clone(),
                        _ => None,
                    });
                match token {
                    Some(token) if gated => specs.push(ChannelSpec {
                        name: (*name).clone(),
                        platform,
                        token,
                        profile: ch.profile.clone(),
                        voice_replies: ch.voice_replies,
                    }),
                    _ => eprintln!(
                        "gateway: channel '{name}' skipped ({}).",
                        if !gated {
                            "the allowlist is empty"
                        } else {
                            "no token found"
                        }
                    ),
                }
            }
            return specs;
        }
        // Legacy shape: exactly today's behavior.
        let plan = pantheon_gateway::ChannelPlan::from_env(
            discord_token.as_deref(),
            telegram_token.as_deref(),
            allow,
        );
        if plan.is_empty() {
            eprintln!("gateway: {}", pantheon_gateway::channels_disabled_note());
        }
        let voice_replies = |name: &str| {
            cfg.as_ref()
                .and_then(|c| c.gateway.as_ref())
                .and_then(|g| g.channels.get(name))
                .map(|c| c.voice_replies)
                .unwrap_or(false)
        };
        let mut specs = Vec::new();
        if let Some(token) = telegram_token.filter(|_| plan.telegram) {
            specs.push(ChannelSpec {
                name: "telegram".to_string(),
                platform: "telegram".to_string(),
                token,
                profile: None,
                voice_replies: voice_replies("telegram"),
            });
        }
        if let Some(token) = discord_token.filter(|_| plan.discord) {
            specs.push(ChannelSpec {
                name: "discord".to_string(),
                platform: "discord".to_string(),
                token,
                profile: None,
                voice_replies: voice_replies("discord"),
            });
        }
        specs
    }

    /// Cursor file for a channel. Legacy channels keep their historical names
    /// so cursors survive the upgrade; explicit channels get `cursor-<name>`.
    fn cursor_file(state_dir: &Path, spec: &ChannelSpec) -> PathBuf {
        match spec.name.as_str() {
            "telegram" => state_dir.join("tg-cursor"),
            "discord" => state_dir.join("discord-cursor"),
            _ => state_dir.join(format!("cursor-{}", spec.name)),
        }
    }

    /// One supervised poller: the spec (cloneable, so the supervisor can
    /// respawn it), its stop flag, its heartbeat, its watchdog budget, and
    /// the live thread handle (replaced on respawn; the old handle is
    /// dropped, detaching the abandoned thread).
    struct SupervisedPoller {
        spec: ChannelSpec,
        stop: Arc<std::sync::atomic::AtomicBool>,
        heartbeat: Arc<pantheon_gateway::GatewayHeartbeat>,
        watchdog: pantheon_gateway::GatewayWatchdog,
        handle: Option<std::thread::JoinHandle<()>>,
        supervised: bool,
    }

    /// Spawn one channel's poller thread: the transport + daemon loop for
    /// that channel. The daemon touches `heartbeat` each loop iteration;
    /// the supervisor watches it and restarts the poller at most once when
    /// it goes quiet (see [`pantheon_gateway::watchdog`]).
    ///
    /// The Discord poller spawns an inner gateway thread and joins it after
    /// the daemon loop returns, so abandoning a Discord poller abandons
    /// both threads together - the inner thread is never orphaned on its
    /// own.
    #[allow(clippy::too_many_arguments)]
    fn spawn_poller(
        spec: ChannelSpec,
        queue: Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>,
        sink: Arc<RuntimeSink>,
        voice: pantheon_gateway::VoicePipes,
        state_dir: PathBuf,
        stop: Arc<std::sync::atomic::AtomicBool>,
        heartbeat: Arc<pantheon_gateway::GatewayHeartbeat>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let surface = SurfaceSink {
                inner: sink.as_ref(),
                gateway: &spec.name,
            };
            let cursor = cursor_file(&state_dir, &spec);
            match spec.platform.as_str() {
                "telegram" => {
                    let channel = Arc::new(
                        pantheon_gateway::TelegramChannel::rest(&spec.token).with_voice(voice),
                    );
                    let named = NamedChannel::new(spec.name.clone(), channel.clone() as Arc<_>);
                    let transport =
                        Arc::new(pantheon_gateway::TelegramRestTransport::new(&spec.token));
                    let daemon =
                        pantheon_gateway::ChannelDaemon::new(cursor).with_heartbeat(heartbeat);
                    daemon.run(
                        vec![named as Arc<dyn pantheon_gateway::Channel>],
                        Some((&channel, &transport)),
                        &surface,
                        queue.as_ref(),
                        &|| stop.load(std::sync::atomic::Ordering::Acquire),
                    );
                }
                "discord" => {
                    // The Discord channel owns both directions: the gateway
                    // websocket feeds its inbox, the daemon drains it, and replies
                    // go out over Discord REST. (It used to be a MemoryChannel
                    // whose outbox nobody read - replies accumulated in RAM and
                    // the user got silence.)
                    let discord = Arc::new(
                        pantheon_gateway::DiscordChannel::rest(&spec.token).with_voice(voice),
                    );
                    let named = NamedChannel::new(spec.name.clone(), discord.clone() as Arc<_>);
                    let gateway =
                        pantheon_gateway::discord_gateway::DiscordGateway::new(&spec.token);
                    let gw_inbox = discord.clone();
                    let gw_stop = stop.clone();
                    let gw = std::thread::spawn(move || {
                        gateway.run(&gw_inbox, &|| {
                            gw_stop.load(std::sync::atomic::Ordering::Acquire)
                        })
                    });
                    // Drain thread: route inbox events through the daemon plumbing.
                    let daemon =
                        pantheon_gateway::ChannelDaemon::new(cursor).with_heartbeat(heartbeat);
                    daemon.run(
                        vec![named as Arc<dyn pantheon_gateway::Channel>],
                        None,
                        &surface,
                        queue.as_ref(),
                        &|| stop.load(std::sync::atomic::Ordering::Acquire),
                    );
                    let _ = gw.join();
                }
                other => {
                    eprintln!(
                        "gateway: channel '{}': unsupported platform '{other}'; skipping",
                        spec.name
                    );
                }
            }
        })
    }

    /// Persistent watchdog report: the failure record for a channel that
    /// hung twice. The operator (and the agent, when asked about gateway
    /// health) reads `gateway/watchdog-<channel>.json`; the file is the
    /// report - the supervisor deliberately does not restart a third time
    /// or fabricate a conversation the channel cannot deliver to.
    fn write_watchdog_report(state_dir: &Path, channel: &str, last_heartbeat_ms: u64) {
        let body = format!(
            "{{\n  \"channel\": {channel:?},\n  \"status\": \"reported\",\n  \"restarted_once\": true,\n  \"last_heartbeat_ms\": {last_heartbeat_ms},\n  \"reported_at_ms\": {}\n}}\n",
            pantheon_gateway::watchdog::now_ms()
        );
        let path = state_dir.join(format!("watchdog-{channel}.json"));
        if let Err(e) = std::fs::write(&path, body) {
            eprintln!("gateway watchdog: channel '{channel}': cannot write report: {e}");
        }
    }

    fn run_gateway_foreground() {
        // The scheduler loop starts unconditionally: a scheduler-only install
        // (no bot tokens) is the main always-on use case, and a service that
        // exits here would crash-loop under the service manager. Chat surfaces
        // start only when their token and the allowlist are present; otherwise
        // the process warns and keeps running.
        let data_dir = crate::terminal::data_dir();
        // Single-flight: one gateway per data dir. A second `gateway run`
        // would double-fire the scheduler and double-deliver webhook-driven
        // channels, so it exits instead of starting. The lock is OS-held for
        // the process lifetime and releases if the process dies, so there is
        // no stale state to clean up.
        let _gateway_singleton = match pantheon_gateway::rendezvous::try_acquire(&data_dir) {
            Ok(pantheon_gateway::rendezvous::AcquireOutcome::Acquired(lock)) => lock,
            Ok(pantheon_gateway::rendezvous::AcquireOutcome::HeldBy { pid }) => {
                match pid {
                    Some(pid) => eprintln!("gateway: already running (pid {pid})"),
                    None => eprintln!("gateway: already running"),
                }
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("gateway: cannot take singleton lock: {e}");
                std::process::exit(1);
            }
        };
        // MCP boot health check: probe enabled MCP servers at startup and log
        // unreachable ones, so a broken server is visible instead of silently
        // absent from the tool list.
        pantheon_gateway::mcp_boot::check_and_log(&data_dir, std::time::Duration::from_secs(10));
        // Tokens come from the secrets store (<data_dir>/gateway.env, written
        // by the Full Setup wizard's Gateway screen) with process env vars
        // winning when both are set.
        let (discord_token, telegram_token, allow) =
            pantheon_gateway::read_channel_tokens(&data_dir);
        // The gateway runs the same policy the config names. It used to read
        // PANTHEON_GATEWAY_POLICY only, so a user with a working `policy =
        // "reader"` config still got coder on Discord and Telegram.
        let gw_cfg = crate::config::Config::load_or_report(&data_dir);
        // One poller per channel: in explicit mode several channels can share
        // a platform (two Telegram bots, two profiles); in legacy mode this is
        // exactly the old telegram/discord pair.
        let specs = channel_specs(&gw_cfg, &data_dir, discord_token, telegram_token, &allow);
        // One outbound queue per channel. The old single shared queue let the
        // Telegram daemon grab Discord replies (send fails → infinite requeue)
        // and the Discord daemon swallow Telegram ones; keyed queues make
        // cross-channel pickup impossible.
        let mut queues: HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>> =
            HashMap::new();
        for spec in &specs {
            queues.insert(spec.name.clone(), Arc::new(Mutex::new(Vec::new())));
        }
        // Voice for the gateway channels: the [stt]/[tts] backends the setup
        // wizard writes, double-gated by the [tools] voice toggle (see
        // pantheon_gateway::channel_voice::VoicePipes). voice_replies is
        // per-channel ([gateway.channels.<name>]), default off - text stays
        // the default.
        let secrets = crate::config::chat_secrets(gw_cfg.as_ref());
        // Voice pipes per channel: the [stt]/[tts] backends the setup wizard
        // writes, double-gated by the [tools] voice toggle (see
        // pantheon_gateway::channel_voice::VoicePipes). voice_replies is
        // per-channel ([gateway.channels.<name>]), default off - text stays
        // the default.
        let mut voices: HashMap<String, pantheon_gateway::VoicePipes> = HashMap::new();
        for spec in &specs {
            let pipes = pantheon_gateway::VoicePipes::from_config(
                gw_cfg.as_ref().and_then(|c| c.tools.as_ref()),
                gw_cfg.as_ref().and_then(|c| c.stt.as_ref()),
                gw_cfg.as_ref().and_then(|c| c.tts.as_ref()),
                &secrets,
                spec.voice_replies,
            );
            eprintln!("gateway: voice {}({})", spec.name, pipes.describe());
            voices.insert(spec.name.clone(), pipes);
        }
        // The config-default policy. PANTHEON_GATEWAY_POLICY still wins when
        // set (see RuntimeSink::policy_for); a channel with an agent otherwise
        // runs its agent's resolved preset.
        let policy = crate::config_schema::policy_for_config(&gw_cfg);
        // One agent runtime per channel: each channel is served by its own
        // profile, so a message on bot A is answered by profile A with A's
        // memory, and bot B never sees it. An unknown profile fails fast with
        // a clear error instead of silently serving the wrong agent. Channels
        // with no resolvable profile stay anonymous, as before.
        let mut agents: HashMap<String, pantheon_runtime::AgentRuntime> = HashMap::new();
        if let Some(cfg) = gw_cfg.as_ref() {
            match pantheon_runtime::Supervisor::open(data_dir.clone()) {
                Ok(supervisor) => {
                    for spec in &specs {
                        match cfg.resolve_profile(spec.profile.as_deref()) {
                            Ok(Some(effective)) => {
                                let built = cfg
                                    .profile_registry()
                                    .map_err(pantheon_runtime::profile_err)
                                    .and_then(|reg| {
                                        let caps = crate::session::swarm_caps_for(cfg, &effective);
                                        pantheon_runtime::AgentRuntime::with_caps(
                                            supervisor.clone(),
                                            reg,
                                            effective,
                                            data_dir.clone(),
                                            caps,
                                        )
                                    });
                                match built {
                                    Ok(agent) => {
                                        agents.insert(spec.name.clone(), agent);
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "gateway: channel '{}': cannot start agent: {e}",
                                            spec.name
                                        );
                                        std::process::exit(1);
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!(
                                    "gateway: channel '{}': {}",
                                    spec.name,
                                    pantheon_runtime::profile_err(e)
                                );
                                std::process::exit(1);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("gateway: supervisor open failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        // Extension manager for `pre_gateway_dispatch`: same load path as
        // the terminal's `load_mgr` (data dir + project root + the
        // extensions dir, with loud warnings on a broken plugin dir so a
        // broken directory never looks like an empty one). With no
        // plugins loaded the fire call below is a no-op.
        let hooks = {
            let mut m = ExtensionManager::new(RunnerConfig::default())
                .with_data_dir(data_dir.clone())
                .with_project_root(std::env::current_dir().unwrap_or_else(|_| data_dir.clone()));
            let d = crate::terminal::ext_dir();
            if d.exists() {
                if let Err(e) = m.load_dir(&d) {
                    eprintln!(
                        "warning: could not load extensions from {}: {e}",
                        d.display()
                    );
                }
                for err in m.load_errors() {
                    eprintln!("warning: {err}");
                }
            }
            Arc::new(m)
        };
        let sink = Arc::new(
            RuntimeSink::new(data_dir.clone(), policy, queues.clone(), Some(allow))
                .with_agents(agents)
                .with_hooks(hooks),
        );
        let state_dir = data_dir.join("gateway");
        let _ = std::fs::create_dir_all(&state_dir);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

        // One poller thread per channel. Two channels on the same platform
        // are fully independent: separate transports, separate cursors,
        // separate queues, and (via the sink) separate agents. Each poller
        // gets its own stop flag and heartbeat, watched by the supervisor
        // thread below: a poller that stops ticking is restarted ONCE, and
        // a second hang is reported, never restarted again (see
        // pantheon_gateway::watchdog - no infinite restart loop).
        let mut supervised: Vec<SupervisedPoller> = Vec::new();
        for spec in specs {
            let queue = match queues.get(&spec.name).cloned() {
                Some(q) => q,
                None => continue,
            };
            let voice = voices.remove(&spec.name).unwrap_or_default();
            let sink = sink.clone();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let heartbeat = Arc::new(pantheon_gateway::GatewayHeartbeat::new());
            let handle = spawn_poller(
                spec.clone(),
                queue,
                sink,
                voice,
                state_dir.clone(),
                stop.clone(),
                heartbeat.clone(),
            );
            supervised.push(SupervisedPoller {
                spec,
                stop,
                heartbeat,
                watchdog: pantheon_gateway::GatewayWatchdog::new(pantheon_gateway::HANG_BUDGET),
                handle: Some(handle),
                supervised: true,
            });
        }

        // Hung-gateway supervisor: ticks every 10 s (the hang budget is
        // 90 s, so detection lags at most ~100 s). On a first breach it
        // abandons the hung thread - it cannot be killed from outside, so
        // its stop flag is set and its handle dropped - and spawns a
        // replacement poller with rebuilt voice pipes. On a second breach
        // it reports (loud log + persistent report file) and stops
        // supervising that channel: a twice-hung channel is broken, and
        // restarting it forever would flap cursors and hide the breakage.
        {
            let queues = queues.clone();
            let sink = sink.clone();
            let state_dir = state_dir.clone();
            // Voice pipes are not cloneable (boxed providers), so the
            // supervisor rebuilds them from the same config sections the
            // initial spawn used.
            let voice_tools = gw_cfg.as_ref().and_then(|c| c.tools.clone());
            let voice_stt = gw_cfg.as_ref().and_then(|c| c.stt.clone());
            let voice_tts = gw_cfg.as_ref().and_then(|c| c.tts.clone());
            let voice_secrets = secrets.clone();
            handles.push(std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                for p in supervised.iter_mut() {
                    if !p.supervised {
                        continue;
                    }
                    let last = p.heartbeat.last_ms();
                    if last > 0 {
                        p.watchdog.heartbeat_at(last);
                    }
                    match p.watchdog.check() {
                        pantheon_gateway::WatchdogAction::Ok => {}
                        pantheon_gateway::WatchdogAction::Restart => {
                            eprintln!(
                                "gateway watchdog: channel '{}' hung (no heartbeat for >90s); restarting its poller ONCE; the hung thread is abandoned, not killed",
                                p.spec.name
                            );
                            p.stop.store(true, std::sync::atomic::Ordering::Release);
                            let queue = match queues.get(&p.spec.name).cloned() {
                                Some(q) => q,
                                None => {
                                    p.supervised = false;
                                    continue;
                                }
                            };
                            let pipes = pantheon_gateway::VoicePipes::from_config(
                                voice_tools.as_ref(),
                                voice_stt.as_ref(),
                                voice_tts.as_ref(),
                                &voice_secrets,
                                p.spec.voice_replies,
                            );
                            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let heartbeat = Arc::new(pantheon_gateway::GatewayHeartbeat::new());
                            // The replacement just started: it is not hung.
                            p.watchdog
                                .heartbeat_at(pantheon_gateway::watchdog::now_ms());
                            let handle = spawn_poller(
                                p.spec.clone(),
                                queue,
                                sink.clone(),
                                pipes,
                                state_dir.clone(),
                                stop.clone(),
                                heartbeat.clone(),
                            );
                            let _ = p.handle.replace(handle);
                            p.stop = stop;
                            p.heartbeat = heartbeat;
                        }
                        pantheon_gateway::WatchdogAction::Report => {
                            eprintln!(
                                "gateway watchdog: channel '{}' hung AGAIN after its one restart; NOT restarting (no infinite restart loop); report written to gateway/watchdog-{}.json - manual intervention needed",
                                p.spec.name, p.spec.name
                            );
                            write_watchdog_report(
                                &state_dir,
                                &p.spec.name,
                                p.heartbeat.last_ms(),
                            );
                            p.supervised = false;
                        }
                    }
                }
            }));
        }

        // Outbox drain thread: picks up replies queued by
        // `pantheon run --deliver <channel>` from another process and files
        // them into that surface's outbound queue. Without this, `--deliver`
        // writes a file nobody reads.
        {
            let queues = queues.clone();
            let stop = stop.clone();
            let dd = data_dir.clone();
            handles.push(std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    let (msgs, bad) = drain_outbound(&dd);
                    for b in bad {
                        eprintln!("gateway outbox: {b}");
                    }
                    for m in msgs {
                        match queues.get(&m.gateway) {
                            Some(q) => q
                                .lock()
                                .map(|mut qq| qq.push(m))
                                .unwrap_or_else(|_| eprintln!("gateway outbox: queue poisoned")),
                            None => eprintln!(
                                "gateway outbox: unknown gateway '{}' for thread {}; dropping",
                                m.gateway, m.to_conversation,
                            ),
                        }
                    }
                    // 2s is frequent enough that a queued reply feels immediate
                    // and rare enough to cost nothing when the queue is empty.
                    for _ in 0..20 {
                        if stop.load(std::sync::atomic::Ordering::Acquire) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }));
        }

        eprintln!("gateway running; Ctrl+C to stop");
        // Scheduler loop: the gateway service is also the always-on scheduler.
        // It ticks due jobs on the same claim ledger and job store as
        // `pantheon schedule tick`, so the two can never double-fire an
        // occurrence (the claim is an atomic first-wins INSERT). Detached: the
        // loop never returns, and the process ends with the rest on Ctrl+C.
        {
            let dd = data_dir.clone();
            std::thread::spawn(move || crate::schedule::run_scheduler_loop(&dd));
        }
        for h in handles {
            let _ = h.join();
        }
    }
    /// A reply produced by `pantheon run --deliver` that the gateway has not
    /// sent yet.
    ///
    /// The gateway's own outbound queue lives in memory, so a reply produced by
    /// a separate process would be lost the moment that process exits. This is
    /// the on-disk hand-off: `run --deliver` appends one line, the running
    /// gateway drains it on its next tick. One JSON object per line, so a
    /// partially written tail is discarded rather than corrupting the queue.
    pub fn outbox_path(data_dir: &Path) -> PathBuf {
        data_dir.join("gateway").join("outbox.jsonl")
    }

    /// Gateway service state for in-process callers (the TUI `/gateway`
    /// command). Read-only: unlike `service_ctl`, asking never installs,
    /// starts, or prints anything.
    pub(crate) struct GatewayStatus {
        /// A service unit exists (systemd unit on Linux, plist on macOS).
        pub installed: bool,
        /// The manager reports the service active.
        pub active: bool,
        /// Queued outbound messages awaiting delivery.
        pub outbox_pending: usize,
    }

    pub(crate) fn gateway_status() -> GatewayStatus {
        let data_dir = crate::terminal::data_dir();
        // Shared cross-platform inspection: the same code `gateway status`
        // uses, so the TUI's `/gateway` line can never disagree with the CLI.
        let st = pantheon_gateway::service_status();
        let outbox_pending = std::fs::read_to_string(outbox_path(&data_dir))
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        GatewayStatus {
            installed: st.installed.is_some(),
            active: st.running,
            outbox_pending,
        }
    }

    pub fn enqueue_outbound(
        data_dir: &Path,
        to_conversation: &str,
        text: &str,
        gateway: &str,
    ) -> Result<(), String> {
        use std::io::Write;
        let path = outbox_path(data_dir);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let line = serde_json::json!({
            "to": to_conversation,
            "text": text,
            "gateway": gateway,
            "queued_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        });
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        writeln!(f, "{line}").map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Take every queued reply, leaving the file empty. Called on each gateway
    /// tick. A malformed line is skipped and reported rather than aborting the
    /// drain, so one bad write cannot wedge delivery.
    pub fn drain_outbound(
        data_dir: &Path,
    ) -> (Vec<pantheon_gateway::OutboundMessage>, Vec<String>) {
        let path = outbox_path(data_dir);
        // No outbox file yet is the common case, not an error.
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return (Vec::new(), Vec::new());
        };
        let mut msgs = Vec::new();
        let mut bad = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => {
                    let to = v.get("to").and_then(|x| x.as_str()).unwrap_or_default();
                    let text = v.get("text").and_then(|x| x.as_str()).unwrap_or_default();
                    let gateway = v
                        .get("gateway")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default();
                    if to.is_empty() || text.is_empty() {
                        bad.push(format!("line {}: missing to/text", i + 1));
                        continue;
                    }
                    msgs.push(pantheon_gateway::OutboundMessage {
                        to_conversation: to.to_string(),
                        text: text.to_string(),
                        gateway: gateway.to_string(),
                        attempts: 0,
                    });
                }
                Err(e) => bad.push(format!("line {}: {e}", i + 1)),
            }
        }
        // Truncate only after a successful read, so a read error leaves the
        // queue intact instead of silently dropping undelivered replies.
        if let Err(e) = std::fs::write(&path, b"") {
            bad.push(format!("truncate {}: {e}", path.display()));
        }
        (msgs, bad)
    }
    // ── service supervision ────────────────────────────────────────────────────
    // Thin over the cross-platform machinery in `pantheon_gateway::service`
    // the same code `pantheon init` uses, so the two can never disagree about
    // how the service is installed. systemd on Linux (cron `@reboot`
    // fallback), launchd on macOS, Task Scheduler on Windows.

    #[derive(Clone, Copy)]
    enum ServiceAction {
        Start,
        Stop,
        Restart,
        Status,
    }

    /// Install (or converge) the background service and confirm it settled.
    /// Shared by `start` and by `restart` when nothing is installed yet.
    fn service_start(data_dir: &std::path::Path) {
        let exe = match pantheon_gateway::self_exe() {
            Some(e) => e,
            None => {
                eprintln!("gateway: cannot resolve the pantheon binary path for ExecStart");
                std::process::exit(1);
            }
        };
        match pantheon_gateway::install_service(data_dir, &exe) {
            pantheon_gateway::InstallOutcome::Installed { mechanism, changed } => {
                if changed {
                    println!("gateway: service installed via {}.", mechanism.as_str());
                } else {
                    println!(
                        "gateway: service already installed via {}; ensured running.",
                        mechanism.as_str()
                    );
                }
                confirm_active(mechanism);
            }
            pantheon_gateway::InstallOutcome::Unavailable { note } => {
                // Fail open, like `pantheon init`: no manager is a manual setup.
                println!("gateway: {note}");
            }
            pantheon_gateway::InstallOutcome::Failed { mechanism, error } => {
                eprintln!(
                    "gateway: install via {} failed: {error}",
                    mechanism.as_str()
                );
                std::process::exit(1);
            }
        }
    }

    /// The manager accepting the start request is not the process staying up:
    /// poll briefly so a crash-loop is reported here instead of as a bot that
    /// never answers. Mechanisms that only fire later (cron `@reboot`, logon
    /// tasks) are reported honestly instead of polled.
    fn confirm_active(mechanism: pantheon_gateway::ServiceMechanism) {
        use pantheon_gateway::ServiceMechanism as M;
        match mechanism {
            M::Cron => {
                println!("gateway: installed; the @reboot entry starts it at next boot.");
                return;
            }
            M::TaskScheduler => {
                println!("gateway: installed; it starts at next logon.");
                println!("         run `pantheon gateway run` in the foreground for this session.");
                return;
            }
            M::Systemd | M::Launchd => {}
            M::None => return,
        }
        for _ in 0..12 {
            if pantheon_gateway::service_status().running {
                println!("gateway: active");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        eprintln!(
            "gateway: installed but not reporting active - it may be crash-looping.\n\
             most common cause: the token or allowlist is not visible to the\n\
             service. Check the service logs."
        );
        std::process::exit(1);
    }

    fn service_ctl(action: ServiceAction) {
        let data_dir = crate::terminal::data_dir();
        match action {
            ServiceAction::Start => service_start(&data_dir),
            ServiceAction::Stop => match pantheon_gateway::stop_service() {
                pantheon_gateway::StopOutcome::Stopped { mechanism } => {
                    println!("gateway: stopped ({}).", mechanism.as_str());
                }
                pantheon_gateway::StopOutcome::NotInstalled => {
                    eprintln!("gateway: no service installed");
                    std::process::exit(1);
                }
                pantheon_gateway::StopOutcome::Noop { note, .. } => println!("gateway: {note}"),
                pantheon_gateway::StopOutcome::Failed { mechanism, error } => {
                    eprintln!("gateway: stop via {} failed: {error}", mechanism.as_str());
                    std::process::exit(1);
                }
            },
            // Restart used to install a missing unit too; keep that.
            ServiceAction::Restart => match pantheon_gateway::restart_service() {
                pantheon_gateway::RestartOutcome::Restarted { mechanism } => {
                    println!("gateway: restarted ({}).", mechanism.as_str());
                }
                pantheon_gateway::RestartOutcome::NotInstalled => service_start(&data_dir),
                pantheon_gateway::RestartOutcome::Noop { note, .. } => println!("gateway: {note}"),
                pantheon_gateway::RestartOutcome::Failed { mechanism, error } => {
                    eprintln!(
                        "gateway: restart via {} failed: {error}",
                        mechanism.as_str()
                    );
                    std::process::exit(1);
                }
            },
            // Status never installs: asking whether it is running must not have
            // the side effect of installing it. Nor does it need tokens: a
            // scheduler-only install is a healthy install.
            ServiceAction::Status => {
                print_schedule_queue();
                let st = pantheon_gateway::service_status();
                match st.installed {
                    Some(m) => println!(
                        "gateway: {} ({}).",
                        if st.running {
                            "active"
                        } else {
                            "installed, not active"
                        },
                        m.as_str()
                    ),
                    None => println!("gateway: not installed (pantheon gateway start)"),
                }
                // Singleton state: whether a `gateway run` process currently
                // holds the data-dir lock, independent of the service manager
                // above (a foreground run is not a unit).
                match pantheon_gateway::rendezvous::try_acquire(&data_dir) {
                    Ok(pantheon_gateway::rendezvous::AcquireOutcome::Acquired(_)) => {
                        println!("singleton: not running")
                    }
                    Ok(pantheon_gateway::rendezvous::AcquireOutcome::HeldBy { pid }) => match pid {
                        Some(pid) => println!("singleton: running (pid {pid})"),
                        None => println!("singleton: running"),
                    },
                    Err(e) => println!("singleton: unknown ({e})"),
                }
            }
        }
    }

    /// Scheduler queue summary for `gateway status`: due jobs and next fire,
    /// loaded read-only from the same job store the tick loop uses. Never
    /// installs or mutates anything.
    fn print_schedule_queue() {
        let data_dir = crate::terminal::data_dir();
        match crate::schedule::load_schedulable(&data_dir) {
            Ok(jobs) => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                println!(
                    "schedule queue: {}",
                    pantheon_gateway::queue_summary(&jobs, now_ms)
                );
            }
            Err(e) => println!("schedule queue: unavailable ({e})"),
        }
    }

    #[cfg(test)]
    mod gateway_multi_agent_tests {
        use super::*;
        use pantheon_gateway::Channel as _;

        fn explicit_cfg() -> crate::config::Config {
            let mut cfg = crate::config::Config::default();
            let mut gw = pantheon_api::config::GatewaySection::default();
            gw.channels.insert(
                "bot-a".to_string(),
                pantheon_api::config::GatewayChannelSection {
                    platform: Some("telegram".to_string()),
                    token_secret: Some("BOT_A_TOKEN".to_string()),
                    profile: Some("support".to_string()),
                    voice_replies: false,
                },
            );
            gw.channels.insert(
                "bot-b".to_string(),
                pantheon_api::config::GatewayChannelSection {
                    platform: Some("telegram".to_string()),
                    token_secret: None,
                    profile: Some("coder".to_string()),
                    voice_replies: true,
                },
            );
            cfg.gateway = Some(gw);
            cfg
        }

        fn allow_one() -> HashSet<String> {
            HashSet::from(["123".to_string()])
        }

        /// Scratch data dir with a gateway.env holding BOT_A_TOKEN.
        fn data_dir_with_secret() -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-gw-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("gateway.env"), "BOT_A_TOKEN=secret-token-a\n").unwrap();
            dir
        }

        #[test]
        fn two_telegram_bots_get_separate_specs() {
            let data_dir = data_dir_with_secret();
            let specs = channel_specs(
                &Some(explicit_cfg()),
                &data_dir,
                None,
                Some("fallback-tg-token".to_string()),
                &allow_one(),
            );
            let _ = std::fs::remove_dir_all(&data_dir);
            assert_eq!(specs.len(), 2, "one poller per explicit channel");
            // Sorted by channel name for deterministic startup order.
            assert_eq!(specs[0].name, "bot-a");
            assert_eq!(specs[1].name, "bot-b");
            for s in &specs {
                assert_eq!(s.platform, "telegram");
            }
            // bot-a's token comes from its token_secret; bot-b falls back to
            // the platform's standard token.
            assert_eq!(specs[0].token, "secret-token-a");
            assert_eq!(specs[1].token, "fallback-tg-token");
            assert_ne!(specs[0].token, specs[1].token);
            // Each bot keeps its own profile.
            assert_eq!(specs[0].profile.as_deref(), Some("support"));
            assert_eq!(specs[1].profile.as_deref(), Some("coder"));
            assert!(!specs[0].voice_replies);
            assert!(specs[1].voice_replies);
        }

        #[test]
        fn legacy_shape_uses_env_tokens_and_default_profile() {
            let data_dir = std::env::temp_dir();
            let specs = channel_specs(
                &Some(crate::config::Config::default()),
                &data_dir,
                None,
                Some("tg-token".to_string()),
                &allow_one(),
            );
            assert_eq!(specs.len(), 1);
            assert_eq!(specs[0].name, "telegram");
            assert_eq!(specs[0].platform, "telegram");
            assert_eq!(specs[0].token, "tg-token");
            assert_eq!(specs[0].profile, None);
        }

        #[test]
        fn empty_allowlist_starts_no_channels() {
            let data_dir = std::env::temp_dir();
            let specs = channel_specs(
                &Some(explicit_cfg()),
                &data_dir,
                None,
                Some("tg-token".to_string()),
                &HashSet::new(),
            );
            assert!(
                specs.is_empty(),
                "a token without an allowlist starts nothing"
            );
        }

        #[test]
        fn no_tokens_means_no_specs() {
            let data_dir = std::env::temp_dir();
            let specs = channel_specs(&None, &data_dir, None, None, &allow_one());
            assert!(specs.is_empty());
        }

        #[test]
        fn thread_key_namespaces_by_channel() {
            // Two bots on the same platform hand out overlapping thread ids;
            // the namespaced keys must differ.
            let a = thread_key("bot-a", "42");
            let b = thread_key("bot-b", "42");
            assert_ne!(a, b);
            assert_eq!(thread_key("bot-a", "42"), thread_key("bot-a", "42"));
            assert!(a.contains("bot-a"));
        }

        #[test]
        fn named_channel_reports_the_configured_name() {
            let inner = Arc::new(pantheon_gateway::MemoryChannel::new("telegram"));
            let named = NamedChannel::new("bot-a".to_string(), inner);
            assert_eq!(named.name(), "bot-a");
            // The inner channel still does the transport work.
            assert!(named.poll().is_empty());
        }

        fn spec(name: &str, platform: &str) -> ChannelSpec {
            ChannelSpec {
                name: name.to_string(),
                platform: platform.to_string(),
                token: "t".to_string(),
                profile: None,
                voice_replies: false,
            }
        }

        /// Scratch sink whose only surface is `telegram`, with its outbound
        /// queue exposed so a test can read what the user would receive.
        fn sink_with_allow(
            allow: Option<HashSet<String>>,
        ) -> (
            RuntimeSink,
            Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>,
            PathBuf,
        ) {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-gw-allow-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let queue = Arc::new(Mutex::new(Vec::new()));
            let queues = HashMap::from([("telegram".to_string(), queue.clone())]);
            let sink = RuntimeSink::new(dir.clone(), Policy::coder(), queues, allow);
            (sink, queue, dir)
        }

        /// S4 regression: a sender not on the allowlist never reaches the
        /// runtime. The refusal is answered on the same conversation, and no
        /// run is opened for the thread (the sink returns before `open_session`,
        /// so there is no model call to make either).
        #[test]
        fn non_allowlisted_sender_is_refused_before_the_runtime() {
            let (sink, queue, dir) = sink_with_allow(Some(HashSet::from(["123".to_string()])));
            sink.on_message("telegram", "thread-1", Some("999"), "rm -rf /");
            let sent = queue.lock().unwrap();
            assert_eq!(sent.len(), 1, "exactly one refusal reply");
            assert!(
                sent[0].text.contains("not authorized"),
                "unexpected reply: {}",
                sent[0].text
            );
            drop(sent);
            assert!(
                sink.threads.lock().unwrap().is_empty(),
                "a refused sender must not open a run"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A bridge that strips sender identity is refused whenever an
        /// allowlist is active: "unknown" is not "trusted".
        #[test]
        fn missing_sender_identity_is_refused_when_allowlist_is_active() {
            let (sink, queue, dir) = sink_with_allow(Some(HashSet::from(["123".to_string()])));
            sink.on_message("telegram", "thread-1", None, "hello");
            let sent = queue.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert!(sent[0].text.contains("not authorized"));
            drop(sent);
            assert!(sink.threads.lock().unwrap().is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Approval callbacks are gated identically: a non-allowlisted sender
        /// gets no grant/deny path, so a leaked thread id is not enough to
        /// approve a parked run.
        #[test]
        fn non_allowlisted_sender_cannot_decide_an_approval() {
            let (sink, queue, dir) = sink_with_allow(Some(HashSet::from(["123".to_string()])));
            sink.on_approval(
                "telegram",
                "thread-1",
                Some("999"),
                None,
                "shell.exec",
                true,
            );
            let sent = queue.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert!(sent[0].text.contains("not authorized"));
            drop(sent);
            assert!(
                sink.threads.lock().unwrap().is_empty(),
                "no run may be created or resolved for a refused sender"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// No allowlist (local testing / a trusted single-user install) keeps
        /// the historical behaviour: the check is inert, not a silent deny.
        #[test]
        fn absent_allowlist_allows_any_sender() {
            let (sink, _queue, dir) = sink_with_allow(None);
            assert!(sink.allowed(Some("999")));
            assert!(sink.allowed(None));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn cursor_files_keep_legacy_names() {
            let dir = Path::new("/tmp/state");
            // Legacy channels keep their historical cursor names so cursors
            // survive the upgrade; explicit channels get cursor-<name>.
            assert_eq!(
                cursor_file(dir, &spec("telegram", "telegram")),
                dir.join("tg-cursor")
            );
            assert_eq!(
                cursor_file(dir, &spec("discord", "discord")),
                dir.join("discord-cursor")
            );
            assert_eq!(
                cursor_file(dir, &spec("bot-a", "telegram")),
                dir.join("cursor-bot-a")
            );
        }
    }
}

pub mod mcp {
    //! `pantheon mcp`: manage MCP servers.
    //!
    //! Two sources declare servers: `[mcp.servers.<name>]` in config.toml
    //! (primary) and the migration declarations at `<data_dir>/mcp/*.json`
    //! (fill in names the config section does not define). Both feed the
    //! launcher in `pantheon_mcp::manager`, which registers each server's
    //! tools as `mcp_<server>_<tool>` in the agent's tool registry.
    //!
    //! A server never runs without an explicit approval: `pantheon mcp
    //! approve <name>` prints exactly what the server is, shows the warning,
    //! and asks for confirmation. The approval binds the server's name,
    //! reported version, and a content hash - any upgrade or change
    //! invalidates it and the operator is asked again.
    //!
    //! `resources`, `templates`, `read`, `prompts`, and `prompt` are the
    //! operator view of the same MCP resource/prompt surface the agent sees
    //! as `mcp.resources.*` / `mcp.prompts.*` tools. They connect lazily and
    //! pass through the same approval gate.

    use crate::config;
    use crate::terminal::data_dir;
    use pantheon_api::approval::ApprovalStore;
    use pantheon_mcp::manager::{McpManager, McpServerSpec, McpTransport, ServerStatus};
    use pantheon_migration::{read_mcp_declarations, McpServer};
    use std::path::Path;

    /// Re-exported from `pantheon_migration` (single source of truth, shared
    /// with the dashboard).
    pub use pantheon_migration::server_readiness;

    /// Build a manager wired like the session's: the data-dir scope,
    /// secrets-backed env resolver, and resolved server specs.
    ///
    /// `pub(crate)` so the nightly repair adapter can drive the same manager
    /// the sessions use. Configured, not connected: no servers are launched.
    ///
    /// Policy note (operator-direct exception): this manager installs no
    /// capability policy (`policy = None`), unlike a session manager.
    /// A session's policy is its agent preset's, and there is no preset
    /// for a bare CLI verb; a CLI operator invoking `pantheon mcp ...`
    /// directly is the consenting party, and approval still gates every
    /// connect. Do not "fix" this by inventing a CLI policy.
    pub(crate) fn manager_for(dd: &Path) -> McpManager {
        let cfg = config::Config::load_or_report(dd);
        let secrets = config::chat_secrets(cfg.as_ref());
        let declarations = read_mcp_declarations(dd);
        let mcfg =
            config::resolve_mcp_section(cfg.as_ref().and_then(|c| c.mcp.as_ref()), &declarations);
        let m = McpManager::new(dd.to_path_buf());
        m.set_secrets(secrets.clone());
        m.set_env_resolver(std::sync::Arc::new(move |var: &str| {
            secrets
                .resolve(var)
                .ok()
                .flatten()
                .map(|v| v.expose().to_owned())
                .or_else(|| std::env::var(var).ok())
        }));
        // Thread the [mcp] master switch: when it is off the manager holds
        // no specs, so no CLI verb (resources, approve, login, ...) can
        // launch a server. Re-enable [mcp] to approve or log in again.
        if mcfg.enabled {
            m.configure(mcfg.servers);
        } else {
            m.configure(Vec::new());
        }
        m
    }

    /// One row of the merged server view: config section first, declarations
    /// fill names the section does not define.
    struct Row {
        name: String,
        origin: &'static str,
        spec: McpServerSpec,
        /// Readiness blocker from the declaration, if this row came from one.
        decl_blocker: Option<String>,
    }

    fn collect(dd: &Path) -> Vec<Row> {
        let cfg = config::Config::load_or_report(dd);
        let declarations = read_mcp_declarations(dd);
        let mcfg =
            config::resolve_mcp_section(cfg.as_ref().and_then(|c| c.mcp.as_ref()), &declarations);
        let mut rows: Vec<Row> = mcfg
            .servers
            .into_iter()
            .map(|spec| {
                let origin = if cfg
                    .as_ref()
                    .and_then(|c| c.mcp.as_ref())
                    .is_some_and(|m| m.servers.contains_key(&spec.name))
                {
                    "config"
                } else {
                    "declaration"
                };
                Row {
                    name: spec.name.clone(),
                    origin,
                    spec,
                    decl_blocker: None,
                }
            })
            .collect();
        // Config entries the resolver skipped (disabled, unknown transport /
        // auth, missing command/url) still show in `list`, marked with their
        // blocker - previously they vanished with only an eprintln.
        if let Some(sec) = cfg.as_ref().and_then(|c| c.mcp.as_ref()) {
            for (name, e) in &sec.servers {
                if rows.iter().any(|r| r.name == *name) {
                    continue;
                }
                let blocker = if !e.enabled {
                    "disabled in config".to_string()
                } else {
                    e.problems(name)
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| "skipped by resolver".to_string())
                };
                rows.push(Row {
                    name: name.clone(),
                    origin: "config",
                    spec: McpServerSpec {
                        name: name.clone(),
                        transport: match e.transport.as_str() {
                            "sse" => McpTransport::Sse,
                            "http" => McpTransport::Http,
                            _ => McpTransport::Stdio,
                        },
                        command: e.command.clone(),
                        args: e.args.clone(),
                        env: e.env.clone(),
                        url: e.url.clone(),
                        enabled: e.enabled,
                        timeout: std::time::Duration::from_secs(30),
                        headers: Default::default(),
                        auth: pantheon_mcp::McpAuth::parse(e.auth.as_deref())
                            .unwrap_or(pantheon_mcp::McpAuth::None),
                        scopes: e.scopes.clone(),
                        client_id: e.client_id.clone(),
                    },
                    decl_blocker: Some(blocker),
                });
            }
        }
        // Declarations that resolve_mcp_section skipped (disabled or
        // malformed) still show in `list`, marked with their blocker.
        for d in &declarations {
            for s in &d.servers {
                if rows.iter().any(|r| r.name == s.name) {
                    continue;
                }
                rows.push(Row {
                    name: s.name.clone(),
                    origin: "declaration",
                    spec: decl_spec(s),
                    decl_blocker: Some(decl_blocker(s)),
                });
            }
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Best-effort spec for a declaration row that the resolver skipped.
    fn decl_spec(s: &McpServer) -> McpServerSpec {
        McpServerSpec {
            name: s.name.clone(),
            transport: match s.transport.as_str() {
                "sse" => McpTransport::Sse,
                "http" => McpTransport::Http,
                _ => McpTransport::Stdio,
            },
            command: s.command.clone(),
            args: s.args.clone(),
            env: s
                .requires_env
                .iter()
                .map(|v| (v.clone(), format!("env:{v}")))
                .collect(),
            url: s.url.clone(),
            enabled: s.enabled,
            timeout: std::time::Duration::from_secs(30),
            headers: Default::default(),
            auth: pantheon_mcp::McpAuth::None,
            scopes: Vec::new(),
            client_id: None,
        }
    }

    fn decl_blocker(s: &McpServer) -> String {
        if !s.enabled {
            return "disabled in declaration".to_string();
        }
        server_readiness(s).unwrap_or_else(|| "skipped by resolver".to_string())
    }

    fn target_of(spec: &McpServerSpec) -> String {
        match spec.transport {
            McpTransport::Stdio => {
                let cmd = spec.command.as_deref().unwrap_or("-");
                if spec.args.is_empty() {
                    cmd.to_string()
                } else {
                    format!("{cmd} {}", spec.args.join(" "))
                }
            }
            _ => spec.url.as_deref().unwrap_or("-").to_string(),
        }
    }

    fn transport_name(t: &McpTransport) -> &'static str {
        match t {
            McpTransport::Stdio => "stdio",
            McpTransport::Sse => "sse",
            McpTransport::Http => "http",
        }
    }

    /// Approval state for the list view: a stored record exists. Whether it
    /// still matches the live version + content hash is verified at connect
    /// time - that check is authoritative, this column is a hint.
    fn approved_names(dd: &Path) -> Vec<String> {
        ApprovalStore::open(&dd.join("mcp")).names()
    }

    pub fn cmd_mcp(args: &[String]) {
        let verb = args.first().map(|s| s.as_str()).unwrap_or("list");
        // Help is not an unknown verb: `mcp --help` used to exit 2 with
        // an "unknown verb" error.
        if matches!(verb, "help" | "--help" | "-h") {
            println!("usage: pantheon mcp <list|status|health|approve|enable|disable|resources|templates|read|prompts|prompt|login|logout|auth> [--json]");
            return;
        }
        match verb {
            "list" | "" => list(args),
            "status" => status(args),
            "health" => health(args),
            "approve" => approve(args),
            "enable" => set_decl_enabled(args, true),
            "disable" => set_decl_enabled(args, false),
            "resources" => resources(args),
            "templates" => templates(args),
            "read" => read_resource(args),
            "prompts" => prompts(args),
            "prompt" => prompt(args),
            "login" => oauth_login(args),
            "logout" => oauth_logout(args),
            "auth" => oauth_auth(args),
            other => {
                eprintln!("mcp: unknown verb '{other}'");
                eprintln!(
                    "  usage: pantheon mcp <list|status|health|approve|enable|disable|resources|templates|read|prompts|prompt|login|logout|auth> [--json]"
                );
                std::process::exit(2);
            }
        }
    }

    fn oauth_login(args: &[String]) {
        let name = need_name(args, "login");
        let m = manager_for(&data_dir());
        match m.login_oauth(&name) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("mcp login: {e}");
                std::process::exit(1);
            }
        }
    }

    fn oauth_logout(args: &[String]) {
        let name = need_name(args, "logout");
        let m = manager_for(&data_dir());
        match m.logout_oauth(&name) {
            Ok(()) => println!("mcp: forgot OAuth credentials for '{name}'"),
            Err(e) => {
                eprintln!("mcp logout: {e}");
                std::process::exit(1);
            }
        }
    }

    fn oauth_auth(args: &[String]) {
        let name = need_name(args, "auth");
        let dd = data_dir();
        let cfg = config::Config::load_or_report(&dd);
        let declarations = read_mcp_declarations(&dd);
        let mcfg =
            config::resolve_mcp_section(cfg.as_ref().and_then(|c| c.mcp.as_ref()), &declarations);
        let Some(spec) = mcfg.servers.iter().find(|s| s.name == name) else {
            eprintln!("mcp auth: no server named '{name}'");
            std::process::exit(2);
        };
        let m = manager_for(&dd);
        match m.oauth_status(&name) {
            Ok((connected, scopes, expires_at)) => {
                if has_json(args) {
                    println!(
                        "{}",
                        serde_json::json!({
                            "server": name,
                            "auth": if spec.auth == pantheon_mcp::McpAuth::OAuth { "oauth" } else { "none" },
                            "connected": connected,
                            "scopes": scopes,
                            "expires_at": expires_at,
                        })
                    );
                } else if spec.auth != pantheon_mcp::McpAuth::OAuth {
                    println!("{name}: auth = none");
                } else if connected {
                    let expiry = expires_at
                        .map(|s| format!(" (expires {s})"))
                        .unwrap_or_default();
                    println!(
                        "{name}: oauth connected{expiry}{}",
                        if scopes.is_empty() {
                            String::new()
                        } else {
                            format!(" scopes={}", scopes.join(","))
                        }
                    );
                } else {
                    println!("{name}: oauth not connected (run `pantheon mcp login {name}`)");
                }
            }
            Err(e) => {
                eprintln!("mcp auth: {e}");
                std::process::exit(1);
            }
        }
    }

    fn need_name(args: &[String], verb: &str) -> String {
        match args.get(1) {
            Some(n) if !n.starts_with('-') => n.clone(),
            _ => {
                eprintln!("mcp {verb}: needs a server name");
                std::process::exit(2);
            }
        }
    }

    fn has_json(args: &[String]) -> bool {
        args.iter().any(|a| a == "--json")
    }

    fn list(args: &[String]) {
        let dd = data_dir();
        let rows = collect(&dd);
        let approved = approved_names(&dd);
        if has_json(args) {
            let v: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r.name,
                        "origin": r.origin,
                        "transport": transport_name(&r.spec.transport),
                        "target": target_of(&r.spec),
                        "env": r.spec.env.keys().cloned().collect::<Vec<_>>(),
                        "approved": approved.iter().any(|a| a == &r.name),
                        "blocker": r.decl_blocker.clone(),
                    })
                })
                .collect();
            match serde_json::to_string_pretty(&v) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("mcp: encode failed: {e}"),
            }
            return;
        }
        if rows.is_empty() {
            println!(
                "no MCP servers declared - add `[mcp.servers.<name>]` to {} or run `pantheon migrate apply`",
                dd.join("config.toml").display()
            );
            return;
        }
        println!(
            "{:<26} {:<11} {:<7} {:<8} TARGET",
            "NAME", "ORIGIN", "TRANS", "APPROVED"
        );
        for r in &rows {
            let appr = if approved.iter().any(|a| a == &r.name) {
                "yes"
            } else {
                "no"
            };
            let mut line = format!(
                "{:<26} {:<11} {:<7} {:<8} {}",
                r.name,
                r.origin,
                transport_name(&r.spec.transport),
                appr,
                target_of(&r.spec)
            );
            if let Some(b) = &r.decl_blocker {
                line.push_str(&format!("  ({b})"));
            }
            println!("{line}");
        }
        println!();
        println!(
            "{} server(s); unapproved servers never launch - `pantheon mcp approve <name>`",
            rows.len()
        );
    }

    /// `status`: the live snapshot from `<data_dir>/mcp/live.json`, written
    /// by the manager whenever a session registers tools or runs a health
    /// check. No session running = the last known state.
    fn status(args: &[String]) {
        let dd = data_dir();
        let live = read_live(&dd);
        let servers = live
            .as_ref()
            .and_then(|v| v.get("servers"))
            .and_then(|v| v.as_array());
        if has_json(args) {
            match serde_json::to_string_pretty(&live.unwrap_or(serde_json::json!({}))) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("mcp: encode failed: {e}"),
            }
            return;
        }
        let Some(servers) = servers else {
            println!("no live MCP state yet - start a session to launch servers");
            return;
        };
        if servers.is_empty() {
            println!("no MCP servers configured");
            return;
        }
        println!(
            "{:<26} {:<15} {:>5} {:>8} {:>8}",
            "NAME", "STATUS", "TOOLS", "CONNECTS", "FAILS"
        );
        let mut rows: Vec<&serde_json::Value> = servers.iter().collect();
        rows.sort_by(|a, b| {
            a.get("name")
                .and_then(|v| v.as_str())
                .cmp(&b.get("name").and_then(|v| v.as_str()))
        });
        for s in rows {
            let n = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let st = s.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let tools = s.get("tools").and_then(|v| v.as_u64()).unwrap_or(0);
            let connects = s.get("connects").and_then(|v| v.as_u64()).unwrap_or(0);
            let fails = s.get("failures").and_then(|v| v.as_u64()).unwrap_or(0);
            let mut line = format!("{n:<26} {st:<15} {tools:>5} {connects:>8} {fails:>8}");
            if let Some(err) = s.get("last_error").and_then(|v| v.as_str()) {
                line.push_str(&format!("  ({err})"));
            }
            if st == "unapproved" {
                line.push_str("  [needs `mcp approve`]");
            }
            println!("{line}");
        }
    }

    /// Synthesize the CUA driver's MCP spec into the manager when it is not
    /// already declared. Mirrors what the runtime does at tool-registration
    /// time: the driver answers to the ComputerUse group and `[computer_use]`,
    /// not to `[mcp.servers]`.
    fn ensure_driver_spec(m: &McpManager, dd: &Path) {
        let cfg = config::Config::load_or_report(dd);
        let section = cfg.as_ref().and_then(|c| c.computer_use.clone());
        let (driver, binary) = match section.as_ref() {
            Some(s) => (s.driver.as_deref(), s.binary.as_deref()),
            None => (None, None),
        };
        if let Some(spec) = pantheon_runtime::computer::computer_driver_spec(driver, binary) {
            let server_name = spec.name.clone();
            if m.spec(&server_name).is_some() {
                return;
            }
            let mut specs: Vec<_> = m
                .spec_names()
                .into_iter()
                .filter_map(|n| m.spec(&n))
                .collect();
            specs.push(spec);
            m.configure(specs);
        }
    }

    /// `health <name>`: full detail for one server from the live snapshot.
    fn health(args: &[String]) {
        let name = need_name(args, "health");
        let dd = data_dir();
        let live = read_live(&dd);
        let entry = live.as_ref().and_then(|v| v.get("servers")).and_then(|v| {
            v.as_array()?
                .iter()
                .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
        });
        if has_json(args) {
            match serde_json::to_string_pretty(&entry.unwrap_or(&serde_json::Value::Null)) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("mcp: encode failed: {e}"),
            }
            return;
        }
        let Some(e) = entry else {
            eprintln!("mcp: no live state for '{name}' - is it configured? (`mcp list`)");
            std::process::exit(1);
        };
        let get = |k: &str| e.get(k).map(|v| v.to_string()).unwrap_or_default();
        println!("server:  {name}");
        println!("status:  {}", get("status"));
        println!("target:  {}", get("target"));
        println!("tools:   {}", get("tools"));
        println!("connects: {}", get("connects"));
        println!("failures: {}", get("failures"));
        if !get("last_error").is_empty() && get("last_error") != "null" {
            println!("error:   {}", get("last_error"));
        }
    }

    fn manager_live(dd: &Path) -> McpManager {
        let m = manager_for(dd);
        ensure_driver_spec(&m, dd);
        m
    }

    /// `resources <server> [--json]`: list a server's static resources.
    fn resources(args: &[String]) {
        let server = need_name(args, "resources");
        let dd = data_dir();
        let m = manager_live(&dd);
        match m.list_resources(&server) {
            Ok(items) => {
                if has_json(args) {
                    match serde_json::to_string_pretty(&items) {
                        Ok(s) => println!("{s}"),
                        Err(e) => eprintln!("mcp: encode failed: {e}"),
                    }
                    return;
                }
                if items.is_empty() {
                    println!("no resources advertised by '{server}'");
                    return;
                }
                for r in items {
                    let mime = r.mime_type.as_deref().unwrap_or("-");
                    let title = r.title.as_deref().unwrap_or(&r.name);
                    println!("{:<40} {:<24} {}", r.uri, mime, title);
                    if let Some(d) = r.description {
                        println!("  {d}");
                    }
                }
            }
            Err(e) => {
                eprintln!("mcp: resources from '{server}' failed: {e}");
                std::process::exit(1);
            }
        }
    }

    /// `templates <server> [--json]`: list parameterized resource templates.
    fn templates(args: &[String]) {
        let server = need_name(args, "templates");
        let dd = data_dir();
        let m = manager_live(&dd);
        match m.list_resource_templates(&server) {
            Ok(items) => {
                if has_json(args) {
                    match serde_json::to_string_pretty(&items) {
                        Ok(s) => println!("{s}"),
                        Err(e) => eprintln!("mcp: encode failed: {e}"),
                    }
                    return;
                }
                if items.is_empty() {
                    println!("no resource templates advertised by '{server}'");
                    return;
                }
                for t in items {
                    let mime = t.mime_type.as_deref().unwrap_or("-");
                    println!("{:<48} {:<24} {}", t.uri_template, mime, t.name);
                    if let Some(d) = t.description {
                        println!("  {d}");
                    }
                }
            }
            Err(e) => {
                eprintln!("mcp: templates from '{server}' failed: {e}");
                std::process::exit(1);
            }
        }
    }

    /// `read <server> <uri> [--json]`: fetch one resource.
    fn read_resource(args: &[String]) {
        let server = need_name(args, "read");
        let Some(uri) = args.get(2) else {
            eprintln!("mcp read: needs <server> <uri>");
            std::process::exit(2);
        };
        let dd = data_dir();
        let m = manager_live(&dd);
        match m.read_resource(&server, uri) {
            Ok(contents) => {
                if has_json(args) {
                    match serde_json::to_string_pretty(&contents) {
                        Ok(s) => println!("{s}"),
                        Err(e) => eprintln!("mcp: encode failed: {e}"),
                    }
                    return;
                }
                for c in contents {
                    if let Some(text) = c.text {
                        println!("{text}");
                    } else if let Some(blob) = c.blob {
                        println!(
                            "[{}] {} bytes of base64 data",
                            c.mime_type.as_deref().unwrap_or("application/octet-stream"),
                            blob.len()
                        );
                    } else {
                        println!("[{}] (empty resource)", c.uri);
                    }
                }
            }
            Err(e) => {
                eprintln!("mcp: read '{uri}' from '{server}' failed: {e}");
                std::process::exit(1);
            }
        }
    }

    /// `prompts <server> [--json]`: list prompt templates.
    fn prompts(args: &[String]) {
        let server = need_name(args, "prompts");
        let dd = data_dir();
        let m = manager_live(&dd);
        match m.list_prompts(&server) {
            Ok(items) => {
                if has_json(args) {
                    match serde_json::to_string_pretty(&items) {
                        Ok(s) => println!("{s}"),
                        Err(e) => eprintln!("mcp: encode failed: {e}"),
                    }
                    return;
                }
                if items.is_empty() {
                    println!("no prompts advertised by '{server}'");
                    return;
                }
                for p in items {
                    let required: Vec<String> = p
                        .arguments
                        .iter()
                        .filter(|a| a.required)
                        .map(|a| a.name.clone())
                        .collect();
                    let suffix = if required.is_empty() {
                        String::new()
                    } else {
                        format!(" (required: {})", required.join(", "))
                    };
                    println!("{}{}", p.name, suffix);
                    if let Some(d) = p.description {
                        println!("  {d}");
                    }
                }
            }
            Err(e) => {
                eprintln!("mcp: prompts from '{server}' failed: {e}");
                std::process::exit(1);
            }
        }
    }

    /// `prompt <server> <name> [--arg key=value]... [--json]`: expand a prompt.
    fn prompt(args: &[String]) {
        let server = need_name(args, "prompt");
        let Some(name) = args.get(2) else {
            eprintln!("mcp prompt: needs <server> <name> [--arg key=value]...");
            std::process::exit(2);
        };
        let mut arguments = serde_json::Map::new();
        let mut i = 3;
        while i < args.len() {
            if args[i] == "--arg" {
                let Some(kv) = args.get(i + 1) else {
                    eprintln!("mcp prompt: --arg needs key=value");
                    std::process::exit(2);
                };
                let Some((k, v)) = kv.split_once('=') else {
                    eprintln!("mcp prompt: --arg needs key=value, got '{kv}'");
                    std::process::exit(2);
                };
                arguments.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                i += 2;
            } else {
                i += 1;
            }
        }
        let dd = data_dir();
        let m = manager_live(&dd);
        match m.get_prompt(&server, name, &serde_json::Value::Object(arguments)) {
            Ok(result) => {
                if has_json(args) {
                    match serde_json::to_string_pretty(&result) {
                        Ok(s) => println!("{s}"),
                        Err(e) => eprintln!("mcp: encode failed: {e}"),
                    }
                    return;
                }
                if let Some(d) = result.description {
                    println!("{d}\n");
                }
                for msg in result.messages {
                    let text = msg
                        .content
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| msg.content.to_string());
                    println!("[{}] {text}", msg.role);
                }
            }
            Err(e) => {
                eprintln!("mcp: prompt '{name}' from '{server}' failed: {e}");
                std::process::exit(1);
            }
        }
    }

    /// `approve <name> [--yes]`: the explicit approval flow. Prints exactly
    /// what the server is, shows the warning, and requires confirmation
    /// interactive (type the server name) unless `--yes` is passed.
    fn approve(args: &[String]) {
        let name = need_name(args, "approve");
        let yes = args.iter().any(|a| a == "--yes" || a == "-y");
        let dd = data_dir();
        let m = manager_for(&dd);
        // The CUA driver is not declared in `[mcp.servers]` - the runtime
        // synthesizes its spec from `[computer_use]` when the ComputerUse
        // group is on. Approval must see the same spec, or the driver could
        // never be approved.
        ensure_driver_spec(&m, &dd);
        let spec = match m.spec(&name) {
            Some(s) => s,
            None => {
                eprintln!(
                    "mcp: no server named '{name}' - `pantheon mcp list` shows the declared ones"
                );
                std::process::exit(1);
            }
        };
        println!("You are about to approve the MCP server '{name}'.");
        println!();
        match spec.transport {
            McpTransport::Stdio => println!(
                "  launch: {} {}",
                spec.command.as_deref().unwrap_or("?"),
                spec.args.join(" ")
            ),
            McpTransport::Sse => {
                println!("  connect (sse): {}", spec.url.as_deref().unwrap_or("?"))
            }
            McpTransport::Http => {
                println!("  connect (http): {}", spec.url.as_deref().unwrap_or("?"))
            }
        }
        if spec.env.is_empty() {
            println!("  env:    (none declared)");
        } else {
            let mut names: Vec<&String> = spec.env.keys().collect();
            names.sort();
            println!(
                "  env:    {}",
                names
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        println!();
        println!("Approving lets Pantheon launch this third-party server and hand it");
        println!("your agent's tool calls. Only approve servers you trust: a malicious");
        println!("server sees everything the agent can see. Secrets listed above resolve");
        println!("from your vault at launch time and are never printed.");
        println!();
        // REA-specific disclosure: its tracing tools execute the analyzed
        // binary. This is not implied by the general MCP warning above, and
        // approval must not be mistaken for containment.
        if name == pantheon_mcp::rea::REA_SERVER_NAME {
            println!("SECURITY NOTICE — REA reverse-engineering tools:");
            println!("  - Tracing tools (trace_*, capture_*, observe_*) EXECUTE the");
            println!("    analyzed binary on this machine to observe its behavior.");
            println!("  - MCP server processes currently run UNSANDBOXED with your");
            println!("    user privileges. Approval does NOT provide process");
            println!("    isolation or containment.");
            println!("  - Each tracing call parks for your explicit approval before");
            println!("    it runs, but approval of the call is not a sandbox.");
            println!("  Only analyze software you trust, from sources you trust.");
            println!();
        }
        println!("The approval binds the server's name, its reported version, and a");
        println!("content hash of what would run. Any upgrade or change invalidates it");
        println!("and you will be asked again.");
        println!();
        if !yes {
            use std::io::Write as _;
            print!("Type the server name to approve it: ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() || line.trim() != name {
                println!("mcp: approval aborted");
                std::process::exit(1);
            }
        }
        match m.approve_server(&name) {
            Ok(rec) => {
                let hash12: String = rec.dir_hash.chars().take(12).collect();
                println!(
                    "approved '{name}' (version {}, hash {hash12}...)",
                    rec.version
                );
            }
            Err(e) => {
                eprintln!("mcp: approve failed: {e}");
                std::process::exit(1);
            }
        }
    }

    /// `enable` / `disable <name>`: toggle a *declaration* server. Servers
    /// defined in config.toml are edited there, not rewritten by this verb.
    fn set_decl_enabled(args: &[String], enabled: bool) {
        let verb = if enabled { "enable" } else { "disable" };
        let name = need_name(args, verb);
        let dd = data_dir();
        let declarations = read_mcp_declarations(&dd);
        let source = declarations
            .iter()
            .find(|d| d.servers.iter().any(|s| s.name == name))
            .map(|d| d.source.clone());
        let Some(source) = source else {
            eprintln!("mcp: no declared server named '{name}'");
            eprintln!("  (config-defined servers are toggled with `[mcp.servers.{name}] enabled` in config.toml)");
            std::process::exit(1);
        };
        let path = dd.join("mcp").join(format!("{source}.json"));
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("mcp: cannot parse {}: {e}", path.display());
                std::process::exit(1);
            }
        };
        let mut touched = false;
        if let Some(servers) = v.get_mut("servers").and_then(|s| s.as_array_mut()) {
            for s in servers {
                if s.get("name").and_then(|n| n.as_str()) == Some(name.as_str()) {
                    s["enabled"] = serde_json::Value::Bool(enabled);
                    touched = true;
                }
            }
        }
        if !touched {
            eprintln!("mcp: server '{name}' not found in {}", path.display());
            std::process::exit(1);
        }
        // Atomic rewrite: temp file + rename.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default()).is_err()
            || std::fs::rename(&tmp, &path).is_err()
        {
            eprintln!("mcp: failed to rewrite {}", path.display());
            std::process::exit(1);
        }
        println!("mcp: {verb}d '{name}'");
    }

    fn read_live(dd: &Path) -> Option<serde_json::Value> {
        std::fs::read_to_string(dd.join("mcp").join("live.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
    }

    /// Re-scan declarations + config, push the resolved specs into the
    /// session's manager, and report readiness. Called by `/mcp reload` in
    /// the TUI: the manager drops live clients for changed specs and
    /// reconnects lazily on the next registry build, so a reload can never
    /// drop the session - the worst case is an unchanged report.
    pub fn reload_report(
        session: &pantheon_runtime::session::Session,
        dd: &std::path::Path,
    ) -> Vec<String> {
        let cfg = config::Config::load_or_report(dd);
        let declarations = read_mcp_declarations(dd);
        let mcfg =
            config::resolve_mcp_section(cfg.as_ref().and_then(|c| c.mcp.as_ref()), &declarations);
        session.set_mcp_config(mcfg);
        let mut lines = vec!["mcp: re-scanned config + declarations".to_string()];
        let health = session.mcp_manager.health();
        let mut names: Vec<&pantheon_mcp::manager::ServerHealth> = health.iter().collect();
        names.sort_by(|a, b| a.name.cmp(&b.name));
        let mut pending = 0usize;
        for h in names {
            let status = match h.status {
                ServerStatus::Ready => "ready",
                ServerStatus::Connecting => "connecting",
                ServerStatus::Failed => "failed",
                ServerStatus::Backoff => "backoff",
                ServerStatus::Unapproved => {
                    pending += 1;
                    "needs approval"
                }
                ServerStatus::Disabled => "disabled",
            };
            let mut line = format!("  {} [{status}]", h.name);
            if h.tools > 0 {
                line.push_str(&format!(" ({} tools)", h.tools));
            }
            if let Some(e) = &h.last_error {
                line.push_str(&format!(": {e}"));
            }
            lines.push(line);
        }
        if health.is_empty() {
            lines.push("  no MCP servers configured".to_string());
        }
        if pending > 0 {
            lines.push(format!(
                "{pending} server(s) need `pantheon mcp approve <name>` before they launch"
            ));
        }
        lines
    }
}

pub mod notify {
    //! Turn-complete notifications for background session tabs.
    //!
    //! When a turn finishes on a tab that is not the visible one, the operator
    //! is probably looking elsewhere. This module raises two best-effort
    //! signals: a terminal bell and a desktop notification via the shared
    //! cross-platform notifier (`pantheon_gateway::notify`: notify-send on
    //! Linux, osascript on macOS, PowerShell toast on Windows). Both are
    //! fire-and-forget: a missing helper or a failed spawn never affects the
    //! turn. The visible tab never notifies - the transcript itself is the
    //! signal there.

    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Pure trigger logic: notify only when the completed run is not the one
    /// on screen. An empty run id never notifies (defensive; callers always
    /// set it).
    pub fn should_notify(completed_run_id: &str, visible_session_id: &str) -> bool {
        !completed_run_id.is_empty() && completed_run_id != visible_session_id
    }

    /// Executable-file check shared with the setup wizard's command lookup.
    #[cfg(unix)]
    pub(crate) fn is_executable_file(p: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        p.is_file()
            && p.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }

    #[cfg(not(unix))]
    pub(crate) fn is_executable_file(p: &Path) -> bool {
        p.is_file()
    }

    /// Find `notify-send` on an explicit search path. Split out from PATH
    /// lookup so tests can pass a temp dir without touching the environment.
    /// (Linux-only helper kept for the eval tests; the live path uses the
    /// cross-platform [`pantheon_gateway::notify`] instead.)
    pub fn find_notify_send(search_dirs: &[PathBuf]) -> Option<PathBuf> {
        search_dirs
            .iter()
            .map(|d| d.join("notify-send"))
            .find(|p| is_executable_file(p))
    }

    /// PATH directories of this process.
    fn path_dirs() -> Vec<PathBuf> {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default()
    }

    /// Build the desktop-notification command for an explicit binary path.
    /// Split from discovery so tests assert argv without touching PATH.
    pub fn notify_command_with(bin: &Path, title: &str, body: &str) -> Command {
        let mut cmd = Command::new(bin);
        cmd.arg("--app-name")
            .arg("Pantheon")
            .arg("--expire-time")
            .arg("8000")
            .arg(title)
            .arg(body);
        cmd
    }

    /// Build the desktop-notification command, or `None` when `notify-send`
    /// is unavailable. Kept separate from spawning so tests can assert on the
    /// exact argv without executing anything.
    pub fn notify_command(title: &str, body: &str) -> Option<Command> {
        let bin = find_notify_send(&path_dirs())?;
        Some(notify_command_with(&bin, title, body))
    }

    /// Short human summary for the notification body: first line of the last
    /// assistant message, capped; falls back to a generic note.
    pub fn summarize_turn(last_assistant_text: Option<&str>, turns_completed: u32) -> String {
        match last_assistant_text.map(str::trim).filter(|s| !s.is_empty()) {
            Some(t) => {
                let first: String = t.lines().next().unwrap_or("").chars().take(120).collect();
                format!("turn {turns_completed} done - {first}")
            }
            None => format!("turn {turns_completed} done"),
        }
    }

    /// Best-effort notification: terminal bell, then desktop. Never panics,
    /// never blocks the caller meaningfully.
    pub fn emit_turn_notification(tab_label: &str, summary: &str) {
        // Terminal bell: the TUI owns stdout, so BEL rings the terminal.
        {
            use std::io::Write as _;
            let _ = write!(std::io::stdout(), "\x07");
            let _ = std::io::stdout().flush();
        }
        let title = format!("Pantheon - {tab_label}");
        // Cross-platform desktop notification; a failure is silent by design.
        let _ = pantheon_gateway::notify::desktop_notify(&title, summary);
    }
}

pub mod fallback {
    //! `pantheon fallback <add|list|remove>` - the ordered provider/model
    //! fallback chain.
    //!
    //! The chain already exists in config as `[model].fallbacks` and is honoured
    //! by the provider chain at runtime; it just had no surface. Without one, the
    //! only way to change it was hand-editing config.toml, which is exactly the
    //! kind of thing that drifts and then silently does nothing.
    //!
    //! Order is the contract: the runtime walks the list in order and only moves
    //! to the next entry when the previous one fails. So `add` appends and
    //! `remove` takes an index, not a name - two entries can legitimately name
    //! the same provider with different models.

    use crate::config::{Config, FallbackEntry};
    use std::path::Path;

    fn usage() -> &'static str {
        "usage:\n  \
         pantheon fallback list\n  \
         pantheon fallback add <provider> <model>\n  \
         pantheon fallback remove <index>\n  \
         pantheon fallback insert <index> <provider> <model>"
    }

    pub fn cmd_fallback(args: &[String]) {
        let sub = match args.get(2).map(String::as_str) {
            Some(s) => s,
            None => {
                eprintln!("{}", usage());
                std::process::exit(2);
            }
        };
        let dd = crate::terminal::data_dir();
        // One load, one save: read-modify-write so a concurrent edit cannot be
        // half-applied, and so validation runs against what is actually on disk.
        let mut cfg = match Config::load(&dd) {
            Ok(c) => c,
            Err(e) if e.code == "CONFIG_OPEN" => {
                eprintln!("no config at {} - run `pantheon setup` first", dd.display());
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("read config: {e}");
                std::process::exit(1);
            }
        };

        // A fallback chain is meaningless without a default to fall back
        // from, and creating `[model]` here with empty provider/model used
        // to poison resolution: the empty config values beat the environment
        // and the local default in `build_model_policy`. So nothing here
        // ever creates a `[model]` section: `list` reads the existing chain
        // (or nothing), `remove` without a section is an out-of-range
        // error, and `add`/`insert` refuse until a real default model
        // exists. Named `section` not `model`: the `add`/`insert` arms bind
        // a `model` string parameter, which would otherwise shadow this.
        // The write arms take their `&mut` borrow inside the arm, after
        // argument validation, so a bad invocation never mutates `cfg`.
        match sub {
            "list" => {
                let chain = cfg
                    .model
                    .as_ref()
                    .map(|m| m.fallbacks.clone())
                    .unwrap_or_default();
                if chain.is_empty() {
                    println!("no fallbacks configured");
                    println!("add one: pantheon fallback add <provider> <model>");
                    return;
                }
                println!("fallback chain (tried in order after the default fails):");
                for (i, f) in chain.iter().enumerate() {
                    println!("  {i}  {} / {}", f.provider, f.model);
                }
            }
            "add" => {
                let (provider, model) = match (args.get(3), args.get(4)) {
                    (Some(p), Some(m)) => (p.clone(), m.clone()),
                    _ => {
                        eprintln!("{}", usage());
                        std::process::exit(2);
                    }
                };
                if provider.trim().is_empty() || model.trim().is_empty() {
                    eprintln!("provider and model must both be non-empty");
                    std::process::exit(2);
                }
                let Some(section) = cfg.model.as_mut() else {
                    eprintln!(
                        "no [model] section yet; set a default model first (e.g. `pantheon model`), then add fallbacks"
                    );
                    std::process::exit(2);
                };
                warn_about(&dd, section, &provider, &model);
                section.fallbacks.push(FallbackEntry {
                    provider,
                    model,
                    key_env: None,
                });
                let n = section.fallbacks.len();
                save(&dd, &cfg);
                println!("fallback {n} added");
            }
            "insert" => {
                let (idx, provider, model) = match (args.get(3), args.get(4), args.get(5)) {
                    (Some(i), Some(p), Some(m)) => (i.clone(), p.clone(), m.clone()),
                    _ => {
                        eprintln!("{}", usage());
                        std::process::exit(2);
                    }
                };
                let at: usize = match idx.parse() {
                    Ok(v) => v,
                    Err(_) => {
                        eprintln!("'{idx}' is not an index");
                        std::process::exit(2);
                    }
                };
                let Some(section) = cfg.model.as_mut() else {
                    eprintln!(
                        "no [model] section yet; set a default model first (e.g. `pantheon model`), then add fallbacks"
                    );
                    std::process::exit(2);
                };
                if at > section.fallbacks.len() {
                    eprintln!(
                        "index {at} is past the end (chain has {} entries)",
                        section.fallbacks.len()
                    );
                    std::process::exit(2);
                }
                warn_about(&dd, section, &provider, &model);
                section.fallbacks.insert(
                    at,
                    FallbackEntry {
                        provider,
                        model,
                        key_env: None,
                    },
                );
                let len = section.fallbacks.len();
                save(&dd, &cfg);
                let _ = len;
                println!("fallback inserted at {at}");
            }
            "remove" => {
                let idx = match args.get(3) {
                    Some(i) => i.clone(),
                    None => {
                        eprintln!("{}", usage());
                        std::process::exit(2);
                    }
                };
                let at: usize = match idx.parse() {
                    Ok(v) => v,
                    Err(_) => {
                        eprintln!("'{idx}' is not an index; run `pantheon fallback list`");
                        std::process::exit(2);
                    }
                };
                let Some(section) = cfg.model.as_mut() else {
                    eprintln!("index {at} is out of range (no fallbacks configured)");
                    std::process::exit(2);
                };
                if at >= section.fallbacks.len() {
                    eprintln!(
                        "index {at} is out of range (chain has {} entries)",
                        section.fallbacks.len()
                    );
                    std::process::exit(2);
                }
                let gone = section.fallbacks.remove(at);
                let label = format!("{} / {}", gone.provider, gone.model);
                save(&dd, &cfg);
                println!("removed {label}");
            }
            other => {
                eprintln!("unknown subcommand '{other}'");
                eprintln!("{}", usage());
                std::process::exit(2);
            }
        }
    }

    /// Warnings for a fallback that would burn retries at runtime:
    /// unknown provider, a duplicate of an earlier entry, an entry
    /// identical to the default (a fallback to itself), or a provider
    /// whose key env var is unset in both the process env and
    /// `<data_dir>/.env`. Warnings, not refusals: the chain walk still
    /// decides at runtime, but adding a dead entry silently used to cost
    /// 3 retries per turn before anyone noticed.
    fn warn_about(dd: &Path, section: &crate::config::ModelSection, provider: &str, model: &str) {
        let known = pantheon_providers::catalog::provider(provider).is_some()
            || provider.starts_with("http://")
            || provider.starts_with("https://");
        if !known {
            eprintln!("warning: '{provider}' is not a catalog provider; this fallback will fail as PROVIDER_CONFIG");
        }
        if section.provider == provider && section.model == model {
            eprintln!("warning: fallback {provider}/{model} is identical to the default model; it adds retries, not resilience");
        }
        if section
            .fallbacks
            .iter()
            .any(|f| f.provider == provider && f.model == model)
        {
            eprintln!("warning: fallback {provider}/{model} duplicates an existing entry");
        }
        if known {
            let key_env = pantheon_providers::catalog::provider(provider)
                .map(|p| {
                    if p.key_env.trim().is_empty() {
                        crate::config::provider_key_env(provider, None)
                    } else {
                        p.key_env.clone()
                    }
                })
                .unwrap_or_else(|| crate::config::provider_key_env(provider, None));
            let in_env = std::env::var(&key_env)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_some();
            let in_dotenv = std::fs::read_to_string(dd.join(".env"))
                .map(|text| {
                    text.lines().any(|line| match line.trim().split_once('=') {
                        Some((name, value)) => {
                            name.trim() == key_env && !value.trim().trim_matches('"').is_empty()
                        }
                        None => false,
                    })
                })
                .unwrap_or(false);
            if !in_env && !in_dotenv {
                eprintln!(
                    "warning: {key_env} is not set; this fallback will 401 before it can help"
                );
            }
        }
    }

    fn save(dd: &Path, cfg: &Config) {
        if let Err(e) = cfg.save(dd) {
            eprintln!("write config: {e}");
            std::process::exit(1);
        }
    }
}

pub mod init {
    //! `pantheon init`: idempotent, non-interactive bootstrap of the always-on
    //! gateway service (chat surfaces + scheduler loop).
    //!
    //! Detects the platform's service manager and installs accordingly
    //! systemd user unit, cron `@reboot` fallback, launchd agent, or Windows
    //! Task Scheduler - sharing the same jobs and durable claim ledger as
    //! `pantheon schedule tick`. Re-running converges: it updates the install
    //! and ensures the service is enabled and running, never duplicating it.
    //! With no supported manager it fails open, printing the manual cron line.
    //! Never prompts; always safe to call.

    use pantheon_gateway::{install_service, self_exe, InstallOutcome};

    fn print_usage() {
        eprintln!(
            "usage: pantheon init\n\n\
             Install the Pantheon gateway as an always-on user service\n\
             (chat surfaces + scheduled tasks), idempotently:\n\
            - Linux + systemd: user unit + `systemctl --user enable --now`\n\
            - Linux w/o systemd: cron `@reboot` entry\n\
            - macOS: LaunchAgent + `launchctl bootstrap`\n\
            - Windows: Task Scheduler logon task\n\
            - otherwise: prints the manual cron line instead of failing\n\n\
             Re-running updates the install and ensures it is running.\n\
             See also: pantheon gateway status|restart"
        );
    }

    /// Entry point for `pantheon init`. `args` is the full argv.
    pub fn cmd_init(args: &[String]) {
        if args.iter().any(|a| a == "--help" || a == "-h") {
            print_usage();
            return;
        }
        if let Some(extra) = args.get(2) {
            eprintln!("init: unexpected argument '{extra}'");
            print_usage();
            std::process::exit(2);
        }
        let data_dir = crate::terminal::data_dir();
        let exe = match self_exe() {
            Some(e) => e,
            None => {
                eprintln!("init: cannot resolve the pantheon binary path");
                std::process::exit(1);
            }
        };
        match install_service(&data_dir, &exe) {
            InstallOutcome::Installed { mechanism, changed } => {
                if changed {
                    println!(
                        "init: gateway service installed via {}.",
                        mechanism.as_str()
                    );
                } else {
                    println!(
                        "init: gateway service already installed via {}; ensured running.",
                        mechanism.as_str()
                    );
                }
                println!("      status: pantheon gateway status");
                println!("      restart: pantheon gateway restart");
            }
            InstallOutcome::Unavailable { note } => {
                // Fail open: no manager is not an error, just a manual setup.
                println!("init: {note}");
            }
            InstallOutcome::Failed { mechanism, error } => {
                eprintln!("init: install via {} failed: {error}", mechanism.as_str());
                std::process::exit(1);
            }
        }
    }
}

pub mod pipeline {
    //! `pantheon pipeline`: run the orchestration pipeline against the runtime.
    //!
    //! The StageExecutor drives one session per stage; the
    //! evaluator is a second session asked to critique. Human gates park the
    //! pipeline; `pantheon pipeline <run_id> --approve <stage>` / `--deny
    //! <stage>` resolve them and resume.

    use std::path::Path;

    use crate::config::Config;
    use crate::config_schema::PolicyPreset;
    use pantheon_runtime::pipeline::{AcceptAllEvaluator, StageEvaluator, StageExecutor};
    use pantheon_runtime::pipeline_runner::PipelineRunner;
    use pantheon_runtime::Supervisor;
    use pantheon_storage::OperationStatus;
    use std::path::PathBuf;

    fn policy_for(cfg: Option<&Config>) -> pantheon_api::capability::Policy {
        match cfg.and_then(|c| c.policy) {
            Some(PolicyPreset::Reader) => pantheon_api::capability::Policy::researcher_readonly(),
            Some(PolicyPreset::CoderMemory) => {
                pantheon_api::capability::Policy::coder_with_memory()
            }
            _ => pantheon_api::capability::Policy::coder(),
        }
    }

    /// Real executor: one Session per stage call, prompt framed per stage.
    struct RuntimeExecutor {
        data_dir: PathBuf,
        policy: pantheon_api::capability::Policy,
    }

    impl StageExecutor for RuntimeExecutor {
        fn run_stage(
            &self,
            stage: &str,
            input: &str,
        ) -> Result<String, pantheon_api::error::PantheonError> {
            let session = open_session(&self.data_dir, self.policy.clone())?;
            let run_id = format!("pipe-{stage}-{}", pantheon_runtime::new_run_id());
            let prompt = match stage {
                "intake" => format!("Restate the following task as a precise, testable specification. Output only the specification.\n\n{input}"),
                "research" => format!("Gather the context and constraints needed for this specification. List facts, unknowns, and risks. Output only the list.\n\n{input}"),
                "plan" => format!("Produce a step-by-step implementation plan for this specification. Output only the plan.\n\n{input}"),
                "implement" => format!("Execute this plan. Produce the work product and a summary of what you did.\n\n{input}"),
                "review" => format!("Review this work against the plan. List defects and confirm or reject. Output the verdict and reasons.\n\n{input}"),
                "commit" => format!("Finalize: produce the commit message and summary for this work.\n\n{input}"),
                other => format!("{other}: {input}"),
            };
            match session.chat(&run_id, &prompt) {
                Ok(pantheon_agent::LoopOutcome::Answered { text, .. }) => Ok(text),
                Ok(other) => Ok(format!("[stage {stage} ended: {other:?}]")),
                Err(e) => Err(e),
            }
        }
    }

    /// Real evaluator: a separate session asked to accept or reject.
    struct RuntimeEvaluator {
        data_dir: PathBuf,
        policy: pantheon_api::capability::Policy,
    }

    impl StageEvaluator for RuntimeEvaluator {
        fn accept(
            &self,
            stage: &str,
            output: &str,
        ) -> Result<bool, pantheon_api::error::PantheonError> {
            let session = open_session(&self.data_dir, self.policy.clone())?;
            let run_id = format!("pipe-eval-{}", pantheon_runtime::new_run_id());
            let prompt = format!(
                "You are a strict evaluator. Does the following {stage} output meet its goal? \
    Reply with exactly ACCEPT or REJECT followed by one reason line.\n\n{output}"
            );
            let answer = match session.chat(&run_id, &prompt) {
                Ok(pantheon_agent::LoopOutcome::Answered { text: t, .. }) => t.to_ascii_lowercase(),
                _ => return Ok(false),
            };
            Ok(answer.starts_with("accept"))
        }
    }

    fn open_session(
        data_dir: &Path,
        policy: pantheon_api::capability::Policy,
    ) -> Result<pantheon_runtime::session::Session, pantheon_api::error::PantheonError> {
        let cfg = Config::load_or_report(data_dir);
        let (provider, model) = cfg
            .as_ref()
            .and_then(|c| c.model.clone())
            .map(|m| (m.provider, m.model))
            .unwrap_or_else(|| {
                (
                    std::env::var("PANTHEON_PROVIDER").unwrap_or_else(|_| "local".into()),
                    std::env::var("PANTHEON_MODEL").unwrap_or_else(|_| "llama3.2".into()),
                )
            });
        let model_policy = pantheon_api::model::ModelPolicy {
            reasoning_budget: Default::default(),
            reasoning: Default::default(),
            default: pantheon_api::model::DefaultModel {
                provider: provider.clone(),
                model: model.clone(),
                key_env: None,
            },
            fallbacks: pantheon_api::model::FallbackChain::default(),
            auxiliaries: crate::config::auxiliaries(
                cfg.as_ref(),
                &pantheon_api::model::DefaultModel {
                    provider,
                    model,
                    key_env: None,
                },
            ),
        };
        let secrets = crate::config::chat_secrets(cfg.as_ref());
        // `[memory]` wins over memory-backend.toml on load: sync BEFORE
        // the session reads the selection, or the sync lands a session late.
        crate::config::sync_memory_selection(data_dir, cfg.as_ref());
        pantheon_runtime::session::Session::new(
            data_dir.to_path_buf(),
            policy,
            model_policy,
            secrets,
        )
        .inspect(|s| crate::config::apply_session_config(s, cfg.as_ref()))
    }

    pub fn cmd_pipeline(args: &[String]) {
        let parsed = crate::args::Args::parse(&args[2.min(args.len())..]);
        if parsed.has("help") || parsed.has("h") {
            // Help goes to stdout like every other verb's help, not stderr:
            // the old stderr print exited 0, so `pipeline --help` looked
            // like a failed run to anything capturing streams separately.
            println!(
                "usage: pantheon pipeline [--spec \"task\"] | <run_id> --approve <stage> | <run_id> --deny <stage>"
            );
            return;
        }
        // First positional (if any) is the run id.
        let run_id = parsed
            .positional(0)
            .unwrap_or_else(|| format!("pipe-{}", pantheon_runtime::new_run_id()));
        let approve = parsed.flag("approve");
        let deny = parsed.flag("deny");
        let spec = parsed.flag("spec");

        let data_dir = crate::terminal::data_dir();
        let sup = Supervisor::open(data_dir.clone()).unwrap_or_else(|e| {
            eprintln!("pipeline: open runtime: {e}");
            std::process::exit(1);
        });

        // Gate resolution mode: settle the gate, then resume the pipeline
        // from its persisted spec. Without the resume the approval would be
        // a dead letter: the run would sit `running` forever.
        let denied_flag = approve.is_none();
        if let Some(stage) = approve.clone().or_else(|| deny.clone()) {
            let denied = denied_flag;
            let op_id = format!("{run_id}:{stage}");
            let store = sup.operations();
            let op = match store.get(&op_id) {
                Ok(Some(op)) => op,
                _ => {
                    eprintln!("pipeline: no gate {op_id}");
                    std::process::exit(1);
                }
            };
            if op.status == OperationStatus::Completed || op.status == OperationStatus::Failed {
                eprintln!("pipeline: gate {op_id} already settled");
                std::process::exit(1);
            }
            let mut state = op.state.clone();
            state["decision"] = serde_json::json!(if denied { "denied" } else { "approved" });
            let next = if denied {
                OperationStatus::Failed
            } else {
                OperationStatus::Completed
            };
            if let Err(e) = store.transition(&op_id, op.version, next, state) {
                eprintln!("pipeline: {e}");
                std::process::exit(1);
            }
            println!(
                "gate {op_id} {}",
                if denied { "denied" } else { "approved" }
            );
            match load_spec(&sup, &run_id) {
                Some(spec) => drive(&sup, &data_dir, &run_id, &spec),
                None => {
                    println!("no persisted spec for {run_id}: re-run with --spec to continue");
                }
            }
            return;
        }

        // Run mode.
        let Some(spec) = spec else {
            eprintln!(
                "usage: pantheon pipeline [--spec \"task\"] | <run_id> --approve <stage> | <run_id> --deny <stage>"
            );
            std::process::exit(2);
        };

        // Record the run in the ledger for observability.
        sup.start_run(&run_id).unwrap_or_else(|e| {
            eprintln!("pipeline: start run: {e}");
            std::process::exit(1);
        });

        drive(&sup, &data_dir, &run_id, &spec);
    }

    /// The pipeline spec, persisted at the first park so a later
    /// `--approve`/`--deny` invocation (a different process) can resume.
    fn spec_op_id(run_id: &str) -> String {
        format!("{run_id}:$spec")
    }

    fn load_spec(sup: &Supervisor, run_id: &str) -> Option<String> {
        sup.operations()
            .get(&spec_op_id(run_id))
            .ok()
            .flatten()
            .and_then(|op| {
                op.state
                    .get("spec")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
    }

    fn drive(sup: &Supervisor, data_dir: &std::path::Path, run_id: &str, spec: &str) {
        let data_dir = data_dir.to_path_buf();
        let cfg = Config::load_or_report(&data_dir);
        let policy = policy_for(cfg.as_ref());
        let exec = RuntimeExecutor {
            data_dir: data_dir.clone(),
            policy: policy.clone(),
        };
        // The evaluator runs by default; PANTHEON_PIPELINE_EVAL=0 disables it
        // (the human gate still gates regardless).
        let eval_enabled = std::env::var("PANTHEON_PIPELINE_EVAL")
            .map(|v| v != "0")
            .unwrap_or(true);
        let noop = AcceptAllEvaluator;
        let strict = RuntimeEvaluator {
            data_dir: data_dir.clone(),
            policy,
        };
        let runner = PipelineRunner {
            store: sup.operations(),
            run_id: run_id.to_string(),
            executor: &exec,
            evaluator: if eval_enabled { &strict } else { &noop },
            // `[budget] max_iterations` in config.toml; absent = 3.
            max_iterations: cfg.as_ref().map(|c| c.pipeline_iterations()).unwrap_or(3),
        };
        match runner.run(spec) {
            Ok(outcome) => {
                for (stage, text) in &outcome.outputs {
                    println!("== {stage} ==");
                    // Stage output is model text: untrusted, and printed
                    // outside any ratatui buffer, so escape sequences would
                    // reach the terminal verbatim.
                    println!("{}", crate::sanitize::strip_control(text));
                    println!();
                }
                sup.complete(run_id).unwrap_or_else(|e| {
                    eprintln!("pipeline: complete run: {e}");
                });
            }
            Err(e) => match e.code.as_str() {
                "PIPELINE_GATE" => {
                    // Persist the spec so the approving process can resume.
                    let store = sup.operations();
                    if store.get(&spec_op_id(run_id)).ok().flatten().is_none() {
                        let _ = store.create(
                            spec_op_id(run_id),
                            "pipeline.spec",
                            serde_json::json!({"spec": spec}),
                        );
                    }
                    println!("run id: {run_id}");
                    println!("pipeline parked: {e}");
                    println!("approve with: pantheon pipeline {run_id} --approve <stage>");
                    println!("deny with:    pantheon pipeline {run_id} --deny <stage>");
                }
                _ => {
                    eprintln!("pipeline: {e}");
                    sup.fail(run_id, &e.code).unwrap_or_else(|x| {
                        eprintln!("pipeline: mark failed: {x}");
                    });
                    std::process::exit(1);
                }
            },
        }
    }
}

pub mod plugin_toggle {
    //! Interactive plugin toggle screen: list with space/enter
    //! to toggle.
    //!
    //! Lists every plugin (bundled + third-party) with kind, version,
    //! enabled state, and the trust note - plugins run code with the
    //! operator's user privileges, which is the distinction the approval UI
    //! must surface (see `pantheon_extensions::bundled` docs): an MCP
    //! server is an integration Pantheon talks to, never code it runs.
    //!
    //! The screen is a one-shot crossterm alternate-screen loop mirroring
    //! `model::pick`: raw mode + a Drop guard that always restores the
    //! terminal. Toggling goes through the same [`crate::plugins_verb`]
    //! machinery as the `pantheon plugins enable/disable` verbs, so the
    //! screen and the verbs can never disagree.

    use crate::plugins_verb::{apply_toggle, collect_rows, PluginRow};
    use crate::sanitize::strip_control;
    use std::path::Path;

    /// The trust distinction, shown on every render. Condensed from the
    /// `pantheon_extensions::bundled` module docs.
    pub(crate) const TRUST_NOTE: &str =
        "trust: a plugin is code Pantheon runs on your machine with your user privileges - \
    Pantheon does not sandbox plugins, so enabling one is consent to run its code. \
    MCP servers are different: integrations Pantheon talks to, never code it runs.";

    struct TermGuard;
    impl Drop for TermGuard {
        fn drop(&mut self) {
            use crossterm::terminal::disable_raw_mode;
            let _ = disable_raw_mode();
            use crossterm::{cursor, execute};
            let _ = execute!(std::io::stdout(), cursor::Show);
        }
    }

    /// One rendered row: `[*] name  kind  version  status  description`.
    /// Pure so it is unit-testable without a TTY.
    ///
    /// Every manifest-derived field is control-stripped before it is placed
    /// in the row. This screen runs in raw mode with no alternate screen, so
    /// an unescaped CSI or OSC in a third-party `description` could clear the
    /// screen, move the cursor, or retitle the terminal while the operator
    /// believes they are looking at a list.
    pub(crate) fn format_row(row: &PluginRow, selected: bool) -> String {
        let marker = if selected { ">" } else { " " };
        let check = if row.enabled { "[*]" } else { "[ ]" };
        let status = if row.enabled { "enabled" } else { "disabled" };
        let bundled = if row.bundled { " bundled" } else { "" };
        let line = format!(
            "{marker} {check} {:<24} {:<4} {:<10} {:<8} {}{}",
            strip_control(&row.name),
            strip_control(row.kind),
            strip_control(&row.version),
            status,
            strip_control(&row.description),
            bundled
        );
        if selected {
            format!("\x1b[1m{line}\x1b[0m")
        } else {
            line
        }
    }

    /// Run the interactive toggle screen against `data_dir`. Falls back to
    /// a plain list when the terminal cannot be taken over.
    pub(crate) fn run_toggle_screen(data_dir: &Path) {
        use crossterm::{
            cursor,
            event::{self, Event, KeyCode, KeyModifiers},
            execute,
            terminal::{enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
        };

        let mut rows = collect_rows(data_dir);
        if rows.is_empty() {
            println!("no plugins installed");
            return;
        }
        if enable_raw_mode().is_err() {
            for r in &rows {
                println!("{}", format_row(r, false));
            }
            return;
        }
        let _guard = TermGuard;
        let mut stdout = std::io::stdout();
        if execute!(stdout, EnterAlternateScreen, cursor::Hide).is_err() {
            return;
        }
        // LeaveAlternateScreen must run before the guard's Drop (which only
        // restores raw mode + cursor); do it explicitly on every exit path.
        let leave = |out: &mut std::io::Stdout| {
            let _ = execute!(*out, LeaveAlternateScreen);
        };

        let mut selected = 0usize;
        let mut status = String::new();
        let visible_rows = 14usize;

        loop {
            if selected >= rows.len() {
                selected = rows.len().saturating_sub(1);
            }
            let start = if rows.len() <= visible_rows {
                0
            } else {
                selected
                    .saturating_sub(visible_rows / 2)
                    .min(rows.len() - visible_rows)
            };

            execute!(stdout, cursor::MoveTo(0, 0)).ok();
            let _ = crossterm::terminal::Clear(crossterm::terminal::ClearType::All);
            println!("Plugins - toggle what the agent may load\r");
            println!("  ↑↓ navigate · space/enter toggle · r refresh · q/Esc quit\r");
            println!("\r");
            for (i, row) in rows.iter().enumerate().skip(start).take(visible_rows) {
                println!("  {}\r", format_row(row, i == selected));
            }
            println!("\r");
            println!("  {TRUST_NOTE}\r");
            if !status.is_empty() {
                println!("\r  {status}\r");
            }
            let _ = std::io::Write::flush(&mut stdout);

            let ev = match event::read() {
                Ok(ev) => ev,
                Err(_) => {
                    leave(&mut stdout);
                    return;
                }
            };
            if let Event::Key(k) = ev {
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        leave(&mut stdout);
                        return;
                    }
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        leave(&mut stdout);
                        return;
                    }
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down => {
                        if selected + 1 < rows.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = rows.len().saturating_sub(1),
                    KeyCode::Char('r') => {
                        rows = collect_rows(data_dir);
                        status = format!("refreshed ({} plugins)", rows.len());
                    }
                    KeyCode::Char(' ') | KeyCode::Enter => {
                        if rows.is_empty() {
                            continue;
                        }
                        let row = &rows[selected];
                        let new_state = !row.enabled;
                        match apply_toggle(data_dir, row, new_state) {
                            Ok(msg) => {
                                rows[selected].enabled = new_state;
                                status = msg;
                            }
                            Err(e) => {
                                status = format!("error: {e}");
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::plugins_verb::Toggle;

        fn row(name: &str, enabled: bool, bundled: bool) -> PluginRow {
            PluginRow {
                name: name.to_string(),
                kind: "tool",
                version: "1.0".to_string(),
                description: "Does things.".to_string(),
                bundled,
                enabled,
                toggle: Toggle::BundledRegistry,
            }
        }

        #[test]
        fn format_row_shows_state_and_bundled_tag() {
            let on = format_row(&row("demo", true, true), false);
            assert!(on.contains("[*]"), "enabled marker missing: {on}");
            assert!(on.contains("enabled"), "status missing: {on}");
            assert!(on.contains("bundled"), "bundled tag missing: {on}");
            assert!(on.contains("demo"), "name missing: {on}");
            assert!(!on.contains("\x1b[1m"), "unselected row must not be bold");

            let off = format_row(&row("demo", false, false), false);
            assert!(off.contains("[ ]"), "disabled marker missing: {off}");
            assert!(off.contains("disabled"), "status missing: {off}");
            assert!(
                !off.contains("bundled"),
                "third-party row wrongly tagged: {off}"
            );

            let sel = format_row(&row("demo", true, false), true);
            assert!(
                sel.starts_with(">\x1b[1m") || sel.contains("\x1b[1m"),
                "selected row must be bold: {sel:?}"
            );
            assert!(sel.contains('>'), "selected marker missing: {sel:?}");
        }

        #[test]
        fn trust_note_states_the_privilege_distinction() {
            assert!(
                TRUST_NOTE.contains("user privileges"),
                "trust note must name the privilege level"
            );
            assert!(
                TRUST_NOTE.contains("does not sandbox"),
                "trust note must state the no-sandbox fact"
            );
            assert!(
                TRUST_NOTE.contains("MCP"),
                "trust note must carry the plugin-vs-MCP distinction"
            );
        }
    }
}

pub mod plugins_verb {
    //! `pantheon plugins`: list, enable/disable, install, and the interactive toggle.
    //!
    //! Four plugin sources feed one merged view:
    //!
    //! 1. The embedded bundled-plugin registry (`pantheon_exec::plugins`,
    //!    tool and hook kinds) - toggled through the `[plugins]` config
    //!    table via [`pantheon_exec::plugins::set_bundled_enabled`].
    //! 2. The extensions bundled catalog (`pantheon_extensions`, e.g.
    //!    `time-gap`) - toggled through the same `[plugins]` config table
    //!    via [`pantheon_extensions::set_enabled`]. Both bundled sources
    //!    share one enablement state: the config file wins over any
    //!    manifest flag.
    //! 3. Third-party tool plugins from
    //!    [`pantheon_exec::plugins::discover_plugins`] - toggled by
    //!    rewriting the manifest `enabled` flag in place (their gate is the
    //!    approval store, enforced at spawn).
    //! 4. Third-party hook plugins from the extensions dir - approval-gated;
    //!    listed read-only here, toggled with
    //!    `pantheon extensions approve <name>`.

    use std::path::{Path, PathBuf};

    /// How one row is toggled.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Toggle {
        /// Bundled plugin from the embedded registry (tool or hook):
        /// `[plugins.<name>]` via `pantheon_exec::plugins::set_bundled_enabled`.
        BundledRegistry,
        /// Hook plugin from the extensions catalog: `[plugins.<name>]` via
        /// `pantheon_extensions::set_enabled`.
        ExtensionsCatalog,
        /// Third-party tool plugin: manifest `enabled` flag in place.
        ManifestFlag,
        /// Third-party hook plugin: the approval store gates it; not
        /// toggleable from here.
        ApprovalGated,
    }

    /// One row of the merged plugin view.
    #[derive(Debug, Clone)]
    pub(crate) struct PluginRow {
        pub name: String,
        /// "tool" | "hook".
        pub kind: &'static str,
        pub version: String,
        pub description: String,
        pub bundled: bool,
        pub enabled: bool,
        pub toggle: Toggle,
    }

    /// Shorten a description to one line for table display.
    fn one_line(s: &str, max: usize) -> String {
        let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.len() <= max {
            return flat;
        }
        let mut cut = max;
        while cut > 0 && !flat.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", flat[..cut].trim_end())
    }

    /// Is the bundled plugin `name` enabled? The `[plugins.<name>]` config
    /// entry wins when present; when absent the bundled manifest's own
    /// `enabled` flag (`manifest_default`) is the default. A missing config
    /// counts as empty, so manifest defaults apply; an unparsable config
    /// fails closed (disabled) - a typo must never silently flip a plugin on.
    fn config_enabled(data_dir: &Path, name: &str, manifest_default: bool) -> bool {
        pantheon_extensions::bundled::load_config(data_dir)
            .map(|c| {
                pantheon_extensions::bundled::is_enabled_with_default(&c, name, manifest_default)
            })
            .unwrap_or(false)
    }

    /// The merged plugin view: bundled (embedded registry + extensions
    /// catalog) and third-party discovered plugins, sorted by name. The
    /// embedded registry wins name collisions so a plugin that exists in
    /// both catalogs renders once.
    pub(crate) fn collect_rows(data_dir: &Path) -> Vec<PluginRow> {
        let mut rows: Vec<PluginRow> = Vec::new();
        let seen = |rows: &[PluginRow], name: &str| rows.iter().any(|r| r.name == name);

        // 1. Embedded bundled-plugin registry (tool + hook kinds).
        for info in pantheon_exec::plugins::bundled_plugin_infos() {
            rows.push(PluginRow {
                name: info.name.clone(),
                kind: info.kind.as_str(),
                version: info.version,
                description: info.description,
                bundled: true,
                enabled: config_enabled(data_dir, &info.name, info.enabled),
                toggle: Toggle::BundledRegistry,
            });
        }
        // 2. Extensions bundled catalog (currently empty; machinery kept for
        // the next bundled plugin).
        for p in pantheon_extensions::bundled_plugins() {
            if seen(&rows, &p.name) {
                continue;
            }
            rows.push(PluginRow {
                name: p.name.clone(),
                kind: p.kind.as_str(),
                version: p.version,
                description: p.description,
                bundled: true,
                enabled: config_enabled(data_dir, &p.name, p.enabled),
                toggle: Toggle::ExtensionsCatalog,
            });
        }
        // 3. Third-party tool plugins. (Discovery also best-effort seeds
        // the bundled registry first; bundled paths are skipped here
        // they are covered by sources 1 and 2 above.)
        let project_root = std::env::current_dir().unwrap_or_else(|_| data_dir.to_path_buf());
        for p in pantheon_exec::plugins::discover_plugins(data_dir, &project_root) {
            // Canonical bundled only: a hand-dropped dir under `bundled/`
            // is listed here as third-party (approval-gated), not hidden
            // behind the bundled rows above.
            if pantheon_exec::plugin_approval::is_canonical_bundled(&p) {
                continue;
            }
            if seen(&rows, &p.manifest.name) {
                continue;
            }
            rows.push(PluginRow {
                name: p.manifest.name.clone(),
                kind: "tool",
                version: p.manifest.version.clone(),
                description: p.manifest.description.clone(),
                bundled: false,
                enabled: p.manifest.enabled,
                toggle: Toggle::ManifestFlag,
            });
        }
        // 4. Third-party hook plugins: top-level dirs of the extensions dir
        // with a plugin.yaml, minus `bundled/`. Enabled = approved in the
        // approval store for the current dir hash (best-effort).
        let ext_dir = crate::terminal::ext_dir();
        if let Ok(entries) = std::fs::read_dir(&ext_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() || !path.join("plugin.yaml").exists() {
                    continue;
                }
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "bundled" || n.starts_with('.'))
                {
                    continue;
                }
                let manifest =
                    match pantheon_extensions::PluginManifest::load(&path.join("plugin.yaml")) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                if seen(&rows, &manifest.name) {
                    continue;
                }
                let enabled = pantheon_api::approval::dir_hash(&path)
                    .map(|h| {
                        pantheon_api::approval::is_approved(
                            &ext_dir,
                            &manifest.name,
                            &manifest.version,
                            &h,
                        )
                    })
                    .unwrap_or(false);
                rows.push(PluginRow {
                    name: manifest.name,
                    kind: "hook",
                    version: manifest.version,
                    description: manifest.description,
                    bundled: false,
                    enabled,
                    toggle: Toggle::ApprovalGated,
                });
            }
        }

        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Toggle one row. Returns a human confirmation on success.
    pub(crate) fn apply_toggle(
        data_dir: &Path,
        row: &PluginRow,
        enabled: bool,
    ) -> Result<String, String> {
        match row.toggle {
            Toggle::BundledRegistry => {
                pantheon_exec::plugins::set_bundled_enabled(data_dir, &row.name, enabled)
                    .map_err(|e| e.to_string())?;
            }
            Toggle::ExtensionsCatalog => {
                pantheon_extensions::set_enabled(data_dir, &row.name, enabled)
                    .map_err(|e| e.to_string())?;
            }
            Toggle::ManifestFlag => {
                let project_root =
                    std::env::current_dir().unwrap_or_else(|_| data_dir.to_path_buf());
                let found = pantheon_exec::plugins::discover_plugins(data_dir, &project_root);
                let plugin = found
                    .iter()
                    .find(|p| {
                        p.manifest.name == row.name
                            && !pantheon_exec::plugin_approval::is_bundled(p)
                    })
                    .ok_or_else(|| format!("plugin '{}' is no longer discovered", row.name))?;
                pantheon_exec::plugins::set_enabled(plugin, enabled).map_err(|e| e.to_string())?;
            }
            Toggle::ApprovalGated => {
                return Err(format!(
                    "third-party hook plugin '{}' is approval-gated; use `pantheon extensions approve {}`",
                    row.name, row.name
                ));
            }
        }
        Ok(format!(
            "{} {}",
            if enabled { "enabled" } else { "disabled" },
            row.name
        ))
    }

    fn print_list(rows: &[PluginRow]) {
        if rows.is_empty() {
            println!("no plugins installed");
            return;
        }
        println!(
            "  {:<26} {:<4} {:<10} {:<8}  DESCRIPTION",
            "NAME", "KIND", "VERSION", "STATUS"
        );
        for r in rows {
            let status = if r.enabled { "enabled" } else { "disabled" };
            let tag = if r.bundled { " [bundled]" } else { "" };
            println!(
                "  {:<26} {:<4} {:<10} {:<8}  {}{}",
                r.name,
                r.kind,
                r.version,
                status,
                one_line(&r.description, 60),
                tag
            );
        }
    }

    fn cmd_list(data_dir: &Path) {
        print_list(&collect_rows(data_dir));
    }

    fn plugins_help() {
        eprintln!("usage: pantheon plugins <list|enable|disable|install> ...");
        eprintln!("  (no subcommand)      interactive toggle screen (TTY) or list (piped)");
        eprintln!("  list                 bundled + third-party plugins, with kind/version/status");
        eprintln!("  enable <name>        enable a plugin");
        eprintln!("  disable <name>       disable a plugin");
        eprintln!("  install <name>       install a plugin from the catalog by name");
        eprintln!();
        eprintln!("bundled plugins toggle through [plugins.<name>] in config.toml;");
        eprintln!("third-party tool plugins toggle their manifest flag in place;");
        eprintln!("third-party hook plugins are approval-gated");
        eprintln!("(`pantheon extensions approve <name>`).");
    }

    /// Entry point called from the verb dispatch in `terminal::run`.
    pub fn cmd_plugins(args: &[String]) {
        let dd = crate::terminal::data_dir();
        match args.get(2).map(String::as_str) {
            Some("list") => cmd_list(&dd),
            Some(cmd @ ("enable" | "disable")) => {
                if args.len() < 4 {
                    eprintln!("usage: pantheon plugins {cmd} <name>");
                    std::process::exit(2);
                }
                let name = &args[3];
                let enabled = cmd == "enable";
                let rows = collect_rows(&dd);
                let row = rows.iter().find(|r| r.name == *name).unwrap_or_else(|| {
                    eprintln!("plugins {cmd}: no plugin named '{name}'");
                    std::process::exit(1);
                });
                match apply_toggle(&dd, row, enabled) {
                    Ok(msg) => println!("{msg}"),
                    Err(e) => {
                        eprintln!("plugins {cmd}: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some("install") => {
                if args.len() < 4 {
                    eprintln!("usage: pantheon plugins install <name>");
                    eprintln!(
                        "catalog plugins: {}",
                        pantheon_exec::plugins::catalog_names().join(", ")
                    );
                    std::process::exit(2);
                }
                let name = &args[3];
                let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                pantheon_exec::plugins::install_catalog(name, &dd, &project_root).unwrap_or_else(
                    |e| {
                        eprintln!("plugins install: {e}");
                        std::process::exit(1);
                    },
                );
                println!("installed {name}");
            }
            Some("--help") | Some("-h") => {
                plugins_help();
                std::process::exit(0);
            }
            Some(other) => {
                eprintln!("plugins: unknown subcommand {other:?}");
                plugins_help();
                std::process::exit(2);
            }
            // Bare `pantheon plugins`: the interactive toggle screen on a
            // TTY table, plain list when piped.
            None => {
                if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    crate::plugin_toggle::run_toggle_screen(&dd);
                } else {
                    cmd_list(&dd);
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Scratch data dir for one test. Unique per call so tests stay
        /// parallel-safe.
        fn scratch_data_dir(tag: &str) -> PathBuf {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "pantheon-plugins-verb-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn write_tool_plugin(dd: &Path, name: &str, enabled: bool) {
            let dir = dd.join("plugins").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("manifest.yaml"),
                format!("name: {name}\nversion: \"1.0\"\ndescription: Test plugin {name}.\nenabled: {enabled}\n"),
            )
            .unwrap();
        }

        #[test]
        fn one_line_truncates_and_flattens() {
            assert_eq!(one_line("hello", 60), "hello");
            assert_eq!(one_line("a\nb  c", 60), "a b c");
            let long = "x".repeat(100);
            let cut = one_line(&long, 60);
            assert!(cut.len() <= 61 + 3, "unexpected length: {}", cut.len());
            assert!(cut.ends_with("..."));
        }

        #[test]
        fn collect_rows_lists_bundled_registry_and_third_party() {
            let dd = scratch_data_dir("rows");
            // A third-party tool plugin on disk.
            write_tool_plugin(&dd, "acme-tool", true);

            let rows = collect_rows(&dd);
            // The embedded bundled registry (cf-ops, noisegate, ...) ships as
            // bundled rows; the extensions catalog is empty since time-gap
            // went native, so no Toggle::ExtensionsCatalog rows appear.
            let registry = rows.iter().filter(|r| r.bundled).collect::<Vec<_>>();
            assert!(
                registry.len() >= 2,
                "embedded registry plugins missing: {rows:?}"
            );
            assert!(registry.iter().all(|r| r.toggle == Toggle::BundledRegistry));
            assert!(
                !rows.iter().any(|r| r.toggle == Toggle::ExtensionsCatalog),
                "extensions catalog is empty: {rows:?}"
            );
            // The third-party tool plugin shows with its manifest state.
            let acme = rows
                .iter()
                .find(|r| r.name == "acme-tool")
                .expect("third-party plugin missing from plugin list");
            assert_eq!(acme.kind, "tool");
            assert!(!acme.bundled);
            assert!(acme.enabled);
            assert_eq!(acme.toggle, Toggle::ManifestFlag);
            // Sorted by name.
            let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            assert_eq!(names, sorted);
        }

        // The bundled-catalog row tests (dedup across bundled sources, the
        // `[plugins]` config toggle round-trip) were removed with the catalog
        // itself: time-gap went native (`[temporal]`), and until the next
        // bundled plugin ships there is no catalog row to drive them. The
        // config-enablement logic stays pinned in pantheon-extensions' tests.

        #[test]
        fn toggle_manifest_flag_for_third_party_tool() {
            let dd = scratch_data_dir("manifest-toggle");
            write_tool_plugin(&dd, "acme-tool", false);
            let rows = collect_rows(&dd);
            let row = rows.iter().find(|r| r.name == "acme-tool").unwrap();

            apply_toggle(&dd, row, true).unwrap();
            // The manifest flag was rewritten in place.
            let rows = collect_rows(&dd);
            let row = rows.iter().find(|r| r.name == "acme-tool").unwrap();
            assert!(row.enabled);
            apply_toggle(&dd, row, false).unwrap();
            let rows = collect_rows(&dd);
            let row = rows.iter().find(|r| r.name == "acme-tool").unwrap();
            assert!(!row.enabled);
        }

        #[test]
        fn toggle_unknown_name_is_an_error() {
            let dd = scratch_data_dir("toggle-unknown");
            let row = PluginRow {
                name: "nope".to_string(),
                kind: "tool",
                version: String::new(),
                description: String::new(),
                bundled: false,
                enabled: false,
                toggle: Toggle::BundledRegistry,
            };
            let err = apply_toggle(&dd, &row, true).unwrap_err();
            assert!(err.contains("not a bundled plugin"), "unexpected: {err}");
        }

        #[test]
        fn toggle_approval_gated_hook_is_rejected() {
            let dd = scratch_data_dir("toggle-gated");
            let row = PluginRow {
                name: "acme-hook".to_string(),
                kind: "hook",
                version: "1.0".to_string(),
                description: String::new(),
                bundled: false,
                enabled: false,
                toggle: Toggle::ApprovalGated,
            };
            let err = apply_toggle(&dd, &row, true).unwrap_err();
            assert!(err.contains("approval-gated"), "unexpected: {err}");
        }
    }
}

pub mod logs {
    //! `pantheon logs` - read the runtime's log files.
    //!
    //! The runtime had no logs at all until `pantheon_api::logging` landed, so
    //! this verb was briefly a run trace wearing the name of a log reader. It is
    //! now what its name says: it reads `<data_dir>/logs/*.log`, and the run trace
    //! is back to being a run trace.
    //!
    //! The shape follows `hermes_cli/logs.py` - named files, `--level`, `--follow`,
    //! `--since`, `logs list` - because that reader is the thing being matched. Two
    //! deliberate differences:
    //!
    //! - The file list is a closed set (`pantheon_api::logging::KNOWN_LOGS`) and
    //!   not a `*.log` glob. A data dir also holds `ledger.db.<stamp>.bak` copies
    //!   from `repair` and any editor swap file, all of which a glob would present
    //!   as logs.
    //! - Filtering is done by re-parsing each line rather than by keeping the
    //!   files pre-split by level, so `--level errors` and `--level warning` on the
    //!   same file are one code path.

    use pantheon_api::logging::{self, Level, KNOWN_LOGS};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn usage() -> &'static str {
        concat!(
            "usage:\n",
            "  pantheon logs                       tail agent.log\n",
            "  pantheon logs list                  the log files and their sizes\n",
            "  pantheon logs <name>                tail one file (agent|errors|gateway)\n",
            "  \n",
            "  pantheon logs [name] [-n N]         last N lines (default 50)\n",
            "                     [-f]             follow, like tail -f\n",
            "                     [--level L]      debug|info|warning|error\n",
            "                     [--since 1h]     only records newer than 1s/1m/1h/1d\n",
            "                     [--grep TEXT]    only lines containing TEXT",
        )
    }

    /// A log file by the name a user types, resolved against the closed set.
    fn resolve(name: &str) -> Result<PathBuf, String> {
        // Resolve the name BEFORE reading the global sink. The other order turns
        // every typo into "logging was not initialised", which sends the user
        // chasing a startup problem instead of their typo.
        let want = match name {
            "agent" => logging::AGENT_LOG,
            "errors" | "error" => logging::ERRORS_LOG,
            "gateway" => logging::GATEWAY_LOG,
            other => {
                return Err(format!(
                    "unknown log '{other}'. known: agent, errors, gateway (or run `pantheon logs list`)"
                ))
            }
        };
        let dirs = logging::log_dir().ok_or_else(|| {
            "logging was not initialised, so there is no log directory yet".to_string()
        })?;
        Ok(dirs.join(want))
    }

    /// `1h`, `30m`, `2d`, `90s` into a cutoff. `None` when unparseable, which the
    /// caller turns into an error rather than "no filter" - a silently ignored
    /// `--since` is how someone ends up reading a whole file believing it is
    /// recent.
    fn parse_since(s: &str) -> Result<SystemTime, String> {
        let t = s.trim().to_ascii_lowercase();
        let (num, unit) = t.split_at(t.len().saturating_sub(1));
        let n: u64 = num
            .trim()
            .parse()
            .map_err(|_| format!("--since: '{s}' is not a duration like 30m, 2h, 1d"))?;
        let secs = match unit {
            "s" => n,
            "m" => n * 60,
            "h" => n * 3600,
            "d" => n * 86_400,
            _ => {
                return Err(format!(
                    "--since: unknown unit '{unit}' (use s, m, h, or d)"
                ))
            }
        };
        Ok(SystemTime::now() - Duration::from_secs(secs))
    }

    struct Options {
        name: String,
        lines: usize,
        follow: bool,
        level: Option<Level>,
        since: Option<SystemTime>,
        grep: Option<String>,
    }

    pub fn cmd_logs(args: &[String]) {
        // `list` is a mode, not a file, so it is handled before option parsing.
        if args.get(2).map(String::as_str) == Some("list") {
            return list_files();
        }
        if args.iter().any(|a| a == "--help" || a == "-h") {
            println!("{}", usage());
            return;
        }

        let mut name: Option<String> = None;
        let mut lines = 50usize;
        let mut follow = false;
        let mut level = None;
        let mut since = None;
        let mut grep = None;

        let mut i = 2;
        while i < args.len() {
            let a = args[i].as_str();
            // Each flag that takes a value refuses a missing one by name, rather
            // than consuming the next flag and failing somewhere unrelated.
            // A flag-like next token is not a value either: `logs --grep
            // --level error` used to swallow `--level` as the grep pattern
            // and then treat `error` as the log name.
            let mut need = |what: &str| -> String {
                i += 1;
                match args.get(i) {
                    Some(v) if !v.starts_with('-') => v.clone(),
                    Some(v) => {
                        eprintln!(
                            "logs: {what} needs a value, got flag-like {v:?}\n{}",
                            usage()
                        );
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("logs: {what} needs a value\n{}", usage());
                        std::process::exit(2);
                    }
                }
            };
            match a {
                "-n" | "--lines" => {
                    let v = need("-n");
                    lines = v.parse().unwrap_or_else(|_| {
                        eprintln!("logs: -n expects a number, got '{v}'");
                        std::process::exit(2);
                    });
                }
                "-f" | "--follow" => follow = true,
                "--level" => {
                    let v = need("--level");
                    level = Some(Level::parse(&v).unwrap_or_else(|| {
                        eprintln!("logs: unknown level '{v}'. use debug, info, warning, or error");
                        std::process::exit(2);
                    }));
                }
                "--since" => {
                    let v = need("--since");
                    since = Some(parse_since(&v).unwrap_or_else(|e| {
                        eprintln!("logs: {e}");
                        std::process::exit(2);
                    }));
                }
                "--grep" => grep = Some(need("--grep")),
                other if other.starts_with('-') => {
                    eprintln!("logs: unknown flag '{other}'\n{}", usage());
                    std::process::exit(2);
                }
                // The file name is the one bare positional. Tracking it as an
                // Option rather than defaulting to "agent" up front is what makes
                // `logs agent --level error` work: with a string default, naming
                // the default explicitly looked like a second argument and was
                // rejected, so the documented spelling of the default silently
                // did nothing.
                other => {
                    if name.is_none() {
                        name = Some(other.to_string());
                    } else {
                        eprintln!("logs: unexpected argument '{other}'\n{}", usage());
                        std::process::exit(2);
                    }
                }
            }
            i += 1;
        }

        let name = name.unwrap_or_else(|| "agent".to_string());
        let path = resolve(&name).unwrap_or_else(|e| {
            eprintln!("logs: {e}");
            std::process::exit(2);
        });
        let opts = Options {
            name,
            lines,
            follow,
            level,
            since,
            grep,
        };
        if let Err(e) = tail(&path, &opts) {
            eprintln!("logs: {e}");
            std::process::exit(1);
        }
    }

    fn list_files() {
        let Some(dir) = logging::log_dir() else {
            println!("no log directory yet (logging was not initialised)");
            return;
        };
        let mut any = false;
        for name in KNOWN_LOGS {
            let p = dir.join(name);
            match std::fs::metadata(&p) {
                Ok(m) => {
                    any = true;
                    println!("{name:<14} {}", human_bytes(m.len()));
                }
                Err(_) => println!("{name:<14} (not created yet)"),
            }
        }
        if !any {
            println!(
                "\nno log files yet. they appear once a turn, a gateway poll, or a warning happens."
            );
        }
    }

    fn human_bytes(n: u64) -> String {
        const K: f64 = 1024.0;
        if n < 1024 {
            format!("{n} B")
        } else if (n as f64) < K * K {
            format!("{:.1} KB", n as f64 / K)
        } else {
            format!("{:.1} MB", n as f64 / (K * K))
        }
    }

    /// Print matching lines, then optionally keep following.
    ///
    /// A missing file is not an error: on a fresh install the logs directory does
    /// not exist yet, and telling the user to run a turn is more useful than an
    /// `ENOENT`.
    fn tail(path: &Path, opts: &Options) -> Result<(), String> {
        if !path.exists() {
            println!(
                "no {} yet - it is created on the first turn or warning",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            return Ok(());
        }
        let mut offset = 0i64;
        emit_from(path, opts, &mut offset)?;

        if !opts.follow {
            return Ok(());
        }
        println!(" - following {} (ctrl-c to stop) - ", opts.name);
        let mut buf = String::new();
        loop {
            std::thread::sleep(Duration::from_millis(400));
            let len = std::fs::metadata(path)
                .map(|m| m.len() as i64)
                .unwrap_or(offset);
            if len < offset {
                // Truncated or rotated under us. Restart from the beginning rather
                // than seeking past the end and printing nothing forever.
                offset = 0;
                buf.clear();
            }
            if len == offset {
                continue;
            }
            buf.clear();
            emit_from(path, opts, &mut offset)?;
        }
    }

    /// Copy new bytes from `offset` to the end, filtering each complete line.
    fn emit_from(path: &Path, opts: &Options, offset: &mut i64) -> Result<(), String> {
        let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if *offset > data.len() as i64 {
            *offset = 0;
        }
        let mut start = *offset as usize;
        // Only whole lines: a partial trailing line is a write in progress, and
        // printing half of it then the rest later would split one record in two.
        let last_nl = match data[start..].iter().rposition(|b| *b == b'\n') {
            Some(p) => start + p + 1,
            None => return Ok(()),
        };
        let chunk = &data[start..last_nl];
        start = last_nl;
        *offset = start as i64;

        let text = String::from_utf8_lossy(chunk);
        let mut kept: Vec<&str> = Vec::new();
        for line in text.lines() {
            if let Some(l) = parse_level(line) {
                if let Some(min) = opts.level {
                    if l < min {
                        continue;
                    }
                }
            }
            if let Some(since) = opts.since {
                if let Some(ts) = parse_ts_ms(line) {
                    if ts >= 0 && timestamp_of(ts) < since {
                        continue;
                    }
                }
            }
            if let Some(g) = &opts.grep {
                if !line.contains(g.as_str()) {
                    continue;
                }
            }
            kept.push(line);
        }
        let out = std::io::stdout();
        let mut w = out.lock();
        if opts.follow {
            for l in &kept {
                // Log lines carry model and provider text. Strip terminal
                // control bytes so `pantheon logs` cannot repaint the
                // operator's terminal from log content.
                let _ = writeln!(w, "{}", crate::sanitize::strip_control(l));
            }
            let _ = w.flush();
        } else {
            // Only the tail, so `-n` is a real bound rather than "everything then
            // the last N", which on a long session prints a screenful nobody reads.
            let start = kept.len().saturating_sub(opts.lines);
            for l in &kept[start..] {
                let _ = writeln!(w, "{l}");
            }
        }
        Ok(())
    }

    /// A log timestamp as a `SystemTime`. Negative milliseconds (a pre-1970
    /// record, which no writer emits) are clamped rather than wrapped, because
    /// `Duration::from_millis` takes a `u64` and a cast would turn a small negative
    /// into a far-future instant that passes every `--since` filter.
    fn timestamp_of(ms: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
    }

    /// Width of the timestamp field: `YYYY-MM-DD HH:MM:SS.mmm` is 23 characters.
    /// Named as a constant because getting it wrong makes every filter a no-op
    /// rather than an error - `line.get(20..)` lands inside the millisecond field
    /// and the level parse silently fails, so `--level error` returns the whole
    /// file and looks like the filter is broken rather than the parser.
    const TS_WIDTH: usize = 23;

    fn parse_level(line: &str) -> Option<Level> {
        // "TIMESTAMP LEVEL [component] message"
        let after_ts = line.get(TS_WIDTH..)?;
        let rest = after_ts.trim_start();
        [Level::Debug, Level::Info, Level::Warning, Level::Error]
            .into_iter()
            .find(|l| rest.starts_with(l.as_str()))
    }

    fn parse_ts_ms(line: &str) -> Option<i64> {
        // "YYYY-MM-DD HH:MM:SS.mmm" at the start of the line, parsed by fixed
        // offset. Splitting on whitespace instead looks equivalent and is not: a
        // `trim_start` on the remainder eats the separator and shifts every field
        // by one, which silently yields a wrong (or absent) timestamp.
        const D: &[std::ops::Range<usize>] = &[0..4, 5..7, 8..10];
        const T: &[std::ops::Range<usize>] = &[11..13, 14..16, 17..19];
        const MS: std::ops::Range<usize> = 20..23;
        let ts = line.get(..TS_WIDTH)?;
        let num = |r: std::ops::Range<usize>| -> Option<i64> { ts.get(r)?.parse().ok() };
        let (y, mo, d) = (num(D[0].clone())?, num(D[1].clone())?, num(D[2].clone())?);
        let (h, mi, sec) = (num(T[0].clone())?, num(T[1].clone())?, num(T[2].clone())?);
        let millis = num(MS)?;
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
            return None;
        }
        let days = days_from_civil(y, mo, d);
        Some(((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000) + millis)
    }

    /// Inverse of the civil-from-days conversion in `logging`. Duplicated rather
    /// than exported because the reader is the only consumer and a private helper
    /// in the writer would be dead code there.
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }
}
