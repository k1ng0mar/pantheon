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

/// Run every doctor check against a data dir.
pub fn run_system_doctor(data_dir: &Path) -> SystemReport {
    let mut checks = Vec::new();

    // 1. Config file: parse + validate.
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

    // 2. Agent identities: visible by effective display name so a
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

    // 3. Model API key present in the environment (config-only check; the
    //    network reachability probe is opt-in via --ping).
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

    // 5. Skills: a broken SKILL.md is dropped silently by discovery, so
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

    // 6. Gateway: no token means the surface is simply off, which is a
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
    let ext_dir = crate::terminal::ext_dir();
    let entries = match std::fs::read_dir(&ext_dir) {
        Ok(entries) => entries,
        // A missing directory is an empty plugin set, not a failure, and it
        // must not short-circuit the sections below it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            checks.push(check("plugins", "ok", "no plugins installed", ""));
            return finish(checks);
        }
        Err(e) => {
            checks.push(check(
                "plugins",
                "fail",
                format!("cannot read {}: {e}", ext_dir.display()),
                "check permissions on the extension dir",
            ));
            return finish(checks);
        }
    };
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

    let ok = !checks.iter().any(|c| c.status == "fail");
    SystemReport { ok, checks }
}

/// Single exit point for the section list so the `ok` verdict can never be
/// computed at one return site and missed at another.
fn finish(checks: Vec<Check>) -> SystemReport {
    let ok = !checks.iter().any(|c| c.status == "fail");
    SystemReport { ok, checks }
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
