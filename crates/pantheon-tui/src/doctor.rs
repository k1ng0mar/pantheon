//! `pantheon doctor`: system diagnostics.
//!
//! The diagnostics engine itself lives in `pantheon-doctor` (findings,
//! registry, checks, renderers, health contract) so the CLI and
//! `GET /api/health` share one pipeline. This module owns the CLI
//! surface: argument parsing, the plugin-dir form, progress wiring,
//! and exit codes.
//!
//! The legacy [`Check`]/[`SystemReport`] types below are a deprecated
//! compatibility shim for the bare-`pantheon` repair path in
//! [`crate::entry`]; new code should use `pantheon_doctor` directly.

use pantheon_doctor::finding::{OverallHealth, Subsystem};
use pantheon_doctor::registry::DoctorCtx;
use std::path::Path;

// Re-exported so `crate::entry` keeps working unchanged.
pub use pantheon_doctor::probe::tcp_probe;

// ---------------------------------------------------------------------------
// New CLI: `pantheon doctor [--ping] [--verbose] [--json] [--check <s>]
// ---------------------------------------------------------------------------

/// Print the doctor usage to stderr.
pub fn doctor_help() {
    eprintln!(
        "usage: pantheon doctor [--ping] [--verbose] [--json] [--check <subsystem>] [<plugin_dir>]"
    );
    eprintln!("  no args:      system diagnostics (concise: failures and warnings only)");
    eprintln!("  --verbose:    show every check, with evidence and timings");
    eprintln!("  --json:       machine-readable JSON on stdout (progress goes to stderr)");
    eprintln!("  --check <s>:  run one subsystem only");
    eprintln!("                runtime | providers | memory | tools | security | config");
    eprintln!("  --ping:       also probe the configured model provider's endpoint");
    eprintln!("  <plugin_dir>: verify one plugin directory (extension doctor)");
    eprintln!(
        "exit codes: 0 healthy, 1 degraded/unhealthy, 2 bad arguments, 3 diagnostics incomplete"
    );
}

fn fail_usage(msg: &str) -> ! {
    eprintln!("doctor: {msg}");
    doctor_help();
    std::process::exit(2);
}

/// `pantheon doctor` argv dispatch. Never returns.
pub fn cmd_doctor(args: &[String], data_dir: &Path) -> ! {
    let mut ping = false;
    let mut json = false;
    let mut verbose = false;
    let mut check: Option<Subsystem> = None;
    let mut plugin_dir: Option<&str> = None;
    let mut check_arg_pending = false;

    for a in args.iter().skip(2) {
        if check_arg_pending {
            match Subsystem::parse(a) {
                Some(s) => check = Some(s),
                None => fail_usage(&format!(
                    "unknown subsystem {a:?}; expected one of: runtime, providers, memory, tools, security, config"
                )),
            }
            check_arg_pending = false;
            continue;
        }
        match a.as_str() {
            "--help" | "-h" => {
                doctor_help();
                std::process::exit(0);
            }
            "--ping" => ping = true,
            "--json" => json = true,
            "--verbose" => verbose = true,
            // Back-compat alias: human-readable concise output is the default.
            "--human" => {}
            "--check" => check_arg_pending = true,
            s if s.starts_with('-') => {
                fail_usage(&format!("unexpected flag {s:?}"));
            }
            s => {
                if plugin_dir.is_none() {
                    plugin_dir = Some(s);
                }
            }
        }
    }
    if check_arg_pending {
        fail_usage("--check needs a subsystem argument");
    }
    if json && verbose {
        fail_usage("--json and --verbose are mutually exclusive");
    }

    // Plugin-dir form: keep the original extension doctor.
    if let Some(dir) = plugin_dir {
        let rep = pantheon_extensions::doctor::doctor(Path::new(dir));
        if json {
            super::terminal::print_json("doctor", &rep);
        } else {
            print!("{}", render_plugin_doctor_human(&rep));
        }
        std::process::exit(if rep.ok { 0 } else { 1 });
    }

    // System diagnostics via the shared engine.
    let ext_dir = match std::env::var("PANTHEON_EXT_DIR") {
        Ok(d) if !d.trim().is_empty() => Path::new(&d).to_path_buf(),
        _ => data_dir.join("extensions"),
    };
    let mut ctx = DoctorCtx::new(data_dir.to_path_buf(), ext_dir);
    ctx.ping_provider = ping;
    // Progress goes to stderr so `--json` stdout stays machine-readable.
    ctx.progress = Some(Box::new(|s| eprintln!("doctor: {s}")));

    let registry = pantheon_doctor::Registry::core();
    let report = registry.run(&ctx, check);

    if json {
        match pantheon_doctor::render_json(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("doctor: {e}");
                std::process::exit(3);
            }
        }
    } else if verbose {
        print!("{}", pantheon_doctor::render_verbose(&report));
    } else {
        print!("{}", pantheon_doctor::render_concise(&report));
    }
    std::process::exit(pantheon_doctor::exit_code(&report));
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

// ---------------------------------------------------------------------------
// Deprecated compatibility shim for `crate::entry`'s repair path.
// ---------------------------------------------------------------------------

/// Legacy check shape. Deprecated: use `pantheon_doctor::Finding`.
#[derive(Debug, Clone)]
pub struct Check {
    pub section: String,
    pub status: String, // "ok" | "warn" | "fail"
    pub detail: String,
    pub fix: String,
}

/// Legacy report shape. Deprecated: use `pantheon_doctor::DiagnosticReport`.
#[derive(Debug, Clone)]
pub struct SystemReport {
    pub ok: bool,
    pub checks: Vec<Check>,
}

/// Deprecated: runs the new engine and adapts the report to the legacy
/// shape. Kept for the bare-`pantheon` repair path; the CLI uses the
/// engine directly.
#[deprecated(note = "use pantheon_doctor::Registry instead")]
pub fn run_system_doctor(data_dir: &Path) -> SystemReport {
    #[allow(deprecated)]
    run_system_doctor_opts(data_dir, false)
}

/// Deprecated: see [`run_system_doctor`].
#[deprecated(note = "use pantheon_doctor::Registry instead")]
pub fn run_system_doctor_opts(data_dir: &Path, ping: bool) -> SystemReport {
    let ext_dir = match std::env::var("PANTHEON_EXT_DIR") {
        Ok(d) if !d.trim().is_empty() => Path::new(&d).to_path_buf(),
        _ => data_dir.join("extensions"),
    };
    let mut ctx = DoctorCtx::new(data_dir.to_path_buf(), ext_dir);
    ctx.ping_provider = ping;
    let report = pantheon_doctor::Registry::core().run(&ctx, None);
    let checks = report
        .findings
        .iter()
        .map(|f| Check {
            section: f.subsystem.id().to_string(),
            status: match f.status {
                pantheon_doctor::CheckStatus::Passed => "ok".to_string(),
                pantheon_doctor::CheckStatus::Skipped
                | pantheon_doctor::CheckStatus::NotApplicable => "ok".to_string(),
                pantheon_doctor::CheckStatus::Failed => match f.severity {
                    pantheon_doctor::Severity::Warning => "warn".to_string(),
                    _ => "fail".to_string(),
                },
            },
            detail: f.summary.clone(),
            fix: f.remediation.clone(),
        })
        .collect();
    SystemReport {
        ok: report.overall == OverallHealth::Healthy,
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_doctor_human_renders_verdict_and_findings() {
        let rep = pantheon_extensions::doctor::DoctorReport {
            plugin: "demo".to_string(),
            ok: false,
            hooks: vec![],
            unknown_hooks: vec!["weird_hook".to_string()],
            findings: vec![pantheon_extensions::doctor::DoctorFinding {
                level: "error".to_string(),
                code: "NO_MANIFEST".to_string(),
                detail: "plugin.yaml missing".to_string(),
            }],
        };
        let text = render_plugin_doctor_human(&rep);
        assert!(text.contains("FAIL"), "{text}");
        assert!(text.contains("NO_MANIFEST"), "{text}");
        assert!(text.contains("weird_hook"), "{text}");
    }

    #[test]
    #[allow(deprecated)]
    fn legacy_shim_preserves_tristate_mapping() {
        // entry.rs and the /doctor slash command still consume the
        // legacy shape: failed+warning findings must surface as "warn",
        // failed+error/critical as "fail".
        let dir = std::env::temp_dir().join("pantheon-doctor-shim-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        // Unparseable config => config.file is critical => "fail".
        std::fs::write(dir.join("config.toml"), "[[[not toml").expect("seed");
        let report = run_system_doctor(&dir);
        assert!(!report.ok);
        let file = report
            .checks
            .iter()
            .find(|c| c.detail.contains("does not parse"))
            .expect("config.file finding");
        assert_eq!(file.status, "fail");
        assert_eq!(file.section, "config");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
