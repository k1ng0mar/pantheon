//! `pantheon doctor`: system-level preflight.
//!
//! Sections: config validation, model reachability, memory backend health,
//! plugin verification (reuses the extension doctor), data-dir integrity.
//! Every check reports pass/warn/fail with a fix hint. Exit code is 0 only
//! when nothing failed.

use crate::config_doc::Config;
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
                for p in problems {
                    checks.push(check(
                        "config",
                        "warn",
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

    // 2. Model API key present in the environment (config-only check; the
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
                "no API key configured (local provider?)",
                "",
            )),
        }
    }

    // 3. Memory backend: the ledger must open and the store must list.
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

    // 4. Plugins: run the extension doctor over the extension dir.
    let ext_dir = crate::ext_dir();
    if ext_dir.exists() {
        let entries = std::fs::read_dir(&ext_dir).unwrap_or_else(|_| unreachable!());
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
    } else {
        checks.push(check(
            "plugins",
            "ok",
            format!("no extension dir at {}", ext_dir.display()),
            "",
        ));
    }

    let ok = !checks.iter().any(|c| c.status == "fail");
    SystemReport { ok, checks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_data_dir_fails_config_but_reports_the_fix() {
        let dir = std::env::temp_dir().join(format!("pantheon-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rep = run_system_doctor(&dir);
        assert!(!rep.ok);
        let cfg_check = rep.checks.iter().find(|c| c.section == "config").unwrap();
        assert_eq!(cfg_check.status, "fail");
        assert!(cfg_check.fix.contains("setup"));
        // Ledger and memory still get checked even without config.
        assert!(rep.checks.iter().any(|c| c.section == "ledger"));
        assert!(rep.checks.iter().any(|c| c.section == "memory"));
    }

    #[test]
    fn configured_data_dir_passes() {
        let dir = std::env::temp_dir().join(format!("pantheon-doctor-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::setup_cli::run_setup(
            &dir,
            crate::setup_cli::SetupAnswers {
                provider: Some("local".into()),
                model: Some("llama3.2".into()),
                ..Default::default()
            },
            true,
        );
        let rep = run_system_doctor(&dir);
        let cfg_check = rep.checks.iter().find(|c| c.section == "config").unwrap();
        assert_eq!(cfg_check.status, "ok");
        assert!(rep.ok, "checks: {:?}", rep.checks);
    }
}
