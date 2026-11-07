//! Doctor diagnostics merged into one file. External paths keep working through the
//! crate-root shims in lib.rs (same pattern as the providers merge).

pub mod checks {
    //! The built-in diagnostic checks, grouped by subsystem.
    //!
    //! Every check is a pure-ish function `fn(&DoctorCtx) -> Vec<Finding>`:
    //! read-only against the data dir (the skill scan reuses the same
    //! discovery the session runs, which idempotently seeds bundled skills),
    //! no agent tasks executed, no persistent state mutated, no credentials
    //! ever placed in a finding. Network I/O happens only in the opt-in
    //! provider reachability probe.

    use crate::finding::{Finding, Severity, Subsystem};
    use crate::probe::{detect_binary, tcp_probe, DETECT_TIMEOUT};
    use crate::registry::{CheckDef, DoctorCtx};
    use pantheon_api::config::Config;
    use std::path::PathBuf;

    /// All built-in checks, in canonical subsystem order.
    pub fn all_checks() -> Vec<CheckDef> {
        vec![
            // -- config ---------------------------------------------------
            CheckDef {
                id: "config.file",
                subsystem: Subsystem::Config,
                title: "config file parses",
                run: check_config_file,
            },
            CheckDef {
                id: "config.schema",
                subsystem: Subsystem::Config,
                title: "config schema validation",
                run: check_config_schema,
            },
            CheckDef {
                id: "config.deprecated_sections",
                subsystem: Subsystem::Config,
                title: "deprecated sections",
                run: check_deprecated_sections,
            },
            CheckDef {
                id: "config.agents",
                subsystem: Subsystem::Config,
                title: "agent identity tables",
                run: check_agents,
            },
            // -- runtime --------------------------------------------------
            CheckDef {
                id: "runtime.nightly_state",
                subsystem: Subsystem::Runtime,
                title: "nightly pass state",
                run: check_nightly_state,
            },
            CheckDef {
                id: "runtime.gateway_channels",
                subsystem: Subsystem::Runtime,
                title: "gateway channel tokens",
                run: check_gateway_channels,
            },
            // -- providers ------------------------------------------------
            CheckDef {
                id: "providers.model_configured",
                subsystem: Subsystem::Providers,
                title: "model section configured",
                run: check_model_configured,
            },
            CheckDef {
                id: "providers.model_key",
                subsystem: Subsystem::Providers,
                title: "model API key present",
                run: check_model_key,
            },
            CheckDef {
                id: "providers.reachable",
                subsystem: Subsystem::Providers,
                title: "provider reachable (opt-in probe)",
                run: check_provider_reachable,
            },
            // -- memory ---------------------------------------------------
            CheckDef {
                id: "memory.ledger",
                subsystem: Subsystem::Memory,
                title: "ledger database opens",
                run: check_ledger,
            },
            CheckDef {
                id: "memory.store",
                subsystem: Subsystem::Memory,
                title: "memory store opens",
                run: check_memory_store,
            },
            // -- tools ----------------------------------------------------
            CheckDef {
                id: "tools.plugins",
                subsystem: Subsystem::Tools,
                title: "plugins verify",
                run: check_plugins,
            },
            CheckDef {
                id: "tools.computer_use",
                subsystem: Subsystem::Tools,
                title: "computer-use driver",
                run: check_computer_use,
            },
            CheckDef {
                id: "tools.skill_deps",
                subsystem: Subsystem::Tools,
                title: "skill dependencies",
                run: check_skill_deps,
            },
            CheckDef {
                id: "tools.skills",
                subsystem: Subsystem::Tools,
                title: "skill tree parses",
                run: check_skills,
            },
            // -- security -------------------------------------------------
            CheckDef {
                id: "security.secrets_allowlist",
                subsystem: Subsystem::Security,
                title: "secrets boundary policy",
                run: check_secrets_allowlist,
            },
            CheckDef {
                id: "security.dotenv_permissions",
                subsystem: Subsystem::Security,
                title: ".env file permissions",
                run: check_dotenv_permissions,
            },
            CheckDef {
                id: "security.exec_sandbox",
                subsystem: Subsystem::Security,
                title: "exec sandbox (bwrap)",
                run: check_exec_sandbox,
            },
        ]
    }

    fn load_config(ctx: &DoctorCtx) -> Result<Config, pantheon_api::error::PantheonError> {
        Config::load(&ctx.data_dir)
    }

    // -- config -----------------------------------------------------------

    fn check_config_file(ctx: &DoctorCtx) -> Vec<Finding> {
        let path = Config::path(&ctx.data_dir);
        match load_config(ctx) {
            Ok(_) => vec![Finding::passed(
                "config.file",
                Subsystem::Config,
                format!("parsed {}", path.display()),
            )
            .with_details(format!("path: {}", path.display()))],
            Err(e) => {
                // A missing file is not a broken file: it used to report as
                // Critical "does not parse", sending users to fix TOML that
                // does not exist. Missing = not configured yet (setup);
                // present-but-unparseable stays Critical.
                if !path.exists() {
                    return vec![Finding::failed(
                        "config.file",
                        Subsystem::Config,
                        Severity::Warning,
                        format!("no config at {}: not configured yet", path.display()),
                        "nothing is configured yet - this is a fresh install, not a broken one",
                        "run `pantheon setup`",
                    )
                    .with_details(format!("path: {}", path.display()))];
                }
                vec![Finding::failed(
                "config.file",
                Subsystem::Config,
                Severity::Critical,
                format!("config does not parse: {}", e.cause),
                "nothing can run: the agent, the gateway, and the dashboard all need a parseable config",
                "run `pantheon setup`, or fix the TOML by hand",
            )
            .with_details(format!("path: {}\nerror: {}", path.display(), e.cause))]
            }
        }
    }

    fn check_config_schema(ctx: &DoctorCtx) -> Vec<Finding> {
        let cfg = match load_config(ctx) {
            Ok(c) => c,
            Err(_) => {
                // config.file already reported the parse failure as critical;
                // a second failure here would double-count the same root cause.
                return vec![Finding::skipped(
                    "config.schema",
                    Subsystem::Config,
                    "skipped: config does not parse (see config.file)",
                )];
            }
        };
        let problems = cfg.validate();
        // Inner-typo lint (see `unknown_inner_keys`): top-level unknowns
        // already warn at load, but a typo *inside* a section
        // (`model.providr`) was dropped silently everywhere. Doctor is the
        // surface that explains a hand edit that "did nothing".
        let mut findings: Vec<Finding> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(Config::path(&ctx.data_dir)) {
            for key in pantheon_api::config::unknown_inner_keys(&text) {
                findings.push(
                Finding::failed(
                    "config.schema",
                    Subsystem::Config,
                    Severity::Warning,
                    format!("unknown config key '{key}' is ignored; check for a typo"),
                    "the misspelled key has no effect - the feature it was meant to configure keeps its old behavior",
                    "fix the spelling in config.toml, or delete the key",
                )
                .with_details(key),
            );
            }
        }
        if problems.is_empty() {
            if findings.is_empty() {
                findings.push(Finding::passed(
                    "config.schema",
                    Subsystem::Config,
                    "validation clean",
                ));
            }
            return findings;
        }
        // A config problem is an error, not a warning: `doctor` is the
        // preflight a user runs before their first session, so "no [model]
        // section" has to be actionable, not advisory.
        let mut out: Vec<Finding> = problems
            .into_iter()
            .map(|p| {
                Finding::failed(
                    "config.schema",
                    Subsystem::Config,
                    Severity::Error,
                    format!("invalid config: {p}"),
                    "the misconfigured section may be ignored or break its feature",
                    "rerun `pantheon setup` or fix the config by hand",
                )
                .with_details(p)
            })
            .collect();
        out.extend(findings);
        out
    }

    /// Deprecated-section nudge for the unified nightly pass. `[nightly]`
    /// is the single authoritative section; legacy `[reflect]` /
    /// `[consolidation]` tables are honored field-by-field only when
    /// `[nightly]` is absent. When `[nightly]` exists, its knobs win -
    /// except enablement: a `[nightly]` table that makes no enable
    /// decision (no flag, no pin, no env) defers to the legacy flags
    /// instead of switching a previously enabled pass off.
    fn check_deprecated_sections(ctx: &DoctorCtx) -> Vec<Finding> {
        let cfg = match load_config(ctx) {
            Ok(c) => c,
            Err(_) => {
                return vec![Finding::skipped(
                    "config.deprecated_sections",
                    Subsystem::Config,
                    "skipped: config does not parse (see config.file)",
                )];
            }
        };
        let nightly_present = cfg.nightly.is_some();
        let mut out = Vec::new();
        if cfg.reflect.is_some() {
            out.push(Finding::failed(
            "config.deprecated_sections",
            Subsystem::Config,
            Severity::Warning,
            if nightly_present {
                "legacy [reflect] section present but ignored entirely while [nightly] exists"
                    .to_string()
            } else {
                "legacy [reflect] section present (deprecated)".to_string()
            },
            "self-improvement knobs may not apply where you expect",
            "copy `enabled`, `auto_turns` into [nightly], then delete those keys (the provider/model/timeout pin can stay)",
        ));
        }
        if cfg.consolidation.is_some() {
            out.push(Finding::failed(
            "config.deprecated_sections",
            Subsystem::Config,
            Severity::Warning,
            if nightly_present {
                "legacy [consolidation] section present but ignored entirely while [nightly] exists"
                    .to_string()
            } else {
                "legacy [consolidation] section present (deprecated)".to_string()
            },
            "self-improvement knobs may not apply where you expect",
            "copy `enabled`, `min_sessions`, `cron` into [nightly], then delete those keys (the provider/model/timeout pin can stay)",
        ));
        }
        if out.is_empty() {
            out.push(Finding::passed(
                "config.deprecated_sections",
                Subsystem::Config,
                "no deprecated sections",
            ));
        }
        out
    }

    fn check_agents(ctx: &DoctorCtx) -> Vec<Finding> {
        let cfg = match load_config(ctx) {
            Ok(c) => c,
            Err(_) => {
                return vec![Finding::skipped(
                    "config.agents",
                    Subsystem::Config,
                    "skipped: config does not parse (see config.file)",
                )];
            }
        };
        if cfg.agents.is_empty() {
            return vec![Finding::passed(
                "config.agents",
                Subsystem::Config,
                "no [agents] tables (anonymous runs)",
            )];
        }
        // One info finding naming every table, so a misconfigured [agents.*]
        // table is obvious, not silent. Info severity: listing identities is
        // context, never action.
        let mut tables: Vec<&String> = cfg.agents.keys().collect();
        tables.sort();
        let detail = tables
            .iter()
            .map(|t| {
                let id = &cfg.agents[*t];
                format!(
                    "{t} (name {:?}, namespace {:?}, policy {:?})",
                    id.name(t),
                    id.namespace(t),
                    id.policy.as_deref().unwrap_or("default"),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        vec![Finding::passed(
            "config.agents",
            Subsystem::Config,
            format!(
                "{} agent table(s): {}",
                tables.len(),
                tables
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .with_details(detail)]
    }

    // -- runtime ----------------------------------------------------------

    /// Master gate for the nightly pass over a whole [`Config`]: the pass
    /// pipeline and repair loop - runs iff this is true. Mirrors the TUI's
    /// entry-point rule so doctor and the launcher cannot disagree.
    fn nightly_pass_enabled(cfg: Option<&Config>) -> bool {
        let legacy = cfg.is_some_and(|c| {
            c.reflect.as_ref().is_some_and(|s| s.enabled)
                || c.consolidation.as_ref().is_some_and(|s| s.enabled)
        });
        match cfg.and_then(|c| c.nightly.as_ref()) {
            Some(n) => pantheon_api::config::nightly_enabled_with_legacy(n, legacy),
            None => legacy,
        }
    }

    fn check_nightly_state(ctx: &DoctorCtx) -> Vec<Finding> {
        let cfg = match load_config(ctx) {
            Ok(c) => c,
            Err(_) => {
                return vec![Finding::skipped(
                    "runtime.nightly_state",
                    Subsystem::Runtime,
                    "skipped: config does not parse (see config.file)",
                )];
            }
        };
        let on = nightly_pass_enabled(Some(&cfg));
        match cfg.nightly.as_ref() {
            Some(n) => {
                // A [nightly] table that decides nothing defers to legacy,
                // so its stock reason ("off (no flag, no model pin)") would
                // contradict an enabled verdict - name the legacy source.
                let legacy_on = cfg.reflect.as_ref().is_some_and(|s| s.enabled)
                    || cfg.consolidation.as_ref().is_some_and(|s| s.enabled);
                let env_pin = std::env::var("PANTHEON_NIGHTLY_PROVIDER")
                    .is_ok_and(|v| !v.trim().is_empty())
                    || std::env::var("PANTHEON_NIGHTLY_MODEL").is_ok_and(|v| !v.trim().is_empty());
                let reason = if on
                    && n.enabled.is_none()
                    && !pantheon_api::config::nightly_model_pin_present(n)
                    && !env_pin
                    && legacy_on
                {
                    "on via legacy [reflect]/[consolidation] enabled ([nightly] makes no enable decision)"
                } else {
                    pantheon_api::config::nightly_enabled_reason(n)
                };
                let pin = pantheon_api::config::nightly_model_pin_present(n);
                if on {
                    vec![Finding::passed(
                        "runtime.nightly_state",
                        Subsystem::Runtime,
                        format!("nightly pass enabled ({reason})"),
                    )]
                } else {
                    vec![Finding::failed(
                        "runtime.nightly_state",
                        Subsystem::Runtime,
                        Severity::Warning,
                        format!("nightly pass disabled ({reason})"),
                        "no nightly self-repair or memory consolidation runs",
                        if pin {
                            "set `enabled = true` under [nightly] or run `/nightly on` to re-enable"
                        } else {
                            "enable it: add a [nightly.model] pin, set `enabled = true`, run `/nightly on`, or use the dashboard toggle"
                        },
                    )]
                }
            }
            None => {
                let legacy_on = cfg.reflect.as_ref().is_some_and(|s| s.enabled)
                    || cfg.consolidation.as_ref().is_some_and(|s| s.enabled);
                if legacy_on {
                    vec![Finding::passed(
                    "runtime.nightly_state",
                    Subsystem::Runtime,
                    "nightly pass enabled via legacy [reflect]/[consolidation] (deprecated - move the knobs into [nightly])",
                )]
                } else {
                    vec![Finding::failed(
                    "runtime.nightly_state",
                    Subsystem::Runtime,
                    Severity::Warning,
                    "nightly pass disabled (default: no [nightly] section)",
                    "no nightly self-repair or memory consolidation runs",
                    "enable it: add a [nightly.model] pin, set `enabled = true`, run `/nightly on`, or use the dashboard toggle",
                )]
                }
            }
        }
    }

    fn check_gateway_channels(ctx: &DoctorCtx) -> Vec<Finding> {
        // No token means the surface is simply off, which is a legitimate
        // configuration - but a user who expects the gateway to answer
        // should see that it is idle, so this stays a warning, never
        // silent and never a failure. Only presence is reported, never the
        // token value.
        //
        // Sources mirror the daemon's resolution (env > gateway.env), plus
        // the configured channels themselves: the old check read only
        // process-env tokens, so a wizard-configured install (tokens in
        // gateway.env, channels in `[gateway.channels.*]`) reported idle
        // while the gateway itself ran fine.
        let mut live = Vec::new();
        // Channels whose explicit token_secret names a secret the broker
        // cannot resolve (env, then <data_dir>/gateway.env, then vault).
        // validate() cannot check this - it is pure, with no data dir or
        // broker - so this probe names them one by one instead of leaving
        // them silently absent from the live list. Only the secret NAME
        // is reported, never the value.
        let mut missing_tokens: Vec<(String, String)> = Vec::new();
        if std::env::var("PANTHEON_DISCORD_TOKEN").is_ok_and(|t| !t.trim().is_empty()) {
            live.push("discord (env)".to_string());
        }
        if std::env::var("PANTHEON_TELEGRAM_BOT_TOKEN").is_ok_and(|t| !t.trim().is_empty()) {
            live.push("telegram (env)".to_string());
        }
        if let Ok(cfg) = load_config(ctx) {
            if let Some(gw) = cfg.gateway.as_ref() {
                for (name, ch) in &gw.channels {
                    let platform = ch.resolved_platform(name).unwrap_or_else(|| name.clone());
                    let has_token = match ch.token_secret.as_deref() {
                        Some(secret_name) => {
                            let resolved =
                                pantheon_secrets::gateway_token(&ctx.data_dir, secret_name)
                                    .is_some();
                            // An empty name is validate's shape territory
                            // (flagged there); only a named-but-unresolvable
                            // secret is this probe's finding.
                            if !resolved && !secret_name.trim().is_empty() {
                                missing_tokens.push((name.clone(), secret_name.to_string()));
                            }
                            resolved
                        }
                        // No token_secret: the platform's standard token,
                        // resolved the daemon's way (env > gateway.env).
                        None => {
                            let std_name = match platform.as_str() {
                                "telegram" => pantheon_secrets::TELEGRAM_TOKEN_NAME,
                                "discord" => pantheon_secrets::DISCORD_TOKEN_NAME,
                                _ => "",
                            };
                            !std_name.is_empty()
                                && pantheon_secrets::gateway_token(&ctx.data_dir, std_name)
                                    .is_some()
                        }
                    };
                    if has_token {
                        live.push(format!("{name} ({platform})"));
                    }
                }
            }
        }
        live.sort();
        live.dedup();
        let mut findings = Vec::new();
        for (name, secret_name) in missing_tokens {
            findings.push(Finding::failed(
            format!("runtime.gateway_channels.{name}"),
            Subsystem::Runtime,
            Severity::Warning,
            format!("channel '{name}' token_secret {secret_name:?} does not resolve; the channel never starts"),
            "the secrets broker (process env, then <data_dir>/gateway.env, then vault) holds no value under that name",
            "set the secret under that name, or remove token_secret to use the platform's standard token",
        ));
        }
        if live.is_empty() {
            findings.push(Finding::failed(
            "runtime.gateway_channels",
            Subsystem::Runtime,
            Severity::Warning,
            "no channel tokens set; gateway is idle",
            "the messaging gateway has no live surface",
            "set PANTHEON_DISCORD_TOKEN and/or PANTHEON_TELEGRAM_BOT_TOKEN to run `pantheon gateway`",
        ));
        } else {
            findings.push(Finding::passed(
                "runtime.gateway_channels",
                Subsystem::Runtime,
                format!("{} surface(s) enabled: {}", live.len(), live.join(", ")),
            ));
        }
        findings
    }

    // -- providers --------------------------------------------------------

    fn check_model_configured(ctx: &DoctorCtx) -> Vec<Finding> {
        let cfg = match load_config(ctx) {
            Ok(c) => c,
            Err(_) => {
                return vec![Finding::skipped(
                    "providers.model_configured",
                    Subsystem::Providers,
                    "skipped: config does not parse (see config.file)",
                )];
            }
        };
        match cfg.model.as_ref() {
            Some(m) if !m.provider.trim().is_empty() && !m.model.trim().is_empty() => {
                vec![Finding::passed(
                    "providers.model_configured",
                    Subsystem::Providers,
                    format!("provider {:?}, model {:?}", m.provider, m.model),
                )]
            }
            _ => vec![Finding::failed(
                "providers.model_configured",
                Subsystem::Providers,
                Severity::Error,
                "no [model] section, so no provider or model is configured",
                "no conversation can run",
                "run `pantheon setup`",
            )],
        }
    }

    fn check_model_key(ctx: &DoctorCtx) -> Vec<Finding> {
        let cfg = match load_config(ctx) {
            Ok(c) => c,
            Err(_) => {
                return vec![Finding::skipped(
                    "providers.model_key",
                    Subsystem::Providers,
                    "skipped: config does not parse (see config.file)",
                )];
            }
        };
        match cfg.model.as_ref().and_then(|m| m.api_key_env.clone()) {
            // Only the variable NAME is ever reported. The value is never
            // read into a finding, never printed, never logged.
            Some(env) => match std::env::var(&env) {
                Ok(v) if !v.is_empty() => vec![Finding::passed(
                    "providers.model_key",
                    Subsystem::Providers,
                    format!("{env} is set"),
                )],
                _ => vec![Finding::failed(
                    "providers.model_key",
                    Subsystem::Providers,
                    Severity::Error,
                    format!("{env} is not set in this environment"),
                    "model requests will fail authentication",
                    format!("export {env}=<key> (or run `pantheon setup` to store it)"),
                )],
            },
            None => {
                let provider = cfg
                    .model
                    .as_ref()
                    .map(|m| m.provider.as_str())
                    .unwrap_or("unknown");
                // The old code passed every keyless provider with "no API
                // key required" - including a keyless `openai`, which then
                // failed at request time. Only `local` is keyless by
                // construction. For anything else, mirror the runtime's
                // fallback (`catalog::key_for`): the catalog's key env, else
                // `PANTHEON_KEY_<PROVIDER>`; an unset fallback is a warning,
                // not a silent pass.
                if provider == "local" {
                    return vec![Finding::passed(
                        "providers.model_key",
                        Subsystem::Providers,
                        "no API key required for local".to_string(),
                    )];
                }
                let fallback_env = pantheon_providers::catalog::provider(provider)
                    .map(|p| {
                        if p.key_env.is_empty() {
                            format!("PANTHEON_KEY_{}", provider.to_uppercase())
                        } else {
                            p.key_env.clone()
                        }
                    })
                    .unwrap_or_else(|| format!("PANTHEON_KEY_{}", provider.to_uppercase()));
                if std::env::var(&fallback_env).is_ok_and(|v| !v.trim().is_empty()) {
                    return vec![Finding::passed(
                        "providers.model_key",
                        Subsystem::Providers,
                        format!("{fallback_env} is set (provider default key env)"),
                    )];
                }
                vec![Finding::failed(
                "providers.model_key",
                Subsystem::Providers,
                Severity::Warning,
                format!(
                    "no API key configured for {provider}: neither [model].api_key_env nor {fallback_env} is set"
                ),
                "model requests will fail authentication",
                format!("set {fallback_env}=<key> (or run `pantheon setup` to store it). When a request does fail, the provider layer appends a credential diagnosis (see pantheon_providers::credential_error): HTTP 401 means the key is missing, invalid, or revoked; HTTP 403 means the key is accepted but not permitted for that endpoint or model; a body reading 'error code: 1010' means Cloudflare blocked the request headers at the edge and the key is not at fault, so do not replace it"),
            )]
            }
        }
    }

    fn check_provider_reachable(ctx: &DoctorCtx) -> Vec<Finding> {
        if !ctx.ping_provider {
            return vec![Finding::skipped(
                "providers.reachable",
                Subsystem::Providers,
                "probe not requested (pass --ping to probe the provider endpoint)",
            )];
        }
        // The config-only key-presence check cannot see a dead endpoint, a
        // wrong base URL, or a proxy that swallows the route, so this is a
        // plain TCP connect to the provider's base URL (5s), reported as
        // its own finding rather than hidden inside the model checks.
        let cfg = match load_config(ctx) {
            Ok(c) => c,
            Err(_) => {
                return vec![Finding::skipped(
                    "providers.reachable",
                    Subsystem::Providers,
                    "skipped: config does not parse (see config.file)",
                )];
            }
        };
        match cfg.model.as_ref() {
            Some(m) => match pantheon_providers::catalog::resolve_base_url(&m.provider) {
                Ok(url) => {
                    if tcp_probe(&url, std::time::Duration::from_secs(5)) {
                        vec![Finding::passed(
                            "providers.reachable",
                            Subsystem::Providers,
                            format!("{} reachable at {url}", m.provider),
                        )]
                    } else {
                        vec![Finding::failed(
                        "providers.reachable",
                        Subsystem::Providers,
                        Severity::Error,
                        format!("{} not reachable at {url}", m.provider),
                        "model requests will fail at the network layer",
                        "check the endpoint is up and reachable from this host (proxy, firewall, base URL)",
                    )
                    .with_details(format!("base URL: {url}"))]
                    }
                }
                Err(e) => vec![Finding::failed(
                    "providers.reachable",
                    Subsystem::Providers,
                    Severity::Warning,
                    format!("cannot resolve a base URL for {:?}: {e}", m.provider),
                    "the reachability probe has nothing to dial",
                    "run `pantheon model` to fix the provider entry",
                )],
            },
            None => vec![Finding::skipped(
                "providers.reachable",
                Subsystem::Providers,
                "no [model] section, nothing to probe",
            )],
        }
    }

    // -- memory -----------------------------------------------------------

    fn check_ledger(ctx: &DoctorCtx) -> Vec<Finding> {
        let path = ctx.data_dir.join("ledger.db");
        // Doctor claims to be read-only, but opening a SQLite file creates
        // it when absent: a `GET /api/health` on a fresh install used to
        // materialize ledger.db. An absent ledger is not a failure - it
        // just means no runs yet - so report that without opening.
        if !path.exists() {
            return vec![Finding::not_applicable(
                "memory.ledger",
                Subsystem::Memory,
                "no ledger yet (no runs recorded)",
            )];
        }
        match pantheon_storage::Ledger::open(&path) {
            Ok(ledger) => {
                let seq = ledger.max_seq().unwrap_or(-1);
                let integrity = match ledger.integrity_check() {
                    Ok(rows) if rows.iter().all(|r| r == "ok") => None,
                    Ok(rows) => Some(format!("integrity_check failed: {}", rows.join("; "))),
                    Err(e) => Some(format!("integrity_check error: {}", e.cause)),
                };
                let stuck = ledger.stuck_runs().unwrap_or_default();
                let mut findings = vec![];
                if let Some(msg) = integrity {
                    findings.push(Finding::failed(
                        "memory.ledger.integrity",
                        Subsystem::Memory,
                        Severity::Critical,
                        msg,
                        "ledger is damaged; run history and approvals are unreliable",
                        "restore from a pantheon backup",
                    ));
                } else {
                    findings.push(
                        Finding::passed(
                            "memory.ledger",
                            Subsystem::Memory,
                            format!("ledger opens and integrity_check ok (max seq {seq})"),
                        )
                        .with_details(format!("path: {}", path.display())),
                    );
                }
                if !stuck.is_empty() {
                    findings.push(Finding::failed(
                        "memory.ledger.stuck",
                        Subsystem::Memory,
                        Severity::Warning,
                        format!("{} stuck or awaiting approval runs", stuck.len()),
                        "runs may need operator attention",
                        "check pantheon logs or settle stuck runs via the dashboard",
                    ));
                }
                if let Ok(meta) = std::fs::metadata(&path) {
                    let free = None::<u64>;
                    let _ = free;
                    let _ = meta.len();
                }
                findings
            }
            Err(e) => vec![Finding::failed(
            "memory.ledger",
            Subsystem::Memory,
            Severity::Critical,
            format!("ledger does not open: {}", e.cause),
            "run history, costs, and approvals are unavailable; the runtime cannot persist state",
            "check disk space and permissions on the data dir",
        )
        .with_details(format!("path: {}\nerror: {}", path.display(), e.cause))],
        }
    }

    fn check_memory_store(ctx: &DoctorCtx) -> Vec<Finding> {
        let path = ctx.data_dir.join("memory.db");
        // Same read-only rule as the ledger: opening creates the file.
        if !path.exists() {
            return vec![Finding::not_applicable(
                "memory.store",
                Subsystem::Memory,
                "no memory store yet (no memories recorded)",
            )];
        }
        match pantheon_memory::MemoryStore::open(&path) {
            Ok(_) => vec![
                Finding::passed("memory.store", Subsystem::Memory, "memory store opens")
                    .with_details(format!("path: {}", path.display())),
            ],
            Err(e) => vec![Finding::failed(
                "memory.store",
                Subsystem::Memory,
                Severity::Error,
                format!("memory store does not open: {}", e.cause),
                "long-term memory reads and writes fail",
                "check disk space and permissions on the data dir",
            )
            .with_details(format!("path: {}\nerror: {}", path.display(), e.cause))],
        }
    }

    // -- tools ------------------------------------------------------------

    fn check_plugins(ctx: &DoctorCtx) -> Vec<Finding> {
        let ext_dir = &ctx.ext_dir;
        let entries = match std::fs::read_dir(ext_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A missing directory is an empty plugin set, not a failure.
                return vec![Finding::passed(
                    "tools.plugins",
                    Subsystem::Tools,
                    "no plugins installed",
                )];
            }
            Err(e) => {
                return vec![Finding::failed(
                    "tools.plugins",
                    Subsystem::Tools,
                    Severity::Error,
                    format!("cannot read {}: {e}", ext_dir.display()),
                    "plugin health is unevaluated",
                    "check permissions on the extension dir",
                )];
            }
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        if dirs.is_empty() {
            return vec![Finding::passed(
                "tools.plugins",
                Subsystem::Tools,
                "no plugins installed",
            )];
        }
        dirs.into_iter()
            .map(|d| {
                let rep = pantheon_extensions::doctor::doctor(&d);
                let detail = if rep.findings.is_empty() {
                    format!("{} verified", rep.plugin)
                } else {
                    rep.findings
                        .iter()
                        .map(|x| format!("{}: {}", x.code, x.detail))
                        .collect::<Vec<_>>()
                        .join("; ")
                };
                if !rep.ok {
                    Finding::failed(
                        "tools.plugins",
                        Subsystem::Tools,
                        Severity::Error,
                        format!("plugin {} failed verification", rep.plugin),
                        format!("the {} plugin may not load or may misbehave", rep.plugin),
                        "reinstall the plugin or fix its manifest",
                    )
                    .with_details(detail)
                } else if !rep.unknown_hooks.is_empty() {
                    Finding::failed(
                        "tools.plugins",
                        Subsystem::Tools,
                        Severity::Warning,
                        format!("plugin {} declares unknown hooks", rep.plugin),
                        format!("unknown hooks in {} are ignored", rep.plugin),
                        "update the plugin or remove the unknown hooks",
                    )
                    .with_details(format!(
                        "{detail}; unknown hooks: {}",
                        rep.unknown_hooks.join(", ")
                    ))
                } else {
                    Finding::passed(
                        "tools.plugins",
                        Subsystem::Tools,
                        format!("plugin {} verified", rep.plugin),
                    )
                }
            })
            .collect()
    }

    fn check_computer_use(ctx: &DoctorCtx) -> Vec<Finding> {
        let cfg = match load_config(ctx) {
            Ok(c) => c,
            Err(_) => {
                return vec![Finding::skipped(
                    "tools.computer_use",
                    Subsystem::Tools,
                    "skipped: config does not parse (see config.file)",
                )];
            }
        };
        // Off is a legitimate configuration; on-but-missing is a warning
        // with the install one-liner, never a silent dead toggle.
        let group_on = cfg
            .tools
            .as_ref()
            .and_then(|t| t.computer_use)
            .unwrap_or(true);
        if !group_on {
            return vec![Finding::not_applicable(
                "tools.computer_use",
                Subsystem::Tools,
                "computer-use group off",
            )];
        }
        let section = cfg.computer_use.clone().unwrap_or_default();
        // Resolve exactly like the dashboard does (`cua_driver_spec`): the
        // configured `[computer_use].driver` ("tapp" resolves to `tapp-mcp`,
        // not `cua-driver`) and an explicit `binary` path. The old check
        // always probed `cua-driver` on PATH and used `Path::is_file` on a
        // bare name, so a configured tapp driver always reported missing.
        let spec = pantheon_runtime::computer::cua_driver_spec(
            section.driver.as_deref(),
            section.binary.as_deref(),
        );
        let found = spec.and_then(|s| s.command);
        let driver_label = section.driver.as_deref().unwrap_or("cua-driver");
        match found {
        Some(p) => vec![Finding::passed(
            "tools.computer_use",
            Subsystem::Tools,
            format!("{driver_label} driver at {p}"),
        )],
        None => vec![Finding::failed(
            "tools.computer_use",
            Subsystem::Tools,
            Severity::Warning,
            format!("{driver_label} driver not found"),
            "the ComputerUse tool group is enabled but its driver is missing; computer-use calls fail",
            if driver_label.eq_ignore_ascii_case("tapp") {
                "install tapp-mcp and put it on PATH, or set [computer_use].binary to its path"
            } else {
                "install cua-driver (see the setup docs), or set [computer_use].binary to its path"
            },
        )],
    }
    }

    /// The fix hint for a missing skill dependency. Pure over `pip_ok` so
    /// the pip-gating rule is unit-testable: a pip package with no working
    /// pip must not suggest a pip install that cannot run.
    fn skill_dep_remediation(dep: &crate::skill_deps::SkillDep, pip_ok: bool) -> String {
        if dep.needs_pip && !pip_ok {
            return format!(
            "python3 with pip is missing - install it first, then run: {} (or rerun `pantheon setup`)",
            dep.install_cmd.unwrap_or("see the Skill dependencies step")
        );
        }
        match dep.install_cmd {
            Some(cmd) => format!("run: {cmd} (or rerun `pantheon setup`)"),
            None => format!("{} (then rerun `pantheon setup`)", dep.install_hint),
        }
    }

    fn check_skill_deps(ctx: &DoctorCtx) -> Vec<Finding> {
        let detect = |cmd: &str| detect_binary(cmd, DETECT_TIMEOUT);
        let pip_ok = crate::skill_deps::pip_ready(&detect);
        let cfg = load_config(ctx).ok();
        let skipped: &[String] = cfg
            .as_ref()
            .and_then(|c| c.skill_deps.as_ref())
            .map(|s| s.skipped.as_slice())
            .unwrap_or(&[]);
        let mut out = Vec::new();
        // A missing dep is a warning, never an error: a skill without its
        // package degrades or falls back, but preflight must not stop the
        // run. A `[skill_deps].skipped` entry that is present now is
        // reported as healed, so the record visibly converges.
        for dep in crate::skill_deps::skill_deps() {
            if detect(dep.detect_cmd) {
                if skipped.iter().any(|s| s == dep.id) {
                    out.push(
                        Finding::passed(
                            "tools.skill_deps",
                            Subsystem::Tools,
                            format!("{} installed since setup (was skipped)", dep.name),
                        )
                        .with_remediation(
                            "remove it from [skill_deps].skipped to clear the record",
                        ),
                    );
                } else {
                    out.push(Finding::passed(
                        "tools.skill_deps",
                        Subsystem::Tools,
                        format!("{} present", dep.name),
                    ));
                }
                continue;
            }
            out.push(
                Finding::failed(
                    "tools.skill_deps",
                    Subsystem::Tools,
                    Severity::Warning,
                    format!("{} not found - needed by {}", dep.name, dep.needed_by),
                    format!("the {} skill degrades without {}", dep.needed_by, dep.name),
                    skill_dep_remediation(dep, pip_ok),
                )
                .with_details(format!("detect: {}", dep.detect_cmd)),
            );
        }
        // Stale record hygiene: a skipped id the registry no longer knows
        // (renamed or removed) is named, not silently kept.
        for id in skipped {
            if crate::skill_deps::find_dep(id).is_none() {
                out.push(Finding::failed(
                    "tools.skill_deps",
                    Subsystem::Tools,
                    Severity::Warning,
                    format!("unknown skill dependency {id:?} in [skill_deps].skipped"),
                    "the skip record references a dependency that no longer exists",
                    "remove it from the config or rerun `pantheon setup`",
                ));
            }
        }
        out
    }

    /// Extra skill roots from the environment, mirroring the TUI's skill
    /// discovery inputs.
    fn extra_skill_roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Ok(d) = std::env::var("PANTHEON_REPLAY_SKILL_DIR") {
            let d = d.trim();
            if !d.is_empty() {
                roots.push(PathBuf::from(d));
            }
        }
        if let Ok(d) = std::env::var("PANTHEON_SKILLS_DIR") {
            for entry in d.split(':').filter(|s| !s.is_empty()) {
                roots.push(PathBuf::from(entry));
            }
        }
        roots
    }

    fn check_skills(ctx: &DoctorCtx) -> Vec<Finding> {
        // A broken SKILL.md is dropped silently by discovery, so count what
        // parsed and say so. Surface the roots, not every skill body (that
        // is what `skills list` is for). Doctor uses the no-seed scan:
        // the regular scan materializes bundled skills into the data dir,
        // which is a write from a check that claims to be read-only (and
        // from `GET /api/health`).
        let project_root = std::env::current_dir().unwrap_or_else(|_| ctx.data_dir.clone());
        let scan = pantheon_exec::skills::scan_skills_ext_noseed(
            &ctx.data_dir,
            &project_root,
            &extra_skill_roots(),
        );
        if scan.rejected.is_empty() {
            return vec![Finding::passed(
                "tools.skills",
                Subsystem::Tools,
                format!("{} skill(s) parsed", scan.loaded.len()),
            )];
        }
        let detail = scan
            .rejected
            .iter()
            .map(|r| format!("{}: {}", r.path.display(), r.reason))
            .collect::<Vec<_>>()
            .join("\n");
        vec![Finding::failed(
            "tools.skills",
            Subsystem::Tools,
            Severity::Warning,
            format!(
                "{} broken skill(s) dropped by discovery ({} parsed)",
                scan.rejected.len(),
                scan.loaded.len()
            ),
            "broken skills are silently unavailable to the agent",
            "run `pantheon skills doctor` for the per-skill breakdown",
        )
        .with_details(detail)]
    }

    // -- security --------------------------------------------------------

    fn check_secrets_allowlist(ctx: &DoctorCtx) -> Vec<Finding> {
        let cfg = match load_config(ctx) {
            Ok(c) => c,
            Err(_) => {
                return vec![Finding::skipped(
                    "security.secrets_allowlist",
                    Subsystem::Security,
                    "skipped: config does not parse (see config.file)",
                )];
            }
        };
        let section = match cfg.secrets.as_ref() {
            Some(s) => s,
            None => {
                return vec![Finding::passed(
                "security.secrets_allowlist",
                Subsystem::Security,
                "no [secrets] section: boundary fails closed (plugins get PATH plus Pantheon-set vars only)",
            )];
            }
        };
        // A bare "*" is the explicit opt-out: every host env var becomes
        // readable through the `env:` form, including API keys. That is a
        // deliberate choice, but it deserves a visible warning.
        if section.env_allowlist.iter().any(|e| e == "*") {
            return vec![Finding::failed(
            "security.secrets_allowlist",
            Subsystem::Security,
            Severity::Warning,
            "[secrets] env_allowlist contains \"*\": every host env var is readable via `env:`",
            "any skill or prompt using the `env:` secret form can read arbitrary host variables, including API keys",
            "replace \"*\" with the exact variable names (or PREFIX_* wildcards) the agent needs",
        )];
        }
        let n = section.env_allowlist.len() + section.plugin_env_allowlist.len();
        vec![Finding::passed(
            "security.secrets_allowlist",
            Subsystem::Security,
            if n == 0 {
                "secrets boundary fails closed (empty allowlists)".to_string()
            } else {
                format!("secrets boundary scoped ({n} allowlist entr(ies), no wildcard)")
            },
        )]
    }

    fn check_dotenv_permissions(ctx: &DoctorCtx) -> Vec<Finding> {
        let path = ctx.data_dir.join(".env");
        if !path.exists() {
            return vec![Finding::not_applicable(
                "security.dotenv_permissions",
                Subsystem::Security,
                "no .env file in the data dir",
            )];
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(&path) {
                Ok(md) => {
                    let mode = md.permissions().mode();
                    // The dotenv holds API keys. Group/other read or write
                    // bits mean another user on the box can read them.
                    if mode & 0o077 != 0 {
                        return vec![Finding::failed(
                            "security.dotenv_permissions",
                            Subsystem::Security,
                            Severity::Warning,
                            format!(
                                ".env is readable beyond the owner (mode {:o})",
                                mode & 0o777
                            ),
                            "API keys in the dotenv may be readable by other users on this machine",
                            format!("chmod 600 {}", path.display()),
                        )
                        .with_details(format!("path: {}", path.display()))];
                    }
                    vec![Finding::passed(
                        "security.dotenv_permissions",
                        Subsystem::Security,
                        format!(".env owner-only (mode {:o})", mode & 0o777),
                    )]
                }
                Err(e) => vec![Finding::failed(
                    "security.dotenv_permissions",
                    Subsystem::Security,
                    Severity::Warning,
                    format!("cannot stat .env: {e}"),
                    "dotenv permissions are unevaluated",
                    "check permissions on the data dir",
                )],
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            vec![Finding::not_applicable(
                "security.dotenv_permissions",
                Subsystem::Security,
                "permission check is unix-only",
            )]
        }
    }

    fn check_exec_sandbox(_ctx: &DoctorCtx) -> Vec<Finding> {
        // The shell tool's default SandboxLevel::High runs through a
        // Container boundary (bwrap). Without bwrap the runner fails closed
        // (SANDBOX_UNAVAILABLE), so a missing binary is a warning with the
        // fix, never silent.
        if detect_binary("command -v bwrap", DETECT_TIMEOUT) {
            vec![Finding::passed(
                "security.exec_sandbox",
                Subsystem::Security,
                "bwrap present (container sandbox for High-level exec)",
            )]
        } else {
            vec![Finding::failed(
            "security.exec_sandbox",
            Subsystem::Security,
            Severity::Warning,
            "bwrap not found on PATH - the shell tool's High sandbox fails closed without it",
            "High-sandbox exec calls are refused (SANDBOX_UNAVAILABLE) instead of contained",
            "install bubblewrap (e.g. `apt install bubblewrap`), or set PANTHEON_SANDBOX_FALLBACK=allow to opt into the direct-spawn fallback",
        )]
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::finding::CheckStatus;
        use crate::registry::DoctorCtx;
        use std::collections::HashMap;
        use std::path::Path;

        fn ctx_in(dir: &Path) -> DoctorCtx {
            DoctorCtx::new(dir.to_path_buf(), dir.join("extensions"))
        }

        fn write_config(dir: &Path, body: &str) {
            std::fs::write(dir.join("config.toml"), body).expect("seed config");
        }

        fn temp_dir(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-doctor-check-test-{}-{}",
                std::process::id(),
                name
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            dir
        }

        #[test]
        fn config_file_critical_when_unparseable() {
            let dir = temp_dir("bad-config");
            write_config(&dir, "[[[not toml");
            let f = check_config_file(&ctx_in(&dir));
            assert_eq!(f.len(), 1);
            assert_eq!(f[0].status, CheckStatus::Failed);
            assert_eq!(f[0].severity, Severity::Critical);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn schema_skipped_when_config_unparseable() {
            // One root cause, one critical finding: schema must not
            // double-count the parse failure.
            let dir = temp_dir("bad-config-2");
            write_config(&dir, "[[[not toml");
            let f = check_config_schema(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Skipped);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn deprecated_sections_warn() {
            let dir = temp_dir("legacy");
            write_config(&dir, "[reflect]\nenabled = true\n");
            let f = check_deprecated_sections(&ctx_in(&dir));
            assert_eq!(f.len(), 1);
            assert_eq!(f[0].severity, Severity::Warning);
            assert!(f[0].summary.contains("[reflect]"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn secrets_wildcard_warns() {
            let dir = temp_dir("wild");
            write_config(&dir, "[secrets]\nenv_allowlist = [\"*\"]\n");
            let f = check_secrets_allowlist(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Failed);
            assert_eq!(f[0].severity, Severity::Warning);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn secrets_absent_is_fail_closed_pass() {
            let dir = temp_dir("no-secrets");
            write_config(&dir, "[model]\nprovider = \"x\"\nmodel = \"y\"\n");
            let f = check_secrets_allowlist(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Passed);
            assert!(f[0].summary.contains("fails closed"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn dotenv_world_readable_warns() {
            let dir = temp_dir("dotenv");
            write_config(&dir, "");
            let env = dir.join(".env");
            std::fs::write(&env, "K=v\n").expect("seed .env");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o644))
                    .expect("chmod");
                let f = check_dotenv_permissions(&ctx_in(&dir));
                assert_eq!(f[0].status, CheckStatus::Failed);
                assert_eq!(f[0].severity, Severity::Warning);
                // The finding must name the file, never its contents.
                assert!(!f[0].summary.contains("K=v"));
                assert!(!f[0].details.as_deref().unwrap_or("").contains("K=v"));
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn dotenv_absent_is_not_applicable() {
            let dir = temp_dir("no-dotenv");
            let f = check_dotenv_permissions(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::NotApplicable);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn computer_use_off_is_not_applicable() {
            let dir = temp_dir("cua-off");
            write_config(&dir, "[tools]\ncomputer_use = false\n");
            let f = check_computer_use(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::NotApplicable);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn reachable_skipped_without_ping_flag() {
            // A missing optional probe is skipped, never a failure.
            let dir = temp_dir("no-ping");
            let f = check_provider_reachable(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Skipped);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn plugins_missing_dir_is_pass_not_fail() {
            let dir = temp_dir("no-ext");
            let f = check_plugins(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Passed);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn agent_tables_listed_as_info() {
            let dir = temp_dir("agents");
            write_config(&dir, "[agents.zeus]\n");
            let f = check_agents(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Passed);
            assert_eq!(f[0].severity, Severity::Info);
            assert!(f[0].summary.contains("zeus"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn all_check_ids_unique_and_well_formed() {
            let mut seen = HashMap::new();
            for def in all_checks() {
                assert!(def.id.contains('.'), "check id must be dotted: {}", def.id);
                assert!(
                    seen.insert(def.id, def.subsystem).is_none(),
                    "duplicate check id: {}",
                    def.id
                );
            }
            assert_eq!(seen.len(), 18);
        }

        // -- previously untested checks -------------------------------------
        // These eight registered checks had no coverage, so a regression in
        // their verdict logic would ship silently. Each test pins the
        // happy/failure distinction that the reported finding depends on.

        #[test]
        fn nightly_state_is_skipped_when_config_does_not_parse() {
            let dir = temp_dir("nightly-bad-config");
            write_config(&dir, "[[[not toml");
            let f = check_nightly_state(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Skipped);
        }

        #[test]
        fn nightly_state_warns_when_disabled_by_default() {
            let dir = temp_dir("nightly-off");
            write_config(&dir, "[model]\nprovider = \"x\"\nmodel = \"y\"\n");
            let f = check_nightly_state(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Failed);
            assert_eq!(
                f[0].severity,
                Severity::Warning,
                "a disabled nightly pass is a warning, not an error: it is a valid configuration"
            );
            assert!(
                f[0].remediation.contains("enable it"),
                "{}",
                f[0].remediation
            );
        }

        #[test]
        fn nightly_state_passes_when_enabled() {
            let dir = temp_dir("nightly-on");
            write_config(
                &dir,
                "[model]\nprovider = \"x\"\nmodel = \"y\"\n[nightly]\nenabled = true\n",
            );
            let f = check_nightly_state(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Passed);
            assert!(f[0].summary.contains("enabled"), "{}", f[0].summary);
        }

        #[test]
        fn gateway_channels_warns_when_no_tokens_are_set() {
            // Guard against a developer machine or CI that exports real tokens,
            // which would turn this assertion into a false pass.
            if std::env::var("PANTHEON_DISCORD_TOKEN").is_ok_and(|t| !t.trim().is_empty())
                || std::env::var("PANTHEON_TELEGRAM_BOT_TOKEN").is_ok_and(|t| !t.trim().is_empty())
            {
                return;
            }
            let dir = temp_dir("gateway-idle");
            write_config(&dir, "[model]\nprovider = \"x\"\nmodel = \"y\"\n");
            let f = check_gateway_channels(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Failed);
            assert_eq!(f[0].severity, Severity::Warning);
            assert!(f[0].summary.contains("idle"), "{}", f[0].summary);
        }

        #[test]
        fn model_configured_fails_without_a_model_section() {
            let dir = temp_dir("no-model");
            write_config(&dir, "[budget]\nmax_tokens = 1000\n");
            let f = check_model_configured(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Failed);
            assert_eq!(f[0].severity, Severity::Error);
            assert_eq!(f[0].id, "providers.model_configured");
        }

        #[test]
        fn model_configured_passes_when_provider_and_model_are_present() {
            let dir = temp_dir("model-ok");
            write_config(
                &dir,
                "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"\n",
            );
            let f = check_model_configured(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Passed);
        }

        #[test]
        fn model_key_fails_when_the_named_env_var_is_absent() {
            let dir = temp_dir("key-missing");
            write_config(
            &dir,
            "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"\napi_key_env = \"PANTHEON_TEST_SURELY_ABSENT_KEY\"\n",
        );
            let f = check_model_key(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Failed);
            assert_eq!(f[0].severity, Severity::Error);
            assert!(
                f[0].summary.contains("PANTHEON_TEST_SURELY_ABSENT_KEY"),
                "the finding must name the variable: {}",
                f[0].summary
            );
            // A finding must never carry a credential value, only its name.
            assert!(!f[0].summary.to_lowercase().contains("sk-"));
        }

        #[test]
        fn ledger_absent_is_not_a_failure() {
            // Doctor must stay read-only: a missing ledger means "no runs yet",
            // and probing for it must not create the file.
            let dir = temp_dir("no-ledger");
            let f = check_ledger(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::NotApplicable);
            assert!(
                !dir.join("ledger.db").exists(),
                "the check must not materialize ledger.db"
            );
        }

        #[test]
        fn ledger_passes_on_a_freshly_opened_database() {
            let dir = temp_dir("good-ledger");
            // Creating through the real API keeps this honest about the schema.
            pantheon_storage::Ledger::open(&dir.join("ledger.db")).expect("ledger opens");
            let f = check_ledger(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Passed, "{:?}", f[0]);
            assert!(
                f[0].summary.contains("integrity_check ok"),
                "{}",
                f[0].summary
            );
        }

        #[test]
        fn memory_store_absent_is_not_a_failure() {
            let dir = temp_dir("no-memory");
            let f = check_memory_store(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::NotApplicable);
            assert!(
                !dir.join("memory.db").exists(),
                "the check must not materialize memory.db"
            );
        }

        #[test]
        fn memory_store_passes_when_the_store_opens() {
            let dir = temp_dir("good-memory");
            pantheon_memory::MemoryStore::open(&dir.join("memory.db")).expect("memory opens");
            let f = check_memory_store(&ctx_in(&dir));
            assert_eq!(f[0].status, CheckStatus::Passed);
        }

        /// `check_skills` scans the project root (not the data dir), so what it
        /// finds depends on the checkout. The invariant worth pinning is the
        /// verdict shape: exactly one finding under a stable id, and a broken
        /// skill is a Warning rather than an Error - a malformed third-party
        /// SKILL.md must not make the installation read as unhealthy.
        #[test]
        fn skills_check_reports_a_stable_id_and_never_escalates_to_error() {
            let dir = temp_dir("skills");
            write_config(&dir, "[model]\nprovider = \"x\"\nmodel = \"y\"\n");
            let f = check_skills(&ctx_in(&dir));
            assert_eq!(f.len(), 1, "the check answers once: {f:?}");
            assert_eq!(f[0].id, "tools.skills");
            assert!(
                matches!(f[0].status, CheckStatus::Passed | CheckStatus::Failed),
                "unexpected status: {:?}",
                f[0].status
            );
            if f[0].status == CheckStatus::Failed {
                assert_eq!(
                    f[0].severity,
                    Severity::Warning,
                    "a broken skill is a warning, not an error"
                );
                assert!(
                    !f[0].remediation.is_empty(),
                    "an actionable finding must say what to do"
                );
            }
        }

        /// The sandbox check reads the host, not the data dir, so the test
        /// asserts the invariant that matters: it always returns exactly one
        /// finding under the stable id, whatever the host has installed.
        #[test]
        fn exec_sandbox_reports_exactly_one_finding_with_a_stable_id() {
            let dir = temp_dir("sandbox");
            let f = check_exec_sandbox(&ctx_in(&dir));
            assert_eq!(f.len(), 1);
            assert_eq!(f[0].id, "security.exec_sandbox");
            assert!(matches!(
                f[0].status,
                CheckStatus::Passed
                    | CheckStatus::Failed
                    | CheckStatus::Skipped
                    | CheckStatus::NotApplicable
            ));
        }
    }
}

pub mod finding {
    //! The findings vocabulary: every diagnostic check speaks this, and every
    //! renderer and the `/api/health` contract read from it. Checks never
    //! print; they return [`Finding`]s.

    use serde::{Deserialize, Serialize};

    /// The six subsystems a health report covers, in canonical display order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Subsystem {
        Runtime,
        Providers,
        Memory,
        Tools,
        Security,
        Config,
    }

    impl Subsystem {
        /// Canonical order for grouped output.
        pub const ALL: [Subsystem; 6] = [
            Subsystem::Runtime,
            Subsystem::Providers,
            Subsystem::Memory,
            Subsystem::Tools,
            Subsystem::Security,
            Subsystem::Config,
        ];

        pub fn id(self) -> &'static str {
            match self {
                Subsystem::Runtime => "runtime",
                Subsystem::Providers => "providers",
                Subsystem::Memory => "memory",
                Subsystem::Tools => "tools",
                Subsystem::Security => "security",
                Subsystem::Config => "config",
            }
        }

        pub fn title(self) -> &'static str {
            match self {
                Subsystem::Runtime => "Runtime & execution",
                Subsystem::Providers => "Models & providers",
                Subsystem::Memory => "Memory & storage",
                Subsystem::Tools => "Tools & integrations",
                Subsystem::Security => "Security",
                Subsystem::Config => "Configuration",
            }
        }

        pub fn subtitle(self) -> &'static str {
            match self {
                Subsystem::Runtime => "Engine, lifecycle, recovery",
                Subsystem::Providers => "Authentication, connectivity, routing",
                Subsystem::Memory => "SQLite, migrations, retrieval",
                Subsystem::Tools => "Availability, dependencies, permissions",
                Subsystem::Security => "Policy, permissions, dependencies",
                Subsystem::Config => "Schema, paths, deprecated settings",
            }
        }

        /// Parse a `--check <subsystem>` argument. Accepts the canonical id
        /// and the few obvious aliases; anything else is a usage error.
        pub fn parse(s: &str) -> Option<Subsystem> {
            match s.to_ascii_lowercase().as_str() {
                "runtime" => Some(Subsystem::Runtime),
                "providers" | "provider" | "models" | "model" => Some(Subsystem::Providers),
                "memory" | "storage" => Some(Subsystem::Memory),
                "tools" | "integrations" => Some(Subsystem::Tools),
                "security" => Some(Subsystem::Security),
                "config" | "configuration" => Some(Subsystem::Config),
                _ => None,
            }
        }
    }

    /// How bad a finding is. The warning policy is explicit and identical
    /// across every renderer:
    ///
    /// - `Critical`: a runtime invariant failed (config unparseable, ledger
    ///   will not open). The install cannot work until this is fixed.
    /// - `Error`: something is broken (missing API key, broken plugin). The
    ///   install runs, but the affected part does not.
    /// - `Warning`: actionable but non-blocking (missing optional dependency,
    ///   idle gateway, overly broad secrets allowlist). Always shown in the
    ///   concise output and always forces a non-zero exit - a warning is a
    ///   promise that something needs attention, not a clean bill of health.
    /// - `Info`: context, not action (nightly enabled, agent tables listed).
    ///
    /// Ordering is least-to-most severe so "most severe first" is a reverse
    /// sort.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Severity {
        Info,
        Warning,
        Error,
        Critical,
    }

    impl Severity {
        pub fn label(self) -> &'static str {
            match self {
                Severity::Info => "info",
                Severity::Warning => "warning",
                Severity::Error => "error",
                Severity::Critical => "critical",
            }
        }
    }

    /// What a check concluded about its subject.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CheckStatus {
        /// The check ran and the expectation held.
        Passed,
        /// The check ran and the expectation did not hold. [`Severity`]
        /// says how much that matters.
        Failed,
        /// The check did not run because a precondition was missing or it
        /// was not requested (opt-in network probe without `--ping`, an
        /// optional integration that is simply absent). A missing optional
        /// integration is `Skipped`, never a failure.
        Skipped,
        /// The check's subject does not exist in this install (driver check
        /// with the computer-use group off). Not a gap, just inapplicable.
        NotApplicable,
    }

    impl CheckStatus {
        pub fn label(self) -> &'static str {
            match self {
                CheckStatus::Passed => "passed",
                CheckStatus::Failed => "failed",
                CheckStatus::Skipped => "skipped",
                CheckStatus::NotApplicable => "not_applicable",
            }
        }
    }

    /// One structured diagnostic result. Built by checks, read by renderers.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Finding {
        /// Stable identifier, e.g. `memory.ledger.open`.
        pub id: String,
        pub subsystem: Subsystem,
        pub severity: Severity,
        pub status: CheckStatus,
        /// One line: what was checked and the verdict.
        pub summary: String,
        /// Verbose evidence (paths, counts, raw values). Shown in verbose
        /// output and the JSON report, never in concise output. Must never
        /// contain credential values, tokens, or secret env contents.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub details: Option<String>,
        /// What functionality is affected when this finding is failed.
        pub impact: String,
        /// The suggested next action.
        pub remediation: String,
        /// Wall time the check took, in nanoseconds. Stamped by the
        /// orchestrator, not the check.
        pub duration_ns: u64,
        /// Reserved for the phase-4 fix planner. Unused this pass.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub fix_id: Option<String>,
        /// True when the check itself errored or panicked instead of
        /// producing a real verdict. Forces overall health to `unknown`: a
        /// diagnosis that could not complete must never read as healthy.
        #[serde(default)]
        pub incomplete: bool,
    }

    impl Finding {
        pub fn passed(
            id: impl Into<String>,
            subsystem: Subsystem,
            summary: impl Into<String>,
        ) -> Self {
            Finding {
                id: id.into(),
                subsystem,
                severity: Severity::Info,
                status: CheckStatus::Passed,
                summary: summary.into(),
                details: None,
                impact: String::new(),
                remediation: String::new(),
                duration_ns: 0,
                fix_id: None,
                incomplete: false,
            }
        }

        pub fn failed(
            id: impl Into<String>,
            subsystem: Subsystem,
            severity: Severity,
            summary: impl Into<String>,
            impact: impl Into<String>,
            remediation: impl Into<String>,
        ) -> Self {
            Finding {
                id: id.into(),
                subsystem,
                severity,
                status: CheckStatus::Failed,
                summary: summary.into(),
                details: None,
                impact: impact.into(),
                remediation: remediation.into(),
                duration_ns: 0,
                fix_id: None,
                incomplete: false,
            }
        }

        pub fn skipped(
            id: impl Into<String>,
            subsystem: Subsystem,
            summary: impl Into<String>,
        ) -> Self {
            Finding {
                id: id.into(),
                subsystem,
                severity: Severity::Info,
                status: CheckStatus::Skipped,
                summary: summary.into(),
                details: None,
                impact: String::new(),
                remediation: String::new(),
                duration_ns: 0,
                fix_id: None,
                incomplete: false,
            }
        }

        pub fn not_applicable(
            id: impl Into<String>,
            subsystem: Subsystem,
            summary: impl Into<String>,
        ) -> Self {
            Finding {
                id: id.into(),
                subsystem,
                severity: Severity::Info,
                status: CheckStatus::NotApplicable,
                summary: summary.into(),
                details: None,
                impact: String::new(),
                remediation: String::new(),
                duration_ns: 0,
                fix_id: None,
                incomplete: false,
            }
        }

        pub fn with_details(mut self, details: impl Into<String>) -> Self {
            self.details = Some(details.into());
            self
        }

        pub fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
            self.remediation = remediation.into();
            self
        }

        /// A finding is actionable when concise output must show it: any
        /// failed check, plus any warning-severity finding.
        pub fn is_actionable(&self) -> bool {
            self.status == CheckStatus::Failed || self.severity == Severity::Warning
        }
    }

    /// Overall install health, aggregated from findings.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum OverallHealth {
        /// Required checks passed, no actionable warnings.
        Healthy,
        /// Operable but impaired: warnings or errors, no critical failure.
        Degraded,
        /// A critical runtime invariant failed.
        Unhealthy,
        /// Diagnostics could not complete reliably (a check errored or
        /// panicked). Never reported as healthy.
        Unknown,
    }

    impl OverallHealth {
        pub fn label(self) -> &'static str {
            match self {
                OverallHealth::Healthy => "healthy",
                OverallHealth::Degraded => "degraded",
                OverallHealth::Unhealthy => "unhealthy",
                OverallHealth::Unknown => "unknown",
            }
        }
    }

    /// Aggregate findings into overall health. The order of the rules is
    /// load-bearing: a critical failure beats an incomplete diagnosis, an
    /// incomplete diagnosis beats a run where nothing could be evaluated,
    /// and any failure beats a clean run.
    pub fn aggregate(findings: &[Finding]) -> OverallHealth {
        if findings
            .iter()
            .any(|f| f.status == CheckStatus::Failed && f.severity == Severity::Critical)
        {
            return OverallHealth::Unhealthy;
        }
        if findings.iter().any(|f| f.incomplete) {
            return OverallHealth::Unknown;
        }
        // Nothing produced a verdict and at least one check could not run:
        // the diagnosis is unreliable, never healthy. (A run where every
        // check is not-applicable is a completed diagnosis of nothing, so
        // it stays healthy.)
        let decisive = findings
            .iter()
            .any(|f| matches!(f.status, CheckStatus::Passed | CheckStatus::Failed));
        if !decisive && findings.iter().any(|f| f.status == CheckStatus::Skipped) {
            return OverallHealth::Unknown;
        }
        if findings.iter().any(|f| f.status == CheckStatus::Failed) {
            return OverallHealth::Degraded;
        }
        OverallHealth::Healthy
    }

    /// The complete result of one diagnostics run.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DiagnosticReport {
        pub schema: String,
        pub generated_at: String,
        pub overall: OverallHealth,
        /// One-line human summary, e.g. "degraded: 1 error, 2 warnings".
        pub summary: String,
        pub findings: Vec<Finding>,
        /// Wall time for the whole run, in nanoseconds.
        pub duration_ns: u64,
    }

    /// Build the one-line summary from finding counts. Shared by the CLI
    /// renderers and the `/api/health` contract so the wording cannot drift.
    pub fn summarize(findings: &[Finding]) -> String {
        let (mut critical, mut errors, mut warnings) = (0, 0, 0);
        let mut passed = 0;
        for f in findings {
            if f.status == CheckStatus::Failed {
                match f.severity {
                    Severity::Critical => critical += 1,
                    Severity::Error => errors += 1,
                    Severity::Warning => warnings += 1,
                    Severity::Info => warnings += 1,
                }
            } else if f.status == CheckStatus::Passed {
                passed += 1;
            }
        }
        let overall = aggregate(findings);
        if overall == OverallHealth::Healthy {
            return format!("healthy: {passed} passed, no actionable findings");
        }
        let mut parts = Vec::new();
        if critical > 0 {
            parts.push(format!("{critical} critical"));
        }
        if errors > 0 {
            parts.push(format!(
                "{errors} error{}",
                if errors == 1 { "" } else { "s" }
            ));
        }
        if warnings > 0 {
            parts.push(format!(
                "{warnings} warning{}",
                if warnings == 1 { "" } else { "s" }
            ));
        }
        if parts.is_empty() {
            let skipped = findings
                .iter()
                .filter(|f| f.status == CheckStatus::Skipped)
                .count();
            if skipped > 0 {
                parts.push(format!("no check produced a verdict ({skipped} skipped)"));
            } else {
                parts.push("diagnostics incomplete".to_string());
            }
        }
        format!("{}: {}", overall.label(), parts.join(", "))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn failed(id: &str, sev: Severity) -> Finding {
            Finding::failed(id, Subsystem::Runtime, sev, "broken", "impact", "fix it")
        }

        #[test]
        fn empty_findings_aggregate_healthy() {
            assert_eq!(aggregate(&[]), OverallHealth::Healthy);
        }

        #[test]
        fn critical_failure_beats_everything() {
            let findings = vec![
                failed("a", Severity::Warning),
                failed("b", Severity::Critical),
                Finding {
                    incomplete: true,
                    ..failed("c", Severity::Error)
                },
            ];
            assert_eq!(aggregate(&findings), OverallHealth::Unhealthy);
        }

        #[test]
        fn incomplete_check_forces_unknown_not_healthy() {
            // A panicking check must never silently produce a healthy result.
            let findings = vec![Finding {
                incomplete: true,
                ..failed("c", Severity::Error)
            }];
            assert_eq!(aggregate(&findings), OverallHealth::Unknown);
        }

        #[test]
        fn incomplete_loses_to_critical_wins_over_degraded() {
            let findings = vec![
                failed("a", Severity::Warning),
                Finding {
                    incomplete: true,
                    ..failed("c", Severity::Error)
                },
            ];
            assert_eq!(aggregate(&findings), OverallHealth::Unknown);
        }

        #[test]
        fn error_or_warning_is_degraded() {
            assert_eq!(
                aggregate(&[failed("a", Severity::Error)]),
                OverallHealth::Degraded
            );
            assert_eq!(
                aggregate(&[failed("a", Severity::Warning)]),
                OverallHealth::Degraded
            );
        }

        #[test]
        fn passed_and_skipped_are_healthy() {
            let findings = vec![
                Finding::passed("a", Subsystem::Config, "ok"),
                Finding::skipped("b", Subsystem::Tools, "not requested"),
                Finding::not_applicable("c", Subsystem::Security, "n/a"),
            ];
            assert_eq!(aggregate(&findings), OverallHealth::Healthy);
        }

        #[test]
        fn all_skipped_is_unknown_never_healthy() {
            // A filtered run where nothing could be evaluated must not read
            // as healthy.
            let findings = vec![
                Finding::skipped("a", Subsystem::Providers, "no config"),
                Finding::skipped("b", Subsystem::Providers, "no config"),
            ];
            assert_eq!(aggregate(&findings), OverallHealth::Unknown);
            let s = summarize(&findings);
            assert!(s.starts_with("unknown:"), "{s}");
            assert!(s.contains("2 skipped"), "{s}");
        }

        #[test]
        fn all_not_applicable_stays_healthy() {
            // Not-applicable is a completed diagnosis of nothing: nothing is
            // broken, so this is healthy, not unknown.
            let findings = vec![Finding::not_applicable("a", Subsystem::Tools, "group off")];
            assert_eq!(aggregate(&findings), OverallHealth::Healthy);
        }

        #[test]
        fn warning_severity_failed_counts_as_warning_in_summary() {
            let s = summarize(&[failed("a", Severity::Warning)]);
            assert!(s.starts_with("degraded:"), "{s}");
            assert!(s.contains("1 warning"), "{s}");
        }

        #[test]
        fn healthy_summary_counts_passes() {
            let s = summarize(&[Finding::passed("a", Subsystem::Config, "ok")]);
            assert_eq!(s, "healthy: 1 passed, no actionable findings");
        }

        #[test]
        fn subsystem_parse_accepts_ids_and_aliases() {
            assert_eq!(Subsystem::parse("runtime"), Some(Subsystem::Runtime));
            assert_eq!(Subsystem::parse("providers"), Some(Subsystem::Providers));
            assert_eq!(Subsystem::parse("models"), Some(Subsystem::Providers));
            assert_eq!(Subsystem::parse("storage"), Some(Subsystem::Memory));
            assert_eq!(Subsystem::parse("CONFIG"), Some(Subsystem::Config));
            assert_eq!(Subsystem::parse("bogus"), None);
        }

        #[test]
        fn severity_orders_least_to_most() {
            assert!(Severity::Info < Severity::Warning);
            assert!(Severity::Warning < Severity::Error);
            assert!(Severity::Error < Severity::Critical);
        }
    }
}

pub mod health {
    //! The `pantheon.health/v1` contract served by `GET /api/health`.
    //!
    //! Built from a [`DiagnosticReport`] by the same pipeline the CLI
    //! uses - no duplicated check logic. The mobile Health page renders
    //! this document directly.

    use crate::finding::{CheckStatus, DiagnosticReport, Finding, Severity, Subsystem};
    use serde::Serialize;

    /// Contract version. Bumped only on breaking shape changes.
    pub const SCHEMA: &str = "pantheon.health/v1";

    /// Per-subsystem rollup for the health contract.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SubsystemStatus {
        Healthy,
        Review,
        Down,
        Unknown,
    }

    impl SubsystemStatus {
        pub fn label(self) -> &'static str {
            match self {
                SubsystemStatus::Healthy => "Healthy",
                SubsystemStatus::Review => "Review",
                SubsystemStatus::Down => "Down",
                SubsystemStatus::Unknown => "Unknown",
            }
        }
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct CoreRuntime {
        pub status: SubsystemStatus,
        pub label: String,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct HealthCheck {
        pub id: String,
        pub status: CheckStatus,
        pub severity: Severity,
        pub summary: String,
        pub details: String,
        pub impact: String,
        pub remediation: String,
        pub duration_ns: u64,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct HealthSubsystem {
        pub id: String,
        pub title: String,
        pub subtitle: String,
        pub status: SubsystemStatus,
        pub checks: Vec<HealthCheck>,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct HealthIssue {
        pub id: String,
        pub subsystem: Subsystem,
        /// Contract severity: error|warning|info. A critical finding maps
        /// to "error" here (its criticality is preserved in `checks`).
        pub severity: String,
        pub title: String,
        pub detail: String,
        pub hint: String,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct HealthResponse {
        pub schema: String,
        pub generated_at: String,
        pub overall: crate::finding::OverallHealth,
        pub summary: String,
        pub core_runtime: CoreRuntime,
        pub issue_count: usize,
        pub subsystems: Vec<HealthSubsystem>,
        pub issues: Vec<HealthIssue>,
    }

    /// Roll one subsystem's findings up to a status: healthy when no check
    /// failed, review on warnings, down on error/critical failures,
    /// unknown when the checks could not run.
    fn subsystem_status(findings: &[&Finding]) -> SubsystemStatus {
        if findings.is_empty() {
            return SubsystemStatus::Unknown;
        }
        if findings.iter().any(|f| f.incomplete) {
            return SubsystemStatus::Unknown;
        }
        if findings.iter().any(|f| {
            f.status == CheckStatus::Failed
                && matches!(f.severity, Severity::Critical | Severity::Error)
        }) {
            return SubsystemStatus::Down;
        }
        if findings.iter().any(|f| f.status == CheckStatus::Failed) {
            return SubsystemStatus::Review;
        }
        SubsystemStatus::Healthy
    }

    /// Build the `pantheon.health/v1` document from a diagnostics report.
    pub fn build(report: &DiagnosticReport) -> HealthResponse {
        let subsystems: Vec<HealthSubsystem> = Subsystem::ALL
            .iter()
            .map(|sub| {
                let group: Vec<&Finding> = report
                    .findings
                    .iter()
                    .filter(|f| f.subsystem == *sub)
                    .collect();
                let status = subsystem_status(&group);
                let checks = group
                    .into_iter()
                    .map(|f| HealthCheck {
                        id: f.id.clone(),
                        status: f.status,
                        severity: f.severity,
                        summary: f.summary.clone(),
                        details: f.details.clone().unwrap_or_default(),
                        impact: f.impact.clone(),
                        remediation: f.remediation.clone(),
                        duration_ns: f.duration_ns,
                    })
                    .collect();
                HealthSubsystem {
                    id: sub.id().to_string(),
                    title: sub.title().to_string(),
                    subtitle: sub.subtitle().to_string(),
                    status,
                    checks,
                }
            })
            .collect();

        let runtime_status = subsystems
            .iter()
            .find(|s| s.id == "runtime")
            .map(|s| s.status)
            .unwrap_or(SubsystemStatus::Unknown);

        // Issues: findings with severity error/warning/critical, most severe
        // first, stable by id within a severity.
        let mut actionable: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| {
                matches!(
                    f.severity,
                    Severity::Critical | Severity::Error | Severity::Warning
                ) && f.status == CheckStatus::Failed
            })
            .collect();
        actionable.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.id.cmp(&b.id)));
        let issues: Vec<HealthIssue> = actionable
            .into_iter()
            .map(|f| HealthIssue {
                id: f.id.clone(),
                subsystem: f.subsystem,
                severity: match f.severity {
                    Severity::Critical => "error".to_string(),
                    Severity::Error => "error".to_string(),
                    Severity::Warning => "warning".to_string(),
                    Severity::Info => "info".to_string(),
                },
                title: f.summary.clone(),
                detail: if f.impact.is_empty() {
                    f.details.clone().unwrap_or_default()
                } else {
                    f.impact.clone()
                },
                hint: f.remediation.clone(),
            })
            .collect();

        HealthResponse {
            schema: SCHEMA.to_string(),
            generated_at: report.generated_at.clone(),
            overall: report.overall,
            summary: report.summary.clone(),
            core_runtime: CoreRuntime {
                status: runtime_status,
                label: runtime_status.label().to_string(),
            },
            issue_count: issues.len(),
            subsystems,
            issues,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::finding::{DiagnosticReport, Finding};

        fn report_with(findings: Vec<Finding>) -> DiagnosticReport {
            let overall = crate::finding::aggregate(&findings);
            DiagnosticReport {
                schema: "pantheon.doctor/v1".to_string(),
                generated_at: "2026-10-14T12:00:00+01:00".to_string(),
                overall,
                summary: crate::finding::summarize(&findings),
                findings,
                duration_ns: 0,
            }
        }

        #[test]
        fn contract_shape_matches_spec() {
            let resp = build(&report_with(vec![
                Finding::passed("config.file", Subsystem::Config, "parsed"),
                Finding::passed(
                    "runtime.nightly_state",
                    Subsystem::Runtime,
                    "nightly pass enabled",
                ),
                Finding::failed(
                    "providers.model_key",
                    Subsystem::Providers,
                    Severity::Error,
                    "KEY is not set",
                    "model requests fail auth",
                    "export KEY=<key>",
                ),
                Finding::failed(
                    "tools.skill_deps",
                    Subsystem::Tools,
                    Severity::Warning,
                    "node not found",
                    "node-debug skill degrades",
                    "install node",
                ),
            ]));
            let v = serde_json::to_value(&resp).expect("serialize");
            assert_eq!(v["schema"], "pantheon.health/v1");
            assert_eq!(v["overall"], "degraded");
            assert_eq!(v["issue_count"], 2);
            assert_eq!(v["subsystems"].as_array().unwrap().len(), 6);
            // Subsystem ids/titles per the contract.
            let subs = v["subsystems"].as_array().unwrap();
            assert_eq!(subs[0]["id"], "runtime");
            assert_eq!(subs[0]["title"], "Runtime & execution");
            assert_eq!(subs[1]["title"], "Models & providers");
            assert_eq!(subs[2]["title"], "Memory & storage");
            assert_eq!(subs[3]["title"], "Tools & integrations");
            assert_eq!(subs[4]["title"], "Security");
            assert_eq!(subs[5]["title"], "Configuration");
            // Rollups: providers down (error), tools review (warning).
            let providers = &subs[1];
            assert_eq!(providers["status"], "down");
            assert_eq!(subs[3]["status"], "review");
            assert_eq!(subs[0]["status"], "healthy");
            // core_runtime mirrors the runtime subsystem.
            assert_eq!(v["core_runtime"]["status"], "healthy");
            assert_eq!(v["core_runtime"]["label"], "Healthy");
            // Issues: most severe first, critical would map to "error".
            let issues = v["issues"].as_array().unwrap();
            assert_eq!(issues[0]["id"], "providers.model_key");
            assert_eq!(issues[0]["severity"], "error");
            assert_eq!(issues[0]["title"], "KEY is not set");
            assert_eq!(issues[0]["detail"], "model requests fail auth");
            assert_eq!(issues[0]["hint"], "export KEY=<key>");
            assert_eq!(issues[0]["subsystem"], "providers");
            assert_eq!(issues[1]["severity"], "warning");
        }

        #[test]
        fn critical_maps_to_error_in_issues_but_down_in_subsystem() {
            let resp = build(&report_with(vec![Finding::failed(
                "memory.ledger",
                Subsystem::Memory,
                Severity::Critical,
                "ledger does not open",
                "no persistence",
                "check disk",
            )]));
            let v = serde_json::to_value(&resp).expect("serialize");
            assert_eq!(v["overall"], "unhealthy");
            assert_eq!(v["subsystems"][2]["status"], "down");
            assert_eq!(v["issues"][0]["severity"], "error");
        }

        #[test]
        fn incomplete_check_is_unknown_everywhere() {
            let resp = build(&report_with(vec![Finding {
                incomplete: true,
                ..Finding::failed(
                    "tools.plugins",
                    Subsystem::Tools,
                    Severity::Error,
                    "panicked",
                    "unevaluated",
                    "rerun",
                )
            }]));
            let v = serde_json::to_value(&resp).expect("serialize");
            assert_eq!(v["overall"], "unknown");
            assert_eq!(v["subsystems"][3]["status"], "unknown");
        }

        #[test]
        fn empty_subsystem_is_unknown_not_healthy() {
            // A subsystem with no findings must not read as healthy.
            let resp = build(&report_with(vec![]));
            let v = serde_json::to_value(&resp).expect("serialize");
            for s in v["subsystems"].as_array().unwrap() {
                assert_eq!(s["status"], "unknown");
            }
            assert_eq!(v["core_runtime"]["status"], "unknown");
            assert_eq!(v["core_runtime"]["label"], "Unknown");
        }
    }
}

pub mod probe {
    //! Read-only probes: TCP reachability and bounded binary detection.
    //!
    //! Network probes are explicit and bounded: nothing here phones home
    //! unless the caller asked for it (`doctor --ping`), and every probe
    //! carries a deadline so a hung network cannot stall the run.

    use std::time::Duration;

    /// Split an `http(s)://host[:port][/...]` URL into `(host, port)`.
    /// Default ports follow the scheme. Returns `None` for anything that
    /// does not look like an HTTP URL - the probe treats that as
    /// unreachable rather than guessing.
    pub fn host_port(url: &str) -> Option<(String, u16)> {
        let after = url.split("://").nth(1)?;
        let hostport = after.split('/').next()?;
        // Strip userinfo if present.
        let hostport = hostport.rsplit('@').next()?;
        if let Some((h, p)) = hostport.rsplit_once(':') {
            if let Ok(port) = p.parse::<u16>() {
                let host = h.trim_matches(|c| c == '[' || c == ']');
                return Some((host.to_string(), port));
            }
        }
        let port = if url.starts_with("https") { 443 } else { 80 };
        Some((hostport.to_string(), port))
    }

    /// TCP reachability probe for an HTTP(S) base URL: `true` when something
    /// accepts a TCP connection within `timeout`. Plain connect, no TLS, no
    /// HTTP request - reachability only, never authentication.
    pub fn tcp_probe(url: &str, timeout: Duration) -> bool {
        use std::net::ToSocketAddrs;
        let (host, port) = match host_port(url) {
            Some(hp) => hp,
            None => return false,
        };
        match (host.as_str(), port).to_socket_addrs() {
            Ok(mut addrs) => {
                addrs.any(|a| std::net::TcpStream::connect_timeout(&a, timeout).is_ok())
            }
            Err(_) => false,
        }
    }

    /// Run `sh -c <detect_cmd>` with a deadline. A probe that hangs must not
    /// stall `doctor`: a `python3 -m pip --version` once took ~9.5s on a cold
    /// box, so the timeout is load-bearing, not decorative.
    pub fn detect_binary(detect_cmd: &str, timeout: Duration) -> bool {
        let mut child = match std::process::Command::new("sh")
            .arg("-c")
            .arg(detect_cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return false,
            }
        }
    }

    /// Default deadline for binary detection probes.
    pub const DETECT_TIMEOUT: Duration = Duration::from_secs(5);

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn tcp_probe_rejects_garbage_and_refused_ports() {
            // Nothing listens on 1; the probe must answer fast, not hang.
            let start = std::time::Instant::now();
            assert!(!tcp_probe("http://127.0.0.1:1/v1", Duration::from_secs(3)));
            assert!(!tcp_probe("not a url", Duration::from_secs(3)));
            assert!(start.elapsed() < Duration::from_secs(10));
        }

        #[test]
        fn tcp_probe_sees_a_listener() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            assert!(tcp_probe(
                &format!("http://127.0.0.1:{port}/v1"),
                Duration::from_secs(3)
            ));
        }

        #[test]
        fn host_port_parses_common_shapes() {
            assert_eq!(
                host_port("http://127.0.0.1:11434/v1"),
                Some(("127.0.0.1".to_string(), 11434))
            );
            assert_eq!(
                host_port("https://api.openai.com/v1"),
                Some(("api.openai.com".to_string(), 443))
            );
            assert_eq!(
                host_port("http://example.com"),
                Some(("example.com".to_string(), 80))
            );
            assert_eq!(host_port("not a url"), None);
        }

        #[test]
        fn detect_binary_true_and_false() {
            assert!(detect_binary("command -v sh", Duration::from_secs(5)));
            assert!(!detect_binary(
                "command -v pantheon-definitely-not-a-binary",
                Duration::from_secs(5)
            ));
        }
    }
}

pub mod registry {
    //! Diagnostic registry and orchestrator.
    //!
    //! Checks register as plain functions (`fn(&DoctorCtx) -> Vec<Finding>`);
    //! the orchestrator invokes them, records wall time, and converts a
    //! panicking or empty-returning check into an `incomplete` finding so a
    //! broken check can never silently read as healthy.

    use crate::finding::{
        aggregate, summarize, CheckStatus, DiagnosticReport, Finding, Severity, Subsystem,
    };
    use std::path::PathBuf;
    use std::time::Instant;

    /// Progress sink type: e.g. `|s| eprintln!("doctor: {s}")`.
    pub type ProgressSink = Box<dyn Fn(&str) + Send + Sync>;

    /// What a check run needs. Everything a check touches comes through
    /// here - checks never read process globals directly, which keeps them
    /// testable and keeps the HTTP path honest about its inputs.
    pub struct DoctorCtx {
        /// The install's data directory (`~/.pantheon` by default).
        pub data_dir: PathBuf,
        /// Plugin/extension directory. Defaults to `<data_dir>/extensions`
        /// (honoring `PANTHEON_EXT_DIR`); the host resolves it.
        pub ext_dir: PathBuf,
        /// Opt-in network reachability probe of the configured model
        /// provider (`doctor --ping`). Without it doctor stays
        /// config-and-filesystem-only.
        pub ping_provider: bool,
        /// Progress sink, e.g. `|s| eprintln!("doctor: {s}")`. The CLI wires
        /// it to stderr so `--json` stdout stays machine-readable; the HTTP
        /// path leaves it unset.
        pub progress: Option<ProgressSink>,
    }

    impl DoctorCtx {
        pub fn new(data_dir: PathBuf, ext_dir: PathBuf) -> Self {
            DoctorCtx {
                data_dir,
                ext_dir,
                ping_provider: false,
                progress: None,
            }
        }

        fn progress(&self, msg: &str) {
            if let Some(p) = &self.progress {
                p(msg);
            }
        }
    }

    /// One registered diagnostic check.
    pub struct CheckDef {
        /// Stable id, e.g. `memory.ledger.open`.
        pub id: &'static str,
        /// Which subsystem this check belongs to.
        pub subsystem: Subsystem,
        /// Human label for verbose output.
        pub title: &'static str,
        /// The check itself. Returns findings, never prints.
        pub run: fn(&DoctorCtx) -> Vec<Finding>,
    }

    /// The diagnostic registry: the set of checks one `doctor` run executes.
    #[derive(Default)]
    pub struct Registry {
        checks: Vec<CheckDef>,
    }

    impl Registry {
        pub fn new() -> Self {
            Registry { checks: Vec::new() }
        }

        /// The built-in check suite (see [`crate::checks`]).
        pub fn core() -> Self {
            let mut r = Registry::new();
            for def in crate::checks::all_checks() {
                r.register(def);
            }
            r
        }

        pub fn register(&mut self, def: CheckDef) {
            self.checks.push(def);
        }

        pub fn len(&self) -> usize {
            self.checks.len()
        }

        pub fn is_empty(&self) -> bool {
            self.checks.is_empty()
        }

        /// Run every check (or one subsystem's checks with `only`), catching
        /// individual check failures without aborting the run.
        pub fn run(&self, ctx: &DoctorCtx, only: Option<Subsystem>) -> DiagnosticReport {
            let run_start = Instant::now();
            let mut findings = Vec::new();
            for def in &self.checks {
                if let Some(sub) = only {
                    if def.subsystem != sub {
                        continue;
                    }
                }
                ctx.progress(def.id);
                findings.extend(run_one(def, ctx));
            }
            let overall = aggregate(&findings);
            DiagnosticReport {
                schema: "pantheon.doctor/v1".to_string(),
                generated_at: chrono::Utc::now().to_rfc3339(),
                summary: summarize(&findings),
                overall,
                findings,
                duration_ns: run_start.elapsed().as_nanos() as u64,
            }
        }
    }

    /// Invoke one check: time it, catch panics, and stamp durations. A
    /// check that panics or returns no findings becomes an `incomplete`
    /// finding - the orchestrator never lets a broken check look clean.
    fn run_one(def: &CheckDef, ctx: &DoctorCtx) -> Vec<Finding> {
        let start = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (def.run)(ctx)));
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        let mut findings = match result {
            Ok(f) => f,
            Err(payload) => vec![Finding {
                id: def.id.to_string(),
                subsystem: def.subsystem,
                severity: Severity::Error,
                status: CheckStatus::Failed,
                summary: format!("check '{}' panicked: {}", def.id, panic_message(&payload)),
                details: None,
                impact: "this diagnostic could not complete; its subject is unevaluated"
                    .to_string(),
                remediation: "rerun `pantheon doctor`; if it persists, report the check id"
                    .to_string(),
                duration_ns: 0,
                fix_id: None,
                incomplete: true,
            }],
        };
        if findings.is_empty() {
            findings.push(Finding {
                id: def.id.to_string(),
                subsystem: def.subsystem,
                severity: Severity::Error,
                status: CheckStatus::Failed,
                summary: format!("check '{}' produced no findings", def.id),
                details: None,
                impact: "this diagnostic could not complete; its subject is unevaluated"
                    .to_string(),
                remediation: "rerun `pantheon doctor`; if it persists, report the check id"
                    .to_string(),
                duration_ns: 0,
                fix_id: None,
                incomplete: true,
            });
        }
        for f in &mut findings {
            f.duration_ns = elapsed_ns;
        }
        findings
    }

    fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = payload.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::finding::OverallHealth;

        fn ok_check(_ctx: &DoctorCtx) -> Vec<Finding> {
            vec![Finding::passed("test.ok", Subsystem::Config, "fine")]
        }

        fn panic_check(_ctx: &DoctorCtx) -> Vec<Finding> {
            panic!("boom");
        }

        fn empty_check(_ctx: &DoctorCtx) -> Vec<Finding> {
            Vec::new()
        }

        fn other_subsystem_check(_ctx: &DoctorCtx) -> Vec<Finding> {
            vec![Finding::passed("test.other", Subsystem::Tools, "fine")]
        }

        fn ctx() -> DoctorCtx {
            DoctorCtx::new(
                std::path::PathBuf::from("/tmp/doctor-test"),
                std::path::PathBuf::from("/tmp/doctor-test/ext"),
            )
        }

        #[test]
        fn panicking_check_becomes_incomplete_unknown() {
            let mut r = Registry::new();
            r.register(CheckDef {
                id: "test.panic",
                subsystem: Subsystem::Config,
                title: "panic",
                run: panic_check,
            });
            let rep = r.run(&ctx(), None);
            assert_eq!(rep.findings.len(), 1);
            assert!(rep.findings[0].incomplete);
            assert_eq!(rep.overall, OverallHealth::Unknown);
        }

        #[test]
        fn empty_check_becomes_incomplete() {
            let mut r = Registry::new();
            r.register(CheckDef {
                id: "test.empty",
                subsystem: Subsystem::Config,
                title: "empty",
                run: empty_check,
            });
            let rep = r.run(&ctx(), None);
            assert!(rep.findings[0].incomplete);
            assert_eq!(rep.overall, OverallHealth::Unknown);
        }

        #[test]
        fn subsystem_filter_runs_only_matching_checks() {
            let mut r = Registry::new();
            r.register(CheckDef {
                id: "test.ok",
                subsystem: Subsystem::Config,
                title: "ok",
                run: ok_check,
            });
            r.register(CheckDef {
                id: "test.other",
                subsystem: Subsystem::Tools,
                title: "other",
                run: other_subsystem_check,
            });
            let rep = r.run(&ctx(), Some(Subsystem::Config));
            assert_eq!(rep.findings.len(), 1);
            assert_eq!(rep.findings[0].id, "test.ok");
        }

        fn two_findings_check(_ctx: &DoctorCtx) -> Vec<Finding> {
            vec![
                Finding::passed("test.one", Subsystem::Config, "fine"),
                Finding::passed("test.two", Subsystem::Config, "fine"),
            ]
        }

        #[test]
        fn durations_are_stamped_per_check() {
            // The orchestrator stamps every finding from one check with that
            // check's wall time: same check => same duration_ns.
            let mut r = Registry::new();
            r.register(CheckDef {
                id: "test.two",
                subsystem: Subsystem::Config,
                title: "two",
                run: two_findings_check,
            });
            let rep = r.run(&ctx(), None);
            assert_eq!(rep.findings.len(), 2);
            assert_eq!(
                rep.findings[0].duration_ns, rep.findings[1].duration_ns,
                "findings from one check must share its wall time"
            );
        }

        #[test]
        fn core_registry_is_nonempty() {
            assert!(!Registry::core().is_empty());
        }
    }
}

pub mod render {
    //! Output renderers for a [`DiagnosticReport`].
    //!
    //! Three shapes, one canonical option each:
    //!
    //! - concise (default): failures and warnings only, grouped by
    //!   subsystem in canonical order, most severe first. Never a line per
    //!   successful check.
    //! - verbose (`--verbose`): every finding, including passed / skipped /
    //!   not-applicable, with evidence and per-check timings.
    //! - JSON (`--json`): stdout contains ONLY valid JSON; progress and
    //!   incidental diagnostics go to stderr (the orchestrator's progress
    //!   sink), never stdout.
    //!
    //! Exit codes (decided here, so CLI and tests cannot drift):
    //!
    //! - 0: no errors/warnings requiring attention (healthy)
    //! - 1: findings indicate degraded or unhealthy
    //! - 2: invalid arguments or config (decided by arg parsing, not here)
    //! - 3: diagnostics could not complete reliably (unknown)

    use crate::finding::{
        CheckStatus, DiagnosticReport, Finding, OverallHealth, Severity, Subsystem,
    };

    /// Exit code for a finished diagnostics run.
    pub fn exit_code(report: &DiagnosticReport) -> i32 {
        match report.overall {
            OverallHealth::Healthy => 0,
            OverallHealth::Degraded | OverallHealth::Unhealthy => 1,
            OverallHealth::Unknown => 3,
        }
    }

    fn severity_glyph(sev: Severity) -> &'static str {
        match sev {
            Severity::Critical => "CRIT",
            Severity::Error => "FAIL",
            Severity::Warning => "warn",
            Severity::Info => "info",
        }
    }

    /// Concise rendering: ONLY failures and warnings, grouped by subsystem
    /// (canonical order), most severe first within each group.
    pub fn render_concise(report: &DiagnosticReport) -> String {
        let mut out = String::new();
        out.push_str(&format!("pantheon doctor: {}\n", report.summary));
        let actionable: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.is_actionable())
            .collect();
        if actionable.is_empty() {
            out.push_str("no actionable findings\n");
        } else {
            for sub in Subsystem::ALL {
                let mut group: Vec<&Finding> = actionable
                    .iter()
                    .filter(|f| f.subsystem == sub)
                    .copied()
                    .collect();
                if group.is_empty() {
                    continue;
                }
                // Most severe first; stable by check id within a severity.
                group.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.id.cmp(&b.id)));
                out.push_str(&format!("\n[{}] {}\n", sub.id(), sub.title()));
                for f in group {
                    out.push_str(&format!(
                        "  {} {}: {}\n",
                        severity_glyph(f.severity),
                        f.id,
                        f.summary
                    ));
                    if !f.impact.is_empty() {
                        out.push_str(&format!("       impact: {}\n", f.impact));
                    }
                    if !f.remediation.is_empty() {
                        out.push_str(&format!("       fix: {}\n", f.remediation));
                    }
                }
            }
        }
        // Skipped / not-applicable findings are hidden above by design;
        // name the count so an "unknown" verdict is never a mystery.
        let hidden = report
            .findings
            .iter()
            .filter(|f| matches!(f.status, CheckStatus::Skipped | CheckStatus::NotApplicable))
            .count();
        if hidden > 0 {
            out.push_str(&format!(
                "\n{hidden} check(s) skipped or not applicable (see --verbose)\n"
            ));
        }
        out
    }

    /// Verbose rendering: every finding, with evidence and timings.
    /// Narrow-terminal safe: no wide tables, wrapping left to the pager.
    pub fn render_verbose(report: &DiagnosticReport) -> String {
        let mut out = String::new();
        out.push_str(&format!("pantheon doctor: {}\n", report.summary));
        out.push_str(&format!(
            "generated: {} ({} findings, {:.1}s)\n",
            report.generated_at,
            report.findings.len(),
            report.duration_ns as f64 / 1_000_000_000.0,
        ));
        for sub in Subsystem::ALL {
            let group: Vec<&Finding> = report
                .findings
                .iter()
                .filter(|f| f.subsystem == sub)
                .collect();
            if group.is_empty() {
                continue;
            }
            out.push_str(&format!("\n[{}] {}\n", sub.id(), sub.title()));
            for f in group {
                let status = match f.status {
                    CheckStatus::Passed => "pass",
                    CheckStatus::Failed => severity_glyph(f.severity),
                    CheckStatus::Skipped => "skip",
                    CheckStatus::NotApplicable => "n/a ",
                };
                out.push_str(&format!("  [{status}] {}: {}\n", f.id, f.summary));
                if let Some(d) = &f.details {
                    for line in d.lines() {
                        out.push_str(&format!("         | {line}\n"));
                    }
                }
                if !f.impact.is_empty() {
                    out.push_str(&format!("         impact: {}\n", f.impact));
                }
                if !f.remediation.is_empty() {
                    out.push_str(&format!("         fix: {}\n", f.remediation));
                }
                out.push_str(&format!(
                    "         ({:.1}ms)\n",
                    f.duration_ns as f64 / 1_000_000.0
                ));
            }
        }
        out
    }

    /// JSON rendering: the report as JSON, nothing else. Serialization
    /// failure is a hard error - a corrupt machine-readable report is
    /// worse than none.
    pub fn render_json(report: &DiagnosticReport) -> Result<String, String> {
        serde_json::to_string_pretty(report)
            .map_err(|e| format!("could not serialize the diagnostics report as JSON: {e}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::finding::Finding;

        fn report_with(findings: Vec<Finding>) -> DiagnosticReport {
            let overall = crate::finding::aggregate(&findings);
            DiagnosticReport {
                schema: "pantheon.doctor/v1".to_string(),
                generated_at: "2026-10-14T12:00:00+01:00".to_string(),
                overall,
                summary: crate::finding::summarize(&findings),
                findings,
                duration_ns: 0,
            }
        }

        fn failed(id: &str, sub: Subsystem, sev: Severity) -> Finding {
            Finding::failed(id, sub, sev, "broken", "impact", "fix it")
        }

        #[test]
        fn exit_codes_match_overall() {
            assert_eq!(
                exit_code(&report_with(vec![Finding::passed(
                    "a",
                    Subsystem::Config,
                    "ok"
                )])),
                0
            );
            assert_eq!(
                exit_code(&report_with(vec![failed(
                    "a",
                    Subsystem::Config,
                    Severity::Warning
                )])),
                1
            );
            assert_eq!(
                exit_code(&report_with(vec![failed(
                    "a",
                    Subsystem::Config,
                    Severity::Critical
                )])),
                1
            );
            let incomplete = Finding {
                incomplete: true,
                ..failed("a", Subsystem::Config, Severity::Error)
            };
            assert_eq!(exit_code(&report_with(vec![incomplete])), 3);
        }

        #[test]
        fn concise_hides_passing_checks() {
            let text = render_concise(&report_with(vec![
                Finding::passed("a", Subsystem::Config, "all good"),
                failed("b", Subsystem::Tools, Severity::Warning),
            ]));
            assert!(!text.contains("all good"), "passing check leaked: {text}");
            assert!(!text.contains("\"a\""), "passing check id leaked: {text}");
            assert!(text.contains("\"b\"") || text.contains("b:"), "{text}");
            assert!(text.contains("warn"), "{text}");
        }

        #[test]
        fn concise_groups_by_subsystem_severe_first() {
            let text = render_concise(&report_with(vec![
                failed("z.warn", Subsystem::Tools, Severity::Warning),
                failed("a.crit", Subsystem::Tools, Severity::Critical),
                failed("m.err", Subsystem::Config, Severity::Error),
            ]));
            let tools_pos = text.find("[tools]").expect("tools group");
            let config_pos = text.find("[config]").expect("config group");
            assert!(tools_pos < config_pos, "canonical order: {text}");
            let crit_pos = text.find("a.crit").expect("crit");
            let warn_pos = text.find("z.warn").expect("warn");
            assert!(crit_pos < warn_pos, "most severe first: {text}");
        }

        #[test]
        fn concise_names_hidden_skips() {
            let text = render_concise(&report_with(vec![
                Finding::skipped("a", Subsystem::Config, "not asked"),
                Finding::not_applicable("b", Subsystem::Tools, "n/a"),
            ]));
            assert!(
                text.contains("2 check(s) skipped or not applicable"),
                "{text}"
            );
            assert!(text.contains("see --verbose"), "{text}");
        }

        #[test]
        fn concise_clean_run_says_so() {
            let text = render_concise(&report_with(vec![Finding::passed(
                "a",
                Subsystem::Config,
                "ok",
            )]));
            assert!(text.contains("no actionable findings"), "{text}");
        }

        #[test]
        fn verbose_shows_everything_with_timings() {
            let text = render_verbose(&report_with(vec![
                Finding::passed("a", Subsystem::Config, "ok"),
                Finding::skipped("b", Subsystem::Tools, "not asked"),
                failed("c", Subsystem::Security, Severity::Error),
            ]));
            assert!(text.contains("pass"), "{text}");
            assert!(text.contains("skip"), "{text}");
            assert!(text.contains("FAIL"), "{text}");
            assert!(text.contains("ms)"), "{text}");
        }

        #[test]
        fn json_is_valid_and_complete() {
            let json = render_json(&report_with(vec![failed(
                "a",
                Subsystem::Config,
                Severity::Error,
            )]))
            .expect("serialize");
            let v: serde_json::Value =
                serde_json::from_str(&json).expect("stdout must be valid JSON");
            assert_eq!(v["overall"], "degraded");
            assert_eq!(v["findings"][0]["id"], "a");
            assert_eq!(v["findings"][0]["severity"], "error");
            assert_eq!(v["findings"][0]["status"], "failed");
        }
    }
}

pub mod skill_deps {
    //! Skill dependencies: third-party binaries and packages the bundled
    //! skill library needs, surfaced at `pantheon setup` (install-or-skip)
    //! and reported by `pantheon doctor` (warn, never fail).
    //!
    //! The registry is static: one row per dependency, detected by shelling
    //! out read-only (`sh -c <detect_cmd>`, see `setup_providers::detect_binary`
    //! for the timeout semantics). Nothing here installs silently - the
    //! install path only runs after an explicit confirm from the user, and a
    //! skip is recorded in `[skill_deps].skipped` (pantheon-api
    //! `SkillDepsSection`) so doctor can converge on it later.

    use std::process::Command;

    /// One dependency the skill library needs.
    pub struct SkillDep {
        /// Stable id recorded in `[skill_deps].skipped`.
        pub id: &'static str,
        /// Human name shown in setup and doctor output.
        pub name: &'static str,
        /// How to notice the dependency is present. Read-only, short.
        pub detect_cmd: &'static str,
        /// What to run when the user says install. `None` = advise only.
        pub install_cmd: Option<&'static str>,
        /// Shown when there is no scripted install (or the install failed).
        pub install_hint: &'static str,
        /// Which bundled skill(s) need it, for the doctor warn line.
        pub needed_by: &'static str,
        /// True when the dependency is a Python package and doctor should
        /// also check that `pip` itself is usable.
        pub needs_pip: bool,
    }

    /// The registry. Sorted by id so doctor output is stable.
    pub fn skill_deps() -> &'static [SkillDep] {
        &[
            SkillDep {
                id: "debugpy",
                name: "debugpy",
                detect_cmd: "python3 -m debugpy --version",
                install_cmd: Some("python3 -m pip install --user debugpy"),
                install_hint: "Install with `python3 -m pip install --user debugpy`.",
                needed_by: "python-debug",
                needs_pip: true,
            },
            SkillDep {
                id: "himalaya",
                name: "himalaya",
                detect_cmd: "command -v himalaya",
                install_cmd: None,
                install_hint: "Install the himalaya CLI (github.com/soywod/himalaya), \
                           then run `himalaya account configure`.",
                needed_by: "himalaya-imap",
                needs_pip: false,
            },
            SkillDep {
                id: "node",
                name: "node",
                detect_cmd: "command -v node",
                install_cmd: None,
                install_hint: "Install Node.js from nodejs.org or your package manager.",
                needed_by: "node-debug",
                needs_pip: false,
            },
            SkillDep {
                id: "python3",
                name: "python3",
                detect_cmd: "command -v python3",
                install_cmd: None,
                install_hint: "Install Python 3 from python.org or your package manager.",
                needed_by: "python-debug",
                needs_pip: false,
            },
        ]
    }

    /// Look one dependency up by stable id (doctor's stale-record hygiene).
    pub fn find_dep(id: &str) -> Option<&'static SkillDep> {
        skill_deps().iter().find(|d| d.id == id)
    }

    /// True when `pip` can run (needed before a `pip install` dep is offered).
    pub fn pip_ready(detect: &dyn Fn(&str) -> bool) -> bool {
        detect("python3 -m pip --version")
    }

    /// Detect every dependency and return the ids that are missing AND
    /// skipped by the user. The scripted (`--yes`) path passes
    /// `offer_install = None`-shaped logic here: detect only, record the
    /// missing ids as skipped, never install behind the user's back.
    pub fn detect_all_skipped(detect: &dyn Fn(&str) -> bool) -> Vec<String> {
        skill_deps()
            .iter()
            .filter(|d| !detect(d.detect_cmd))
            .map(|d| d.id.to_string())
            .collect()
    }

    /// The interactive install-or-skip screen. One status line per dep
    /// (present / missing + which skill needs it), then for each missing
    /// dep: offer the install, run it through `install` on confirm, and
    /// record the id as skipped on decline or failure.
    /// Returns the skipped ids for `[skill_deps].skipped`.
    pub fn run_skill_deps_screen(
        say: &mut dyn FnMut(&str),
        confirm: &mut dyn FnMut(&str, bool) -> Option<bool>,
        install: &mut dyn FnMut(&str) -> bool,
        detect: &dyn Fn(&str) -> bool,
    ) -> Vec<String> {
        let mut skipped = Vec::new();
        for dep in skill_deps() {
            if detect(dep.detect_cmd) {
                say(&format!("{}: found", dep.name));
                continue;
            }
            say(&format!(
                "{}: missing - needed by {}",
                dep.name, dep.needed_by
            ));
            let wants = confirm(&format!("Install {} now?", dep.name), true);
            let installed = match wants {
                Some(true) => match dep.install_cmd {
                    Some(cmd) => {
                        say(&format!("installing {} ...", dep.name));
                        let ok = install(cmd);
                        if !ok {
                            say(&format!("install failed - {}", dep.install_hint));
                        }
                        ok
                    }
                    None => {
                        say(&format!("no scripted install: {}", dep.install_hint));
                        false
                    }
                },
                // None = user aborted the screen: treat like a skip, not a
                // crash. Every remaining undetected dep is recorded skipped.
                Some(false) | None => false,
            };
            if !installed {
                say(&format!("recorded {} as skipped", dep.id));
                skipped.push(dep.id.to_string());
            } else if !detect(dep.detect_cmd) {
                // The installer reported success but detection still fails:
                // record the skip so doctor reports the real state later
                // instead of pretending the dep exists.
                say(&format!(
                    "installed but {} still not detected - recorded as skipped",
                    dep.id
                ));
                skipped.push(dep.id.to_string());
            }
        }
        skipped
    }

    /// The default install runner: shell out, surface nothing on success.
    pub fn shell_install(cmd: &str) -> bool {
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn detect_true(_: &str) -> bool {
            true
        }
        fn detect_false(_: &str) -> bool {
            false
        }

        #[test]
        fn registry_is_sorted_and_ids_unique() {
            let deps = skill_deps();
            let mut ids: Vec<&str> = deps.iter().map(|d| d.id).collect();
            let sorted = ids.clone();
            ids.sort_unstable();
            assert_eq!(ids, sorted, "registry must stay sorted by id");
            let unique = ids.clone();
            ids.dedup();
            assert_eq!(ids, unique, "ids must be unique");
        }

        #[test]
        fn every_needed_by_names_a_real_skill_file() {
            // The needed_by value names a bundled skill directory; a typo
            // here makes doctor's warn line point at a skill that is gone.
            for dep in skill_deps() {
                let dir = concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../pantheon-exec/bundled-skills"
                );
                let skill = std::path::Path::new(dir).join(dep.needed_by);
                assert!(
                    skill.join("SKILL.md").exists(),
                    "{} points at missing bundled skill {}",
                    dep.id,
                    dep.needed_by
                );
            }
        }

        #[test]
        fn find_dep_round_trips_and_misses_cleanly() {
            assert!(find_dep("himalaya").is_some());
            assert!(find_dep("no-such-dep").is_none());
        }

        #[test]
        fn detect_all_skipped_lists_only_missing() {
            // All present -> nothing skipped; none present -> all ids.
            assert!(detect_all_skipped(&detect_true).is_empty());
            let skipped = detect_all_skipped(&detect_false);
            let ids: Vec<&str> = skill_deps().iter().map(|d| d.id).collect();
            assert_eq!(skipped, ids);
        }

        #[test]
        fn screen_skips_when_user_declines_install() {
            let mut lines: Vec<String> = Vec::new();
            let mut say = |l: &str| lines.push(l.to_string());
            let mut confirm = |_: &str, _: bool| Some(false);
            let mut install = |_: &str| {
                panic!("install must not run when the user declines");
            };
            let skipped =
                run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_false);
            let ids: Vec<&str> = skill_deps().iter().map(|d| d.id).collect();
            assert_eq!(skipped, ids);
            assert!(lines
                .iter()
                .any(|l| l.contains("recorded himalaya as skipped")));
        }

        #[test]
        fn screen_records_skip_when_install_fails() {
            let mut say = |_: &str| {};
            let mut confirm = |_: &str, _: bool| Some(true);
            let mut install = |_: &str| false;
            let skipped =
                run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_false);
            assert_eq!(skipped.len(), skill_deps().len());
        }

        #[test]
        fn screen_silent_when_everything_present() {
            let mut say = |_: &str| {};
            let mut confirm =
                |_: &str, _: bool| -> Option<bool> { panic!("no confirm when all present") };
            let mut install = |_: &str| -> bool { panic!("no install when all present") };
            let skipped = run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_true);
            assert!(skipped.is_empty());
        }

        #[test]
        fn screen_records_skip_when_install_succeeds_but_detection_still_fails() {
            let mut say = |_: &str| {};
            let mut confirm = |_: &str, _: bool| Some(true);
            let mut install = |_: &str| true;
            let skipped =
                run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_false);
            assert_eq!(skipped.len(), skill_deps().len());
        }

        #[test]
        fn screen_treats_aborted_confirm_as_skip_not_install() {
            let mut say = |_: &str| {};
            let mut confirm = |_: &str, _: bool| None;
            let mut install = |_: &str| -> bool { panic!("aborted screen must not install") };
            let skipped =
                run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_false);
            assert_eq!(skipped.len(), skill_deps().len());
        }

        #[test]
        fn shell_install_survives_a_missing_shell() {
            assert!(!shell_install("definitely-not-a-real-command-xyz"));
        }
    }
}
