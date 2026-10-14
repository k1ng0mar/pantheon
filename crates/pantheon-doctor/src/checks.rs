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
use std::path::{Path, PathBuf};

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
        CheckDef {
            id: "tools.cloudflare",
            subsystem: Subsystem::Tools,
            title: "cloudflare integration",
            run: check_cloudflare,
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
        Err(e) => vec![Finding::failed(
            "config.file",
            Subsystem::Config,
            Severity::Critical,
            format!("config does not parse: {}", e.cause),
            "nothing can run: the agent, the gateway, and the dashboard all need a parseable config",
            "run `pantheon setup`, or fix the TOML by hand",
        )
        .with_details(format!("path: {}\nerror: {}", path.display(), e.cause))],
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
    if problems.is_empty() {
        return vec![Finding::passed(
            "config.schema",
            Subsystem::Config,
            "validation clean",
        )];
    }
    // A config problem is an error, not a warning: `doctor` is the
    // preflight a user runs before their first session, so "no [model]
    // section" has to be actionable, not advisory.
    problems
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
        .collect()
}

/// Deprecated-section nudge for the unified nightly pass. `[nightly]`
/// is the single authoritative section; legacy `[reflect]` /
/// `[consolidation]` tables are honored field-by-field only when
/// `[nightly]` is absent, and ignored entirely when it is present.
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
    match cfg.and_then(|c| c.nightly.as_ref()) {
        Some(n) => pantheon_api::config::nightly_enabled(n),
        None => cfg.is_some_and(|c| {
            c.reflect.as_ref().is_some_and(|s| s.enabled)
                || c.consolidation.as_ref().is_some_and(|s| s.enabled)
        }),
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
            let reason = pantheon_api::config::nightly_enabled_reason(n);
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

fn check_gateway_channels(_ctx: &DoctorCtx) -> Vec<Finding> {
    // No token means the surface is simply off, which is a legitimate
    // configuration - but a user who expects the gateway to answer
    // should see that it is idle, so this stays a warning, never
    // silent and never a failure. Only presence is reported, never the
    // token value.
    let mut live = Vec::new();
    if std::env::var("PANTHEON_DISCORD_TOKEN").is_ok_and(|t| !t.is_empty()) {
        live.push("discord");
    }
    if std::env::var("PANTHEON_TELEGRAM_BOT_TOKEN").is_ok_and(|t| !t.is_empty()) {
        live.push("telegram");
    }
    if live.is_empty() {
        vec![Finding::failed(
            "runtime.gateway_channels",
            Subsystem::Runtime,
            Severity::Warning,
            "no channel tokens set; gateway is idle",
            "the messaging gateway has no live surface",
            "set PANTHEON_DISCORD_TOKEN and/or PANTHEON_TELEGRAM_BOT_TOKEN to run `pantheon gateway`",
        )]
    } else {
        vec![Finding::passed(
            "runtime.gateway_channels",
            Subsystem::Runtime,
            format!("{} surface(s) enabled: {}", live.len(), live.join(", ")),
        )]
    }
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
            vec![Finding::passed(
                "providers.model_key",
                Subsystem::Providers,
                format!("no API key required for {provider}"),
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
    match pantheon_storage::Ledger::open(&path) {
        Ok(ledger) => {
            let seq = ledger.max_seq().unwrap_or(-1);
            vec![Finding::passed(
                "memory.ledger",
                Subsystem::Memory,
                format!("ledger opens (max seq {seq})"),
            )
            .with_details(format!("path: {}", path.display()))]
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
    let binary = section.binary.clone();
    let found = match binary.as_deref() {
        Some(b) => Path::new(b).is_file().then(|| b.to_string()),
        None => std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths).find_map(|dir| {
                let p = dir.join("cua-driver");
                p.is_file().then(|| p.to_string_lossy().into_owned())
            })
        }),
    };
    match found {
        Some(p) => vec![Finding::passed(
            "tools.computer_use",
            Subsystem::Tools,
            format!("cua-driver at {p}"),
        )],
        None => vec![Finding::failed(
            "tools.computer_use",
            Subsystem::Tools,
            Severity::Warning,
            "cua-driver not found on PATH",
            "the ComputerUse tool group is enabled but its driver is missing; computer-use calls fail",
            "install: /bin/bash -c \"$(curl -fsSL https://cua.ai/driver/install.sh)\"",
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
                    .with_remediation("remove it from [skill_deps].skipped to clear the record"),
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
    // is what `skills list` is for). Discovery idempotently seeds
    // bundled skills first - the same seeding every session performs -
    // and never overwrites a user-edited copy.
    let project_root = std::env::current_dir().unwrap_or_else(|_| ctx.data_dir.clone());
    let scan =
        pantheon_exec::skills::scan_skills_ext(&ctx.data_dir, &project_root, &extra_skill_roots());
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

fn check_cloudflare(ctx: &DoctorCtx) -> Vec<Finding> {
    let cfg = match load_config(ctx) {
        Ok(c) => c,
        Err(_) => {
            return vec![Finding::skipped(
                "tools.cloudflare",
                Subsystem::Tools,
                "skipped: config does not parse (see config.file)",
            )];
        }
    };
    // Every gap is a warning, never an error: the integration is
    // optional and a missing token just means `cf` calls fail with cf's
    // own auth error, which the agent reports instead of acting on.
    let section = cfg.cloudflare.as_ref();
    if section.is_none() {
        return vec![Finding::not_applicable(
            "tools.cloudflare",
            Subsystem::Tools,
            "integration off ([cloudflare] absent)",
        )
        .with_remediation("run `pantheon cloudflare setup` to enable")];
    }
    // Only the secret NAME is reported; the token value is never read
    // into a finding.
    let secret_name = section
        .and_then(|s| s.api_token_secret.clone())
        .unwrap_or_else(|| "CLOUDFLARE_API_TOKEN".to_string());
    let token_ok = std::env::var(&secret_name)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let cf_ok = detect_binary("command -v cf", DETECT_TIMEOUT);
    let enabled = section.map(|s| s.enabled).unwrap_or(false);
    if !enabled {
        vec![Finding::failed(
            "tools.cloudflare",
            Subsystem::Tools,
            Severity::Warning,
            "[cloudflare] present but disabled: no token injection",
            "cloudflare calls run without injected credentials",
            "set enabled = true in [cloudflare] when the token is ready",
        )]
    } else if !cf_ok {
        vec![Finding::failed(
            "tools.cloudflare",
            Subsystem::Tools,
            Severity::Warning,
            "cloudflare enabled but the cf CLI is missing",
            "cloudflare tool calls fail without the CLI",
            "run `pantheon cloudflare setup` (installs the cf CLI)",
        )]
    } else if !token_ok {
        vec![Finding::failed(
            "tools.cloudflare",
            Subsystem::Tools,
            Severity::Warning,
            format!("cf present but {secret_name} is not set in the environment"),
            "cloudflare API calls fail authentication",
            "create a scoped API token (dashboard > My Profile > API Tokens) and export it",
        )]
    } else {
        vec![Finding::passed(
            "tools.cloudflare",
            Subsystem::Tools,
            format!("cf CLI present, {secret_name} resolves, token injection enabled"),
        )]
    }
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
            std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o644)).expect("chmod");
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
    fn cloudflare_absent_is_not_applicable() {
        let dir = temp_dir("no-cf");
        write_config(&dir, "");
        let f = check_cloudflare(&ctx_in(&dir));
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
        assert_eq!(seen.len(), 19);
    }
}
