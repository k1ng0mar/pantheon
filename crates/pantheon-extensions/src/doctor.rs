//! skill doctor: loud preflight for any plugin dir. Targets the classic
//! silent-death complaints: missing env, missing bins, bad manifest,
//! missing entry, dangerous hooks. Non-zero problems => loud report.
use crate::hooks::Hook;
use crate::manifest::PluginManifest;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorFinding {
    pub level: String,
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub plugin: String,
    pub ok: bool,
    pub hooks: Vec<String>,
    pub unknown_hooks: Vec<String>,
    pub findings: Vec<DoctorFinding>,
}

fn f(level: &str, code: &str, detail: String) -> DoctorFinding {
    DoctorFinding {
        level: level.into(),
        code: code.into(),
        detail,
    }
}

/// Inspect a plugin dir without executing it.
pub fn doctor(dir: &Path) -> DoctorReport {
    let mut findings = Vec::new();
    let label = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.display().to_string());
    if !dir.exists() {
        return DoctorReport {
            plugin: label,
            ok: false,
            hooks: vec![],
            unknown_hooks: vec![],
            findings: vec![f(
                "error",
                "NO_DIR",
                format!("{} does not exist", dir.display()),
            )],
        };
    }
    let man_path = dir.join("plugin.yaml");
    if !man_path.exists() {
        findings.push(f("error", "NO_MANIFEST", "plugin.yaml missing".into()));
        return DoctorReport {
            plugin: label,
            ok: false,
            hooks: vec![],
            unknown_hooks: vec![],
            findings,
        };
    }
    let man = match PluginManifest::load(&man_path) {
        Ok(m) => m,
        Err(e) => {
            findings.push(f("error", "BAD_MANIFEST", e.to_string()));
            return DoctorReport {
                plugin: label,
                ok: false,
                hooks: vec![],
                unknown_hooks: vec![],
                findings,
            };
        }
    };
    if man.name.trim().is_empty() {
        findings.push(f("error", "NO_NAME", "manifest has empty name".into()));
    }
    let entry_py = dir.join("__init__.py").exists();
    let entry_ts = dir.join("index.ts").exists();
    if !entry_py && !entry_ts {
        findings.push(f(
            "error",
            "NO_ENTRY",
            "neither __init__.py nor index.ts found".into(),
        ));
    }
    if entry_ts && !entry_py {
        // Error, not a warning: with no __init__.py the Python runner
        // can never run this plugin, so a passing verdict (ok=true,
        // exit 0) lied about a plugin that cannot execute. A TS entry
        // alongside __init__.py stays a warning: the Python entry runs.
        findings.push(f(
            "error",
            "TS_ENTRY",
            "TypeScript-only entry: needs an OpenClaw-compat adapter (Soul path); the Python runner cannot run this plugin"
                .into(),
        ));
    }
    let (hooks, unknown) = man.hook_list();
    for u in &unknown {
        findings.push(f("warn", "UNKNOWN_HOOK", format!("unknown hook '{u}'")));
    }
    // A hook Pantheon knows but cannot fire is the dangerous case: the
    // plugin author believes it runs. Say so loudly rather than letting the
    // manifest look healthy.
    for h in &hooks {
        if !h.is_wired() {
            findings.push(f(
                "warn",
                "UNWIRED_HOOK",
                format!(
                    "'{}' is a known hook with no fire site yet; this plugin's handler will not run",
                    h.name()
                ),
            ));
        }
    }
    if hooks.is_empty() {
        findings.push(f(
            "warn",
            "NO_HOOKS",
            "manifest declares no known hooks".into(),
        ));
    }
    let names: Vec<String> = hooks.iter().map(|h| h.name().to_string()).collect();
    // Static scan of __init__.py for suspicious imports (loud, not blocking).
    if entry_py {
        if let Ok(text) = std::fs::read_to_string(dir.join("__init__.py")) {
            for pat in ["os.system", "subprocess", "socket", "urllib", "requests"] {
                if text.contains(pat) {
                    findings.push(f(
                        "info",
                        "USES_NET_OR_EXEC",
                        format!(
                            "__init__.py mentions '{pat}': plugins are NOT sandboxed and run \
                             with your full privileges - approve only what you trust"
                        ),
                    ));
                }
            }
        }
    }
    // Approval state: a third-party plugin that is not approved will not
    // load, so say so here instead of letting the manifest look healthy.
    if let Some(ext_dir) = dir.parent() {
        if !pantheon_api::approval::is_bundled(ext_dir, dir) && !man.name.trim().is_empty() {
            let approved = pantheon_api::approval::dir_hash(dir)
                .map(|h| pantheon_api::approval::is_approved(ext_dir, &man.name, &man.version, &h))
                .unwrap_or(false);
            if !approved {
                findings.push(f(
                    "warn",
                    "NOT_APPROVED",
                    format!(
                        "'{}' is third-party and not approved; it will not load until you run \
                         `pantheon extensions approve {}`",
                        man.name, man.name
                    ),
                ));
            }
        }
    }
    let ok = !findings.iter().any(|x| x.level == "error");
    let _ = Hook::all();
    DoctorReport {
        plugin: man.name.clone(),
        ok,
        hooks: names,
        unknown_hooks: unknown,
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn plugin_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pantheon-doctor-ts-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("plugin.yaml"), "name: ts-only\nversion: \"1\"\n").unwrap();
        dir
    }

    #[test]
    fn typescript_only_entry_is_an_error_not_a_pass() {
        let dir = plugin_dir("only");
        fs::write(dir.join("index.ts"), "export {}").unwrap();
        let report = doctor(&dir);
        assert!(
            !report.ok,
            "a plugin the Python runner can never run must not pass"
        );
        assert!(
            report
                .findings
                .iter()
                .any(|x| x.level == "error" && x.code == "TS_ENTRY"),
            "findings: {:?}",
            report.findings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn typescript_alongside_python_entry_draws_no_ts_finding() {
        let dir = plugin_dir("both");
        fs::write(dir.join("index.ts"), "export {}").unwrap();
        fs::write(dir.join("__init__.py"), "").unwrap();
        let report = doctor(&dir);
        assert!(
            !report.findings.iter().any(|x| x.code == "TS_ENTRY"),
            "findings: {:?}",
            report.findings
        );
        assert!(report.ok);
        let _ = fs::remove_dir_all(&dir);
    }
}
