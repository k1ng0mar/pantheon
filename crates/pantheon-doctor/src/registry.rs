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
            impact: "this diagnostic could not complete; its subject is unevaluated".to_string(),
            remediation: "rerun `pantheon doctor`; if it persists, report the check id".to_string(),
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
            impact: "this diagnostic could not complete; its subject is unevaluated".to_string(),
            remediation: "rerun `pantheon doctor`; if it persists, report the check id".to_string(),
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
