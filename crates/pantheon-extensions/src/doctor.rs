//! skill doctor: loud preflight for any plugin dir. Mirrors the OpenClaw
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
        findings.push(f(
            "warn",
            "TS_ENTRY",
            "TypeScript entry: needs OpenClaw-compat adapter (Soul path), not the Python runner"
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
