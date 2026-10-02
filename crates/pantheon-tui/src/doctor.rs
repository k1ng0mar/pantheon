//! `pantheon doctor`: system-level preflight.
//!
//! Sections: config validation, model reachability, memory backend health,
//! plugin verification (reuses the extension doctor), data-dir integrity.
//! Every check reports pass/warn/fail with a fix hint. Exit code is 0 only
//! when nothing failed.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub section: String,
    pub status: String, // "ok" | "warn" | "fail"
    pub detail: String,
    pub fix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemReport {
    pub ok: bool,
    pub checks: Vec<Check>,
}

fn check(section: &str, status: &str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        section: section.into(),
        status: status.into(),
        detail: detail.into(),
        fix: fix.into(),
    }
}

/// Deprecated-section nudge for the unified nightly pass. `[nightly]`
/// is the single authoritative section; legacy `[reflect]` /
/// `[consolidation]` tables are honored field-by-field only when
/// `[nightly]` is absent, and ignored entirely when it is present. Warn
/// naming the fields to move: `enabled` + `auto_turns` from `[reflect]`,
/// `enabled` + `min_sessions` + `cron` from `[consolidation]`. The
/// aux-model pins (`provider`/`model`/`api_key_env`/`timeout`) live
/// nowhere else, so they legitimately stay in the legacy tables.
fn legacy_nightly_checks(c: &Config) -> Vec<Check> {
    let nightly_present = c.nightly.is_some();
    let mut out = Vec::new();
    if c.reflect.is_some() {
        out.push(check(
            "config",
            "warn",
            if nightly_present {
                "legacy [reflect] section present but ignored entirely while [nightly] exists; move `enabled`, `auto_turns` into [nightly]"
            } else {
                "legacy [reflect] section present (deprecated); move `enabled`, `auto_turns` into [nightly]"
            },
            "copy the [reflect] behavior knobs into [nightly], then delete those keys (the provider/model/timeout pin can stay)",
        ));
    }
    if c.consolidation.is_some() {
        out.push(check(
            "config",
            "warn",
            if nightly_present {
                "legacy [consolidation] section present but ignored entirely while [nightly] exists; move `enabled`, `min_sessions`, `cron` into [nightly]"
            } else {
                "legacy [consolidation] section present (deprecated); move `enabled`, `min_sessions`, `cron` into [nightly]"
            },
            "copy the [consolidation] behavior knobs into [nightly], then delete those keys (the provider/model/timeout pin can stay)",
        ));
    }
    out
}

/// Nightly state check: report whether the pass is enabled and why, and
/// - when it is disabled with no model pin - hint how to enable it: a
/// `[nightly.model]` pin, `/nightly on`, a config edit, or the dashboard
/// toggle. Pure over the config so the rule is unit-testable.
fn nightly_state_check(c: &Config) -> Check {
    let on = crate::config::nightly_pass_enabled(Some(c));
    match c.nightly.as_ref() {
        Some(n) => {
            let reason = pantheon_api::config::nightly_enabled_reason(n);
            let pin = pantheon_api::config::nightly_model_pin_present(n);
            if on {
                check("nightly", "ok", format!("pass enabled ({reason})"), "")
            } else if pin {
                check(
                    "nightly",
                    "warn",
                    format!("pass disabled ({reason})"),
                    "set `enabled = true` under [nightly] or run `/nightly on` to re-enable",
                )
            } else {
                check(
                    "nightly",
                    "warn",
                    format!("pass disabled ({reason})"),
                    "enable it: add a [nightly.model] pin, set `enabled = true`, run `/nightly on`, or use the dashboard toggle",
                )
            }
        }
        None => {
            let legacy_on = c.reflect.as_ref().is_some_and(|s| s.enabled)
                || c.consolidation.as_ref().is_some_and(|s| s.enabled);
            if legacy_on {
                check(
                    "nightly",
                    "ok",
                    "pass enabled via legacy [reflect]/[consolidation] (deprecated - move the knobs into [nightly])",
                    "move `enabled`, `auto_turns` / `enabled`, `min_sessions`, `cron` into [nightly]",
                )
            } else {
                check(
                    "nightly",
                    "warn",
                    "pass disabled (default: no [nightly] section)",
                    "enable it: add a [nightly.model] pin, set `enabled = true`, run `/nightly on`, or use the dashboard toggle",
                )
            }
        }
    }
}

/// The doctor fix hint for a missing skill dependency. Pure over
/// `pip_ok` so the pip-gating rule is unit-testable: a pip package with
/// no working pip must not suggest a pip install that cannot run.
fn skill_dep_fix(dep: &crate::skill_deps::SkillDep, pip_ok: bool) -> String {
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

/// Run every doctor check against a data dir.
///
/// `ping` enables the opt-in network reachability probe of the
/// configured model provider (`doctor --ping`); without it doctor stays
/// config-and-filesystem-only. Progress goes to stderr so `--json`
/// stdout stays machine-readable; the JSON itself is unchanged.
pub fn run_system_doctor(data_dir: &Path) -> SystemReport {
    run_system_doctor_opts(data_dir, false)
}

/// [`run_system_doctor`] with the `--ping` probe enabled or disabled.
pub fn run_system_doctor_opts(data_dir: &Path, ping: bool) -> SystemReport {
    let mut checks = Vec::new();

    // 1. Config file: parse + validate.
    eprintln!("doctor: config");
    let cfg = match Config::load(data_dir) {
        Ok(c) => {
            checks.push(check(
                "config",
                "ok",
                format!("parsed {}", Config::path(data_dir).display()),
                "",
            ));
            let problems = c.validate();
            if problems.is_empty() {
                checks.push(check("config", "ok", "validation clean", ""));
            } else {
                // A config problem is a failure, not a warning. `doctor` is
                // the preflight a user runs before their first session, so
                // "no [model] section" has to stop the run with a non-zero
                // exit. Downgrading every problem to a warning meant a
                // config that cannot run a conversation still passed.
                for p in problems {
                    checks.push(check(
                        "config",
                        "fail",
                        p.clone(),
                        "rerun `pantheon setup` or fix the config by hand",
                    ));
                }
            }
            Some(c)
        }
        Err(e) => {
            checks.push(check(
                "config",
                "fail",
                e.cause.clone(),
                "run `pantheon setup`",
            ));
            None
        }
    };

    // 2. Deprecated self-improvement sections: nudge `[reflect]` /
    eprintln!("doctor: nightly");
    //    `[consolidation]` users toward the authoritative `[nightly]`.
    if let Some(c) = cfg.as_ref() {
        checks.extend(legacy_nightly_checks(c));
    }

    // 2b. Nightly state: on/off and why, with the enable hint when off
    //    and unpinned.
    if let Some(c) = cfg.as_ref() {
        checks.push(nightly_state_check(c));
    }

    // 2c. Agent identities: visible by effective display name so a
    eprintln!("doctor: agents");
    //    misconfigured [agents.*] table is obvious, not silent.
    if let Some(c) = cfg.as_ref() {
        if c.agents.is_empty() {
            checks.push(check(
                "agents",
                "ok",
                "no [agents] tables (anonymous runs)",
                "",
            ));
        } else {
            let mut tables: Vec<&String> = c.agents.keys().collect();
            tables.sort();
            for t in tables {
                let id = &c.agents[t];
                checks.push(check(
                    "agents",
                    "ok",
                    format!(
                        "{t} (name {:?}, namespace {:?}, policy {:?})",
                        id.name(t),
                        id.namespace(t),
                        id.policy.as_deref().unwrap_or("default"),
                    ),
                    "",
                ));
            }
        }
    }

    // 3. Model API key present in the environment (config-only check;
    //    the network reachability probe is opt-in via `doctor --ping`).
    eprintln!("doctor: model");
    if let Some(m) = cfg.as_ref().and_then(|c| c.model.clone()) {
        match &m.api_key_env {
            Some(env) => match std::env::var(env) {
                Ok(v) if !v.is_empty() => {
                    checks.push(check("model", "ok", format!("{env} is set"), ""))
                }
                _ => checks.push(check(
                    "model",
                    "fail",
                    format!("{env} is not set in this shell"),
                    format!("export {env}=<key>"),
                )),
            },
            None => checks.push(check(
                "model",
                "ok",
                format!("no API key required for {}", m.provider),
                "",
            )),
        }
    } else {
        // Without a [model] section there is nothing to check above, and the
        // user gets no model line at all. Name the gap explicitly.
        checks.push(check(
            "model",
            "fail",
            "no [model] section, so no provider or model is configured",
            "run `pantheon setup`",
        ));
    }

    // 4. Memory backend: the ledger must open and the store must list.
    eprintln!("doctor: storage");
    let ledger_path = data_dir.join("ledger.db");
    match pantheon_storage::Ledger::open(&ledger_path) {
        Ok(ledger) => {
            let seq = ledger.max_seq().unwrap_or(-1);
            checks.push(check(
                "ledger",
                "ok",
                format!("opens fine (max seq {seq})"),
                "",
            ));
        }
        Err(e) => checks.push(check(
            "ledger",
            "fail",
            e.cause.clone(),
            "check disk space and permissions",
        )),
    }
    let mem_path = data_dir.join("memory.db");
    match pantheon_memory::MemoryStore::open(&mem_path) {
        Ok(_) => checks.push(check("memory", "ok", "store opens", "")),
        Err(e) => checks.push(check(
            "memory",
            "fail",
            e.cause.clone(),
            "check disk space and permissions",
        )),
    }

    // 5. Computer use: the CUA driver behind the ComputerUse group. Off
    eprintln!("doctor: computer-use");
    // is a legitimate configuration; on-but-missing is a warn with the
    // install one-liner, never a silent dead toggle.
    if let Some(c) = cfg.as_ref() {
        let group_on = c
            .tools
            .as_ref()
            .and_then(|t| t.computer_use)
            .unwrap_or(true);
        if !group_on {
            checks.push(check("computer-use", "ok", "group off", ""));
        } else {
            let section = c.computer_use.clone().unwrap_or_default();
            let binary = section.binary.clone();
            let found = match binary.as_deref() {
                Some(b) => std::path::Path::new(b).is_file().then(|| b.to_string()),
                None => std::env::var_os("PATH").and_then(|paths| {
                    std::env::split_paths(&paths).find_map(|dir| {
                        let p = dir.join("cua-driver");
                        p.is_file().then(|| p.to_string_lossy().into_owned())
                    })
                }),
            };
            match found {
                Some(p) => checks.push(check(
                    "computer-use",
                    "ok",
                    format!("cua-driver at {p}"),
                    "",
                )),
                None => checks.push(check(
                    "computer-use",
                    "warn",
                    "cua-driver not found on PATH",
                    "install: /bin/bash -c \"$(curl -fsSL https://cua.ai/driver/install.sh)\"",
                )),
            }
        }
    }

    // 5b. Skill dependencies: third-party packages the skill library
    eprintln!("doctor: skill-deps");
    // needs. A missing dep is a warn, never a fail - a skill without
    // its package degrades or falls back, but preflight must not stop
    // the run. A `[skill_deps].skipped` entry that is present now is
    // reported as healed, so the record visibly converges.
    {
        let detect = &crate::setup_providers::detect_binary;
        let pip_ok = crate::skill_deps::pip_ready(detect);
        let skipped: &[String] = cfg
            .as_ref()
            .and_then(|c| c.skill_deps.as_ref())
            .map(|s| s.skipped.as_slice())
            .unwrap_or(&[]);
        for dep in crate::skill_deps::skill_deps() {
            if detect(&dep.detect_cmd) {
                if skipped.iter().any(|s| s == dep.id) {
                    checks.push(check(
                        "skill-deps",
                        "ok",
                        format!("{} installed since setup", dep.name),
                        "",
                    ));
                }
                continue;
            }
            checks.push(check(
                "skill-deps",
                "warn",
                format!("{} not found - needed by {}", dep.name, dep.needed_by),
                skill_dep_fix(&dep, pip_ok),
            ));
        }
        // Stale record hygiene: a skipped id the registry no longer
        // knows (renamed or removed) is named, not silently kept.
        for id in skipped {
            if crate::skill_deps::find_dep(id).is_none() {
                checks.push(check(
                    "skill-deps",
                    "warn",
                    format!("unknown skill dependency {id:?} in [skill_deps].skipped"),
                    "remove it from the config or rerun `pantheon setup`",
                ));
            }
        }
    }

    // 6. Skills: a broken SKILL.md is dropped silently by discovery, so
    eprintln!("doctor: skills");
    // count what parsed and say so. Surface the roots, not every skill body
    // (that is what `skills list` is for).
    match crate::skills::scan_summary() {
        Ok(s) => checks.push(check(
            "skills",
            if s.broken > 0 { "warn" } else { "ok" },
            s.detail(),
            if s.broken > 0 {
                "run `pantheon skills doctor` for the broken ones"
            } else {
                ""
            },
        )),
        Err(e) => checks.push(check(
            "skills",
            "fail",
            e.cause,
            "check permissions on the skill roots",
        )),
    }

    // 7. Gateway: no token means the surface is simply off, which is a
    eprintln!("doctor: gateway");
    // legitimate configuration, not a fault. Name the env var so the user
    // knows the one line that would enable it.
    let mut live = Vec::new();
    if std::env::var("PANTHEON_DISCORD_TOKEN").is_ok_and(|t| !t.is_empty()) {
        live.push("discord");
    }
    if std::env::var("PANTHEON_TELEGRAM_BOT_TOKEN").is_ok_and(|t| !t.is_empty()) {
        live.push("telegram");
    }
    checks.push(check(
        "gateway",
        if live.is_empty() { "warn" } else { "ok" },
        if live.is_empty() {
            "no channel tokens set; gateway is idle".to_string()
        } else {
            format!("{} surface(s) enabled: {}", live.len(), live.join(", "))
        },
        if live.is_empty() {
            "set PANTHEON_DISCORD_TOKEN and/or PANTHEON_TELEGRAM_BOT_TOKEN to run `pantheon gateway`"
        } else {
            ""
        },
    ));

    // 7. Plugins: run the extension doctor over the extension dir.
    eprintln!("doctor: plugins");
    let ext_dir = crate::terminal::ext_dir();
    match std::fs::read_dir(&ext_dir) {
        Ok(entries) => {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            if dirs.is_empty() {
                checks.push(check("plugins", "ok", "no plugins installed", ""));
            }
            for d in dirs {
                let rep = pantheon_extensions::doctor::doctor(&d);
                let status = if !rep.ok {
                    "fail"
                } else if rep.unknown_hooks.is_empty() {
                    "ok"
                } else {
                    "warn"
                };
                let detail = if rep.findings.is_empty() {
                    format!("{} verified", rep.plugin)
                } else {
                    rep.findings
                        .iter()
                        .map(|x| format!("{}: {}", x.code, x.detail))
                        .collect::<Vec<_>>()
                        .join("; ")
                };
                checks.push(check(
                    "plugins",
                    status,
                    detail,
                    "reinstall the plugin or fix its manifest",
                ));
            }
        }
        // A missing directory is an empty plugin set, not a failure, and it
        // must not short-circuit the sections below it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            checks.push(check("plugins", "ok", "no plugins installed", ""));
        }
        Err(e) => {
            checks.push(check(
                "plugins",
                "fail",
                format!("cannot read {}: {e}", ext_dir.display()),
                "check permissions on the extension dir",
            ));
        }
    }

    // 8. Sandbox: the shell tool's default SandboxLevel::High runs through
    //    a Container boundary (bwrap). Without bwrap the runner fails
    //    closed (SANDBOX_UNAVAILABLE), so a missing binary is a warn with
    //    the fix, never silent.
    eprintln!("doctor: sandbox");
    if crate::setup_providers::detect_binary("command -v bwrap") {
        checks.push(check(
            "sandbox",
            "ok",
            "bwrap present (container sandbox for High-level exec)",
            "",
        ));
    } else {
        checks.push(check(
            "sandbox",
            "warn",
            "bwrap not found on PATH - the shell tool's High sandbox fails closed without it",
            "install bubblewrap (e.g. `apt install bubblewrap`), or set PANTHEON_SANDBOX_FALLBACK=allow to opt into the direct-spawn fallback",
        ));
    }

    // 9. Reachability: opt-in via `doctor --ping`. The config-only
    //    key-presence check cannot see a dead endpoint, a wrong base URL,
    //    or a proxy that swallows the route, so this is a plain TCP
    //    connect to the provider's base URL (5s), reported as its own
    //    check row rather than hidden inside the model section.
    if ping {
        eprintln!("doctor: ping");
        match cfg.as_ref().and_then(|c| c.model.clone()) {
            Some(m) => match pantheon_providers::catalog::resolve_base_url(&m.provider) {
                Ok(url) => {
                    if tcp_probe(&url, std::time::Duration::from_secs(5)) {
                        checks.push(check(
                            "ping",
                            "ok",
                            format!("{} reachable at {url}", m.provider),
                            "",
                        ));
                    } else {
                        checks.push(check(
                            "ping",
                            "fail",
                            format!("{} not reachable at {url}", m.provider),
                            format!(
                                "check the endpoint is up and reachable from this host (base URL for {:?})",
                                m.provider
                            ),
                        ));
                    }
                }
                Err(e) => checks.push(check(
                    "ping",
                    "warn",
                    format!("cannot resolve a base URL for {:?}: {e}", m.provider),
                    "run `pantheon model` to fix the provider entry",
                )),
            },
            None => checks.push(check(
                "ping",
                "warn",
                "no [model] section, nothing to ping",
                "run `pantheon setup`",
            )),
        }
    }

    finish(checks)
}

/// Single exit point for the section list so the `ok` verdict can never be
/// computed at one return site and missed at another.
fn finish(checks: Vec<Check>) -> SystemReport {
    let ok = !checks.iter().any(|c| c.status == "fail");
    SystemReport { ok, checks }
}

/// Split an `http(s)://host[:port][/...]` URL into `(host, port)`.
/// Default ports follow the scheme. Returns `None` for anything that
/// does not look like an HTTP URL - the probe treats that as
/// unreachable rather than guessing.
fn host_port(url: &str) -> Option<(String, u16)> {
    let after = url.split("://").nth(1)?;
    let hostport = after.split('/').next()?;
    // Strip userinfo if present.
    let hostport = hostport.rsplit('@').next()?;
    // Strip a bracketed IPv6's zone id? Keep it simple: rsplit on ':'.
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
/// accepts a TCP connection within `timeout`. Used by `doctor --ping`
/// and by the session entry point for the local (Ollama) provider.
pub fn tcp_probe(url: &str, timeout: std::time::Duration) -> bool {
    use std::net::ToSocketAddrs;
    let (host, port) = match host_port(url) {
        Some(hp) => hp,
        None => return false,
    };
    match (host.as_str(), port).to_socket_addrs() {
        Ok(mut addrs) => addrs.any(|a| std::net::TcpStream::connect_timeout(&a, timeout).is_ok()),
        Err(_) => false,
    }
}

/// Render a [`SystemReport`] for a human terminal: one line per check
/// with its fix hint indented below, plus a one-line verdict. This is
/// the default `pantheon doctor` output; `--json` keeps the old
/// machine-readable form.
pub fn render_human(report: &SystemReport) -> String {
    let mut out = String::new();
    let (mut oks, mut warns, mut fails) = (0, 0, 0);
    for c in &report.checks {
        match c.status.as_str() {
            "ok" => oks += 1,
            "warn" => warns += 1,
            _ => fails += 1,
        }
    }
    let verdict = if report.ok { "pass" } else { "FAIL" };
    out.push_str(&format!(
        "pantheon doctor: {verdict} - {oks} ok, {warns} warn, {fails} fail\n\n"
    ));
    for c in &report.checks {
        let glyph = match c.status.as_str() {
            "ok" => "ok  ",
            "warn" => "warn",
            _ => "FAIL",
        };
        out.push_str(&format!("[{glyph}] {:<12} {}\n", c.section, c.detail));
        if !c.fix.is_empty() {
            out.push_str(&format!("                 fix: {}\n", c.fix));
        }
    }
    out
}

/// Human rendering for the per-plugin doctor form
/// (`pantheon doctor <plugin_dir>`): verdict, findings, unknown hooks.
pub fn render_plugin_doctor_human(rep: &pantheon_extensions::doctor::DoctorReport) -> String {
    let mut out = String::new();
    let verdict = if rep.ok { "pass" } else { "FAIL" };
    out.push_str(&format!("pantheon doctor {}: {verdict}\n", rep.plugin));
    if rep.findings.is_empty() {
        out.push_str("verified: no findings\n");
    }
    for f in &rep.findings {
        out.push_str(&format!("- {}: {}\n", f.code, f.detail));
    }
    if !rep.unknown_hooks.is_empty() {
        out.push_str(&format!(
            "unknown hooks: {}\n",
            rep.unknown_hooks.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod doctor_fix_pass4_tests {
    use super::*;

    #[test]
    fn render_human_counts_and_shows_fixes() {
        let report = SystemReport {
            ok: false,
            checks: vec![
                check("config", "ok", "parsed", ""),
                check("model", "fail", "key missing", "export K=<key>"),
                check("sandbox", "warn", "no bwrap", "apt install bubblewrap"),
            ],
        };
        let text = render_human(&report);
        assert!(text.contains("FAIL"), "verdict missing: {text}");
        assert!(
            text.contains("1 ok, 1 warn, 1 fail"),
            "counts wrong: {text}"
        );
        assert!(
            text.contains("fix: export K=<key>"),
            "fix hint missing: {text}"
        );
    }

    #[test]
    fn tcp_probe_rejects_garbage_and_refused_ports() {
        // Nothing listens on 1; the probe must answer fast, not hang.
        let start = std::time::Instant::now();
        assert!(!tcp_probe(
            "http://127.0.0.1:1/v1",
            std::time::Duration::from_secs(3)
        ));
        assert!(!tcp_probe("not a url", std::time::Duration::from_secs(3)));
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn tcp_probe_sees_a_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        assert!(tcp_probe(
            &format!("http://127.0.0.1:{port}/v1"),
            std::time::Duration::from_secs(3)
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
}
