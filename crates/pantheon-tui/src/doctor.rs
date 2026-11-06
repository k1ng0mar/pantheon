//! `pantheon doctor`: system diagnostics.
//!
//! The diagnostics engine itself lives in `pantheon-doctor` (findings,
//! registry, checks, renderers, health contract) so the CLI and
//! `GET /api/health` share one pipeline. This module owns the CLI
//! surface: argument parsing, the plugin-dir form, progress wiring,
//! and exit codes.
//!
//! The unattended path (bare `pantheon` repair, in-session `/doctor`) uses
//! [`actionable_findings`], which returns only the findings a human has to
//! act on; everything else goes through `pantheon_doctor` directly.

use pantheon_doctor::finding::Subsystem;
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
                // A second positional used to vanish silently, and a
                // plugin dir silently dropped `--check` (the plugin
                // form runs before the subsystem engine). Both are
                // argument errors now: the plugin form takes exactly
                // one directory and no `--check`.
                if plugin_dir.is_some() {
                    fail_usage(&format!(
                        "unexpected extra argument {s:?}; doctor takes at most one plugin directory"
                    ));
                }
                plugin_dir = Some(s);
            }
        }
    }
    if check_arg_pending {
        fail_usage("--check needs a subsystem argument");
    }
    if plugin_dir.is_some() && check.is_some() {
        fail_usage("--check does not apply to the plugin-directory form; run it without --check, or without a plugin directory");
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
// Unattended system check for the bare-`pantheon` entry and `/doctor`.
// ---------------------------------------------------------------------------

/// One failed-or-warning finding, flattened for the two surfaces that print
/// a short human list instead of the full report: the bare-`pantheon` repair
/// path (config does not parse) and the in-session `/doctor` command.
pub struct ActionableFinding {
    /// Subsystem id, e.g. `config`.
    pub section: String,
    /// `"warn"` or `"fail"`; passes and skips are never returned.
    pub status: &'static str,
    pub detail: String,
    pub fix: String,
}

/// Run the core doctor registry and return only the findings a human has to
/// act on, plus overall health.
///
/// This replaces the deprecated `run_system_doctor`/`run_system_doctor_opts`
/// pair, which adapted every finding into a legacy `Check` list that callers
/// then filtered anyway. Callers that need the full report (including
/// passes, timings and the JSON schema) should call
/// [`pantheon_doctor::Registry`] directly, as `pantheon doctor` does.
pub fn actionable_findings(data_dir: &Path, ping: bool) -> (bool, Vec<ActionableFinding>) {
    let ext_dir = match std::env::var("PANTHEON_EXT_DIR") {
        Ok(d) if !d.trim().is_empty() => Path::new(&d).to_path_buf(),
        _ => data_dir.join("extensions"),
    };
    let mut ctx = DoctorCtx::new(data_dir.to_path_buf(), ext_dir);
    ctx.ping_provider = ping;
    let report = pantheon_doctor::Registry::core().run(&ctx, None);
    let findings = report
        .findings
        .iter()
        .filter_map(|f| match f.status {
            // Passes and skips are not actionable. A `NotApplicable` check
            // (e.g. a unix-only permission check on Windows) must not read
            // as a problem the user can fix.
            pantheon_doctor::CheckStatus::Passed
            | pantheon_doctor::CheckStatus::Skipped
            | pantheon_doctor::CheckStatus::NotApplicable => None,
            pantheon_doctor::CheckStatus::Failed => Some(ActionableFinding {
                section: f.subsystem.id().to_string(),
                status: match f.severity {
                    pantheon_doctor::Severity::Warning => "warn",
                    _ => "fail",
                },
                detail: f.summary.clone(),
                fix: f.remediation.clone(),
            }),
        })
        .collect();
    (
        report.overall == pantheon_doctor::OverallHealth::Healthy,
        findings,
    )
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
    fn actionable_findings_warns_and_fails_only() {
        // An unparseable config is a critical finding: it must come back as
        // "fail" and carry its remediation, and healthy checks must not be
        // returned at all.
        let dir = std::env::temp_dir().join("pantheon-doctor-actionable-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("config.toml"), "[[[not toml").expect("seed");
        let (ok, findings) = actionable_findings(&dir, false);
        assert!(!ok, "a broken config must not report healthy");
        let file = findings
            .iter()
            .find(|c| c.detail.contains("does not parse"))
            .expect("config.file finding is actionable");
        assert_eq!(file.status, "fail");
        assert_eq!(file.section, "config");
        assert!(
            findings
                .iter()
                .all(|c| c.status == "warn" || c.status == "fail"),
            "passing checks must not be returned as actionable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
