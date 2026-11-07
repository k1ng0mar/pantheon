//! heal_group: doctor, reflect_cli, nightly_cli, nightly_repair, repair, schedule_self_heal, schedule in one file.

pub mod doctor {
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
                crate::terminal::print_json("doctor", &rep);
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
}

pub mod reflect_cli {
    //! `pantheon reflect` - compatibility shim over the unified nightly pass.
    //!
    //! The old standalone reflection pipeline is merged into
    //! [`crate::nightly_cli`]: `pantheon reflect` now runs the full nightly
    //! pass (single ledger scan → proposals → eval/replay gates → approval
    //! queue → audit). Every function here keeps its old name and shape so
    //! the TUI, the scheduler, and scripts keep working; new code should call
    //! `nightly_cli` directly.

    use std::path::Path;

    /// Run one bounded pass. Backed by the unified nightly pipeline.
    pub fn run_one_pass(
        data_dir: &Path,
        dry_run: bool,
    ) -> Result<pantheon_nightly::PassResult, String> {
        crate::nightly_cli::run_one_pass(data_dir, dry_run)
    }

    /// Persist the nightly LLM toggle. Writes `[nightly] enabled`; the old
    /// `[reflect]` section is still read as a legacy fallback.
    pub fn persist_reflect_enabled(data_dir: &Path, enabled: bool) -> Result<(), String> {
        crate::nightly_cli::persist_nightly_enabled(data_dir, enabled)
    }

    /// Current toggle state: `(llm_enabled, auto_turns)`.
    pub fn reflect_state(data_dir: &Path) -> (bool, u32) {
        crate::nightly_cli::nightly_state(data_dir)
    }

    /// One-line status for `/reflect status` / `pantheon reflect status`.
    pub fn status_line(data_dir: &Path) -> String {
        crate::nightly_cli::status_line(data_dir)
    }

    /// Human summary of a finished pass for the CLI and the TUI status line.
    pub fn summarize_pass(out: &pantheon_nightly::PassResult) -> String {
        crate::nightly_cli::summarize_pass(out)
    }

    /// Render pending proposals for approval review.
    pub fn render_pending(pending: &[pantheon_nightly::Proposal]) -> String {
        crate::nightly_cli::render_pending(pending)
    }

    /// `pantheon reflect [--dry-run] [on|off|status|log|pending] [--approve ID] [--deny ID]`
    ///
    /// Compatibility alias for `pantheon nightly`: runs the unified pass.
    pub fn cmd_reflect(args: &[String], data_dir: &Path) {
        eprintln!("note: `pantheon reflect` is now part of the unified `pantheon nightly` pass");
        // Rewrite argv[1] so the nightly parser sees a `nightly` command.
        let mut nightly_args = args.to_vec();
        if nightly_args.len() > 1 {
            nightly_args[1] = "nightly".to_string();
        }
        crate::nightly_cli::cmd_nightly(&nightly_args, data_dir)
    }
}

pub mod nightly_cli {
    //! `pantheon nightly` and the TUI `/nightly` command.
    //!
    //! The unified self-improvement pass: one ledger scan feeds signal
    //! proposals (skill/persona/memory lesson) and memory promotion, with
    //! eval-gating, replay validation (strict improvement on held-out
    //! tasks), human approval for skills/personas, and a JSONL audit trail.
    //!
    //! All LLM-backed steps resolve through auxiliary slots - the pass's own
    //! `[nightly.model]` pin when configured, else proposal refinement
    //! through `AuxiliaryKind::Reflection` and memory distillation through
    //! `AuxiliaryKind::Consolidation` - never the chat model directly.
    //! `run_one_pass` builds the model policy from the on-disk config, so the
    //! CLI, the TUI worker thread, and the scheduler share one routing path.
    //!
    //! The pass is **off by default** and does nothing unless enabled: an
    //! explicit `enabled` flag wins, otherwise a `[nightly.model]` pin (or
    //! the `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env
    //! overrides) implies on - see [`pantheon_api::config::nightly_enabled`].
    //! With no `[nightly]` section, legacy `[reflect]` / `[consolidation]`
    //! With no `[nightly]` section, legacy `[reflect]` / `[consolidation]`
    //! opt-ins are honored as a deprecated fallback.
    //!
    //! The old `pantheon reflect` / `pantheon consolidate` commands remain as
    //! thin compatibility shims over this module.

    use pantheon_api::capability::Policy;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::model::{AuxiliaryKind, DefaultModel};
    use pantheon_nightly::{
        decide as nightly_decide, load_escalated, load_pending, run_pass, CompositeReplayRunner,
        Escalation, NightlyDeps, NightlyEvent, NightlyLlm, NightlyState, PassResult, Proposal,
        RepairTargets, ReplayRunner, ReplayStore, ReplayTask, SubprocessEvalRunner,
    };
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    struct NightlyUsageSink {
        data_dir: std::path::PathBuf,
    }

    impl pantheon_providers::model_event::AuxiliaryUsageSink for NightlyUsageSink {
        fn record(
            &self,
            provider: &str,
            model: &str,
            usage: pantheon_providers::model_event::ModelUsage,
        ) -> Result<(), PantheonError> {
            pantheon_nightly::audit(
                &self.data_dir,
                &NightlyEvent::ModelUsage {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    total_tokens: usage.total_tokens,
                    cost_usd: usage.cost_usd,
                    at_ms: pantheon_api::logging::now_ms(),
                },
            )
            .map_err(|cause| {
                PantheonError::new(
                    "NIGHTLY_USAGE_AUDIT",
                    Layer::Storage,
                    true,
                    cause,
                    "check that the nightly audit path is writable",
                    "",
                )
            })
        }
    }

    /// Run one bounded nightly pass.
    ///
    /// Builds the model policy from the on-disk config (so any LLM step
    /// resolves through the Reflection/Consolidation aux slots), eval-gates
    /// skill/persona proposals with the subprocess runner, replay-gates them
    /// against the held-out task set, queues memory lessons and the rest
    /// for approval (nothing is auto-applied by an unattended pass), and
    /// audits everything.
    pub fn run_one_pass(data_dir: &Path, dry_run: bool) -> Result<PassResult, String> {
        let file_cfg = crate::config::Config::load_or_report(data_dir);
        // Master switch: the pipeline and its repair loop do nothing unless
        // the pass is enabled (explicit flag, model pin, or legacy opt-in).
        // This is the single entry every trigger funnels through - the CLI,
        // the TUI worker thread, and the scheduler - so one gate covers all.
        if !crate::config::nightly_pass_enabled(file_cfg.as_ref()) {
            return Err("nightly pass is disabled - enable it with a [nightly.model] pin, `enabled = true` under [nightly], or `/nightly on`"
                .to_string());
        }
        let mut cfg = crate::config::nightly_config(file_cfg.as_ref(), data_dir);
        cfg.dry_run = dry_run;
        let policy = crate::config::build_model_policy(file_cfg.as_ref(), None, None);

        let ledger = pantheon_storage::Ledger::open(&data_dir.join("ledger.db"))
            .map_err(|e| format!("opening ledger: {e}"))?;
        let backend = pantheon_memory::open_selected(data_dir)
            .map_err(|e| format!("opening memory backend: {e}"))?;
        let mem_policy = Policy::coder_with_memory();

        let eval_runner = SubprocessEvalRunner::new(cfg.eval_timeout, cfg.max_evals);
        // One runner for every replay strategy: tasks with their own exec
        // spec run on the built-in headless runner; the rest use the
        // configured headless-agent command, or fail loudly when none is
        // set (the gate rejects - "couldn't measure" is never a pass).
        let replay_runner =
            CompositeReplayRunner::new(cfg.replay_command.clone(), Duration::from_secs(300));
        let replay_runner: &dyn ReplayRunner = &replay_runner;

        // One LLM client per aux slot, each slot-guarded: refinement calls
        // can only hit the Reflection target, distillation only the
        // Consolidation target.
        let secrets = crate::config::chat_secrets(file_cfg.as_ref());
        let api_key = [
            "PANTHEON_NIGHTLY_API_KEY",
            "PANTHEON_CONSOLIDATION_API_KEY",
            "PANTHEON_API_KEY",
        ]
        .iter()
        .find_map(|env| secrets.inject(env).ok().flatten());

        struct Router {
            refine: pantheon_providers::DistillClient,
            distill: pantheon_providers::DistillClient,
            repair: pantheon_providers::DistillClient,
        }
        impl NightlyLlm for Router {
            fn refine_proposal(
                &self,
                model: &pantheon_api::model::AuxiliaryModel,
                draft: &Proposal,
            ) -> Result<String, String> {
                self.refine.refine_proposal(model, draft)
            }
            fn distill_memories(
                &self,
                model: &pantheon_api::model::AuxiliaryModel,
                texts: &[String],
            ) -> Result<Vec<String>, String> {
                self.distill.distill_memories(model, texts)
            }
            fn diagnose_repair(
                &self,
                model: &pantheon_api::model::AuxiliaryModel,
                prompt: &str,
            ) -> Result<String, String> {
                // The Repair slot, never Reflection/Consolidation: the
                // client's slot guard refuses any model that does not match
                // the Repair aux target it was built for.
                self.repair.diagnose_repair(model, prompt)
            }
        }
        fn client_for(
            policy: &pantheon_api::model::ModelPolicy,
            kind: AuxiliaryKind,
            api_key: Option<pantheon_secrets::SecretValue>,
            usage_sink: Arc<dyn pantheon_providers::model_event::AuxiliaryUsageSink>,
        ) -> pantheon_providers::DistillClient {
            let aux = policy.auxiliary(&kind);
            let timeout_secs = aux.map(|a| a.timeout_secs).unwrap_or(60);
            let target: DefaultModel = aux
                .map(|a| DefaultModel {
                    provider: a.provider.clone(),
                    model: a.model.clone(),
                    key_env: None,
                })
                .unwrap_or_else(|| policy.default.clone());
            pantheon_providers::DistillClient::new(target, api_key)
                .with_timeout_secs(timeout_secs)
                .with_usage_sink(usage_sink)
        }
        let usage_sink: Arc<dyn pantheon_providers::model_event::AuxiliaryUsageSink> =
            Arc::new(NightlyUsageSink {
                data_dir: data_dir.to_path_buf(),
            });
        let router = Router {
            refine: client_for(
                &policy,
                AuxiliaryKind::Reflection,
                api_key.clone(),
                Arc::clone(&usage_sink),
            ),
            distill: client_for(
                &policy,
                AuxiliaryKind::Consolidation,
                api_key.clone(),
                Arc::clone(&usage_sink),
            ),
            repair: client_for(&policy, AuxiliaryKind::Repair, api_key, usage_sink),
        };

        // Repair targets: production adapters over the MCP manager, the
        // schedule store + run history, and the tool registry. Declared
        // before `deps` so the `&mut` borrows in `RepairTargets` live long
        // enough. The manager is configured, not connected: no servers are
        // launched until the repair phase (or a tool probe) needs them.
        let mcp_manager = std::sync::Arc::new(crate::mcp::manager_for(data_dir));
        let mut mcp_adapter = crate::nightly_repair::McpRepairAdapter::new(
            mcp_manager.clone(),
            data_dir.to_path_buf(),
        );
        let mut sched_adapter =
            crate::nightly_repair::ScheduleRepairAdapter::new(data_dir.to_path_buf());
        let mut tool_adapter = crate::nightly_repair::ToolRepairAdapter::new(
            data_dir.to_path_buf(),
            cfg.tool_probe_allowlist.clone(),
            Some(mcp_manager),
        );

        let mut deps = NightlyDeps {
            ledger: &ledger,
            backend: backend.as_ref(),
            capability_policy: &mem_policy,
            model_policy: &policy,
            eval_runner: &eval_runner,
            replay_runner,
            llm: Some(&router),
            repair: Some(RepairTargets {
                mcp: Some(&mut mcp_adapter),
                schedule: Some(&mut sched_adapter),
                tools: Some(&mut tool_adapter),
            }),
        };
        match run_pass(&cfg, &mut deps) {
            Ok(result) => Ok(result),
            Err(e) => {
                let msg = format!("nightly pass failed: {}: {}", e.code, e.cause);
                // Best-effort: a failed pass must not leave the previous
                // pass's outcome standing as the last one. The original
                // error is returned unchanged; a state-write failure here
                // must not mask it.
                let mut state = NightlyState::load(data_dir).unwrap_or_default();
                state.last_run_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                state.last_outcome = "failed".to_string();
                state.last_outcome_reason = msg.clone();
                let _ = state.save(data_dir);
                Err(msg)
            }
        }
    }

    /// Whether the nightly pass is enabled in config - the master switch.
    /// Explicit `enabled` wins; absent, the `[nightly.model]` pin (or the
    /// `PANTHEON_NIGHTLY_*` env overrides) implies on; with no `[nightly]`
    /// section the legacy `[reflect]` / `[consolidation]` opt-ins are
    /// honored as a deprecated fallback. Off by default.
    pub fn nightly_enabled(data_dir: &Path) -> bool {
        let file_cfg = crate::config::Config::load_or_report(data_dir);
        crate::config::nightly_pass_enabled(file_cfg.as_ref())
    }

    /// Persist the nightly LLM toggle to the `[nightly]` config section.
    /// Creates the section when absent; leaves every other key untouched.
    /// Writes the explicit `Some(enabled)` flag - explicit always wins over
    /// the model-pin rule (see [`pantheon_api::config::nightly_enabled`]).
    pub fn persist_nightly_enabled(data_dir: &Path, enabled: bool) -> Result<(), String> {
        let mut cfg =
            crate::config::Config::load_for_save(data_dir).map_err(|e| e.cause.clone())?;
        let mut section = cfg.nightly.clone().unwrap_or_default();
        section.enabled = Some(enabled);
        cfg.nightly = Some(section);
        cfg.save(data_dir)
            .map_err(|e| format!("save config: {}", e.cause))
    }

    /// Current toggle state: `(pass_enabled, auto_turns)`.
    pub fn nightly_state(data_dir: &Path) -> (bool, u32) {
        let cfg = crate::config::Config::load_or_report(data_dir);
        (
            nightly_enabled(data_dir),
            crate::config::nightly_auto_turns(cfg.as_ref()),
        )
    }

    /// Whether the on-disk config carries a `[nightly.model]` pin.
    pub fn nightly_model_pin(data_dir: &Path) -> bool {
        crate::config::Config::load_or_report(data_dir)
            .as_ref()
            .and_then(|c| c.nightly.as_ref())
            .is_some_and(pantheon_api::config::nightly_model_pin_present)
    }

    /// Whether the nightly pass has any model pin in play: the
    /// `[nightly.model]` table or the `PANTHEON_NIGHTLY_PROVIDER` /
    /// `PANTHEON_NIGHTLY_MODEL` env overrides. Mirrors the pin half of
    /// [`pantheon_api::config::nightly_enabled`] - used to decide whether
    /// `/nightly on` should print pin guidance.
    pub fn nightly_pin_present(data_dir: &Path) -> bool {
        nightly_model_pin(data_dir) || pantheon_api::config::nightly_env_pin_present()
    }

    /// Guidance for pinning a model to the nightly pass, printed by
    /// `/nightly on` / `pantheon nightly on` when no `[nightly.model]` pin
    /// is configured. An explicit `enabled = true` turns the pass on, but
    /// without a pin the LLM steps resolve through the Reflection /
    /// Consolidation auxiliary slots (chat-model fallback) - the guidance
    /// shows how to pin a dedicated model instead.
    pub fn pin_guidance() -> String {
        "note: no [nightly.model] pin - LLM steps use the Reflection/Consolidation aux slots.\n\
         to pin a model for the nightly pass:\n\
         \n  [nightly.model]\n  provider = \"openai\"\n  model = \"gpt-4o-mini\"\n  api_key_env = \"OPENAI_API_KEY\"\n\
         \n(`api_key_env` names the env var holding the key, never the key itself.\n\
         `provider = \"default\"` inherits [model].\n\
         Alternatively export PANTHEON_NIGHTLY_PROVIDER / PANTHEON_NIGHTLY_MODEL.)"
            .to_string()
    }

    /// Next scheduled nightly run, if any: the earliest next fire among the
    /// non-paused `pantheon schedule nightly` jobs (`__pantheon_nightly__`
    /// task marker). Rendered as an RFC 3339 UTC timestamp. `None` when no
    /// nightly job is scheduled (the loop can still fire on the auto-turns
    /// trigger - see `/nightly status`).
    pub fn next_nightly_run(data_dir: &Path) -> Option<String> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut next: Option<i64> = None;
        for stored in crate::schedule::load_jobs_public(data_dir) {
            if stored.job.task != crate::schedule::NIGHTLY_TASK_MARKER {
                continue;
            }
            if let Some(fire) = stored.job.next_fire_ms(now_ms, stored.last_run) {
                next = Some(next.map_or(fire, |n: i64| n.min(fire)));
            }
        }
        next.and_then(|ms| chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339()))
    }

    /// Parsed `/nightly` subcommand (the TUI) / `pantheon nightly`
    /// subcommand (the CLI). Pure over the argument string, so command
    /// parsing is unit-testable without a TUI or a data dir.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum NightlySub {
        /// Bare `/nightly`: run one manual pass now.
        Run { dry_run: bool },
        /// `/nightly on`: persist `enabled = true`.
        On,
        /// `/nightly off`: persist `enabled = false`.
        Off,
        /// `/nightly status`: the enable state, reason, pin, next run.
        Status,
        /// Anything else: usage error naming the input.
        Unknown(String),
    }

    impl NightlySub {
        pub fn parse(arg: &str) -> Self {
            match arg.trim() {
                "" => NightlySub::Run { dry_run: false },
                "--dry-run" | "-n" => NightlySub::Run { dry_run: true },
                "on" => NightlySub::On,
                "off" => NightlySub::Off,
                "status" => NightlySub::Status,
                other => NightlySub::Unknown(other.to_string()),
            }
        }
    }

    /// One-line status for `/nightly status` / `pantheon nightly status`.
    pub fn status_line(data_dir: &Path) -> String {
        let cfg = crate::config::Config::load_or_report(data_dir);
        let (enabled, auto_turns) = nightly_state(data_dir);
        let loop_line = if auto_turns == 0 {
            "nightly pass: automatic passes disabled (auto_turns = 0)".to_string()
        } else {
            format!("nightly pass: automatic pass every {auto_turns} turns")
        };
        let reason = cfg
            .as_ref()
            .and_then(|c| c.nightly.as_ref())
            .map(pantheon_api::config::nightly_enabled_reason)
            .unwrap_or_else(|| {
                if enabled {
                    "on via legacy [reflect]/[consolidation] (deprecated)"
                } else {
                    // No `[nightly]` section: the default rule applies, so use
                    // the same vocabulary as `nightly_enabled_reason`.
                    "off (no flag, no model pin)"
                }
            });
        let state_line = if enabled {
            format!("nightly pass: on ({reason})")
        } else {
            format!(
                "nightly pass: off ({reason}) - enable with a [nightly.model] pin, `enabled = true`, or `/nightly on`"
            )
        };
        let schedule_line = match next_nightly_run(data_dir) {
            Some(next) => format!("next scheduled run: {next}"),
            None => "next scheduled run: none (`pantheon schedule nightly` to add one)".to_string(),
        };
        match NightlyState::load(data_dir) {
            Ok(state) if state.last_run_ms > 0 => format!(
                "{loop_line}\n{state_line}\n{schedule_line}\nlast pass: {} - {} proposed, {} applied, {} pending approval{}",
                state.last_run_ms,
                state.last_proposals,
                state.last_applied,
                state.last_pending,
                if state.last_dry_run { " (dry run)" } else { "" },
            ),
            _ => format!("{loop_line}\n{state_line}\n{schedule_line}\nno nightly passes yet"),
        }
    }

    /// Human summary of a finished pass for the CLI and the TUI status line.
    pub fn summarize_pass(out: &PassResult) -> String {
        let mut s = format!(
            "nightly pass: {} proposed, {} lessons applied, {} awaiting approval",
            out.proposals.len(),
            out.applied,
            out.pending,
        );
        let mut rejected = 0;
        for p in &out.proposals {
            match p.status {
                pantheon_nightly::ProposalStatus::EvalFailed
                | pantheon_nightly::ProposalStatus::ReplayFailed => {
                    rejected += 1;
                    s.push_str(&format!("\n  rejected: {} ({:?})", p.title, p.status));
                }
                _ => {}
            }
        }
        if rejected > 0 {
            s = format!("{s}\n{rejected} rejected by gates");
        }
        s
    }

    /// Render pending proposals for approval review.
    pub fn render_pending(pending: &[Proposal]) -> String {
        if pending.is_empty() {
            return "no pending nightly proposals".to_string();
        }
        let mut s = String::from("pending nightly proposals:");
        for p in pending {
            s.push_str(&format!(
                "\n\n[{}] {} ({})\n  {}\n  evals: {}\n  from runs: {}",
                p.id,
                p.title,
                p.kind_name(),
                p.body.lines().next().unwrap_or(""),
                if p.eval_tags.is_empty() {
                    "none".to_string()
                } else {
                    p.eval_tags.join(", ")
                },
                if p.provenance_runs.is_empty() {
                    " - ".to_string()
                } else {
                    p.provenance_runs.join(", ")
                },
            ));
        }
        s.push_str("\n\napprove with: pantheon nightly --approve <id>  (or y/n in the TUI)");
        s
    }

    /// Render escalated nightly proposals: one block per escalation, with
    /// the escalation id, kind, title, readable escalation time, and the
    /// reason the pass gave up on it.
    pub fn render_escalations(esc: &[Escalation]) -> String {
        if esc.is_empty() {
            return "no escalated nightly proposals".to_string();
        }
        let mut s = String::from("escalated nightly proposals:");
        for e in esc {
            let when = chrono::DateTime::from_timestamp_millis(e.at_ms)
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| e.at_ms.to_string());
            s.push_str(&format!(
                "\n\n[{}] {} ({})\n  attempts: {}\n  escalated: {}\n  {}",
                e.id, e.title, e.kind, e.attempts, when, e.reason
            ));
        }
        s
    }

    /// Read the audit log (most recent last).
    pub fn read_audit(data_dir: &Path) -> Result<Vec<NightlyEvent>, String> {
        let path = pantheon_nightly::audit_path(data_dir);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut events = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let e: NightlyEvent = serde_json::from_str(line)
                .map_err(|e| format!("parse audit line {}: {e}", i + 1))?;
            events.push(e);
        }
        Ok(events)
    }

    /// Render the audit log as human-readable lines.
    pub fn render_log(events: &[NightlyEvent]) -> String {
        if events.is_empty() {
            return "no nightly audit events".to_string();
        }
        events
            .iter()
            .map(|e| format!("{e:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Approve or deny a pending proposal by id. Returns whether the id was
    /// found.
    pub fn decide(data_dir: &Path, id: &str, approve: bool) -> Result<bool, String> {
        let file_cfg = crate::config::Config::load_or_report(data_dir);
        let cfg = crate::config::nightly_config(file_cfg.as_ref(), data_dir);
        nightly_decide(data_dir, &cfg, id, approve)
    }

    /// Manage the held-out replay task set: `add`, `list`, `remove`.
    pub fn cmd_replay_tasks(args: &[String], data_dir: &Path) {
        let mut store = match ReplayStore::open(data_dir) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
        let sub = args.first().map(|s| s.as_str());
        match sub {
            Some("list") | None => {
                let tasks = store.list();
                if tasks.is_empty() {
                    println!("no replay tasks configured");
                    println!("add one with: pantheon nightly replay-tasks add --id <id> --name <name> --prompt <prompt> --check <tool_sequence|contains|judge> --arg <value> [--exec-cmd <cmd> --exec-arg <a>... --exec-env KEY=VALUE...]");
                    return;
                }
                for t in tasks {
                    let exec = t
                        .exec
                        .as_ref()
                        .map(|e| format!(" exec: {}", e.command))
                        .unwrap_or_else(|| {
                            " exec: none (needs [nightly] replay_command)".to_string()
                        });
                    println!("- {}: {} [{:?}]{exec}", t.id, t.name, t.check);
                }
            }
            Some("add") => {
                let mut id = String::new();
                let mut name = String::new();
                let mut prompt = String::new();
                let mut check = String::new();
                let mut arg = String::new();
                let mut exec_cmd: Option<String> = None;
                let mut exec_args: Vec<String> = Vec::new();
                let mut exec_env: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
                let mut i = 1;
                while i < args.len() {
                    match args[i].as_str() {
                        "--id" => {
                            i += 1;
                            id = args.get(i).cloned().unwrap_or_default();
                        }
                        "--name" => {
                            i += 1;
                            name = args.get(i).cloned().unwrap_or_default();
                        }
                        "--prompt" => {
                            i += 1;
                            prompt = args.get(i).cloned().unwrap_or_default();
                        }
                        "--check" => {
                            i += 1;
                            check = args.get(i).cloned().unwrap_or_default();
                        }
                        "--arg" => {
                            i += 1;
                            if !arg.is_empty() {
                                arg.push(',');
                            }
                            arg.push_str(&args.get(i).cloned().unwrap_or_default());
                        }
                        // Self-contained execution for the built-in replay
                        // runner: `--exec-cmd sh --exec-arg -c --exec-arg
                        // "echo done"` runs the task headlessly; stdout is
                        // the scored transcript. Repeatable flags.
                        "--exec-cmd" => {
                            i += 1;
                            exec_cmd = args.get(i).cloned();
                        }
                        "--exec-arg" => {
                            i += 1;
                            exec_args.push(args.get(i).cloned().unwrap_or_default());
                        }
                        "--exec-env" => {
                            i += 1;
                            let kv = args.get(i).cloned().unwrap_or_default();
                            match kv.split_once('=') {
                                Some((k, v)) if !k.trim().is_empty() => {
                                    exec_env.insert(k.to_string(), v.to_string());
                                }
                                _ => {
                                    eprintln!("error: --exec-env needs KEY=VALUE");
                                    std::process::exit(2);
                                }
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                let check = match check.as_str() {
                    "tool_sequence" => pantheon_nightly::ReplayCheck::ToolSequence {
                        tools: arg
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    },
                    "contains" => pantheon_nightly::ReplayCheck::Contains {
                        text: arg,
                        negate: false,
                    },
                    "judge" => pantheon_nightly::ReplayCheck::Judge { rubric: arg },
                    _ => {
                        eprintln!("error: --check must be tool_sequence, contains, or judge");
                        std::process::exit(2);
                    }
                };
                let task = ReplayTask {
                    id: id.clone(),
                    name,
                    prompt,
                    check,
                    exec: exec_cmd.map(|command| pantheon_nightly::TaskExec {
                        command,
                        args: exec_args,
                        env: exec_env,
                    }),
                };
                match store.save(task) {
                    Ok(()) => println!("saved replay task '{id}'"),
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some("remove") => {
                let id = args.get(1).cloned().unwrap_or_default();
                match store.delete(&id) {
                    Ok(true) => println!("removed replay task '{id}'"),
                    Ok(false) => {
                        eprintln!("error: no replay task '{id}'");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some(other) => {
                eprintln!("unknown replay-tasks subcommand: {other}");
                eprintln!("usage: pantheon nightly replay-tasks [list|add|remove]");
                std::process::exit(2);
            }
        }
    }

    /// `pantheon nightly --help`.
    fn nightly_help() {
        eprintln!("usage: pantheon nightly [--dry-run] [on|off|status|log|pending|escalations|report|replay-tasks ...] [--approve ID] [--deny ID]");
        eprintln!(
            "  on | off                enable or disable the nightly pass (persists to [nightly])"
        );
        eprintln!("  status                   show the pass state and last run");
        eprintln!("  log                      show recent nightly history");
        eprintln!("  pending                  list proposals awaiting approval");
        eprintln!("  escalations              list escalated repairs");
        eprintln!("  report                   show the latest pass report");
        eprintln!("  replay-tasks [list|add|remove]   manage replayable tasks");
        eprintln!("  --dry-run                report what would happen without changing anything");
        eprintln!("  --approve ID | --deny ID  decide a pending proposal");
    }

    /// `pantheon nightly [--dry-run] [on|off|status|log|pending|escalations|report|replay-tasks ...] [--approve ID] [--deny ID]`
    pub fn cmd_nightly(args: &[String], data_dir: &Path) {
        let mut dry_run = false;
        let mut approve: Option<String> = None;
        let mut deny: Option<String> = None;
        let mut sub: Option<&str> = None;
        let mut rest: Vec<String> = Vec::new();
        let mut i = 2;
        while i < args.len() {
            match args[i].as_str() {
                "--help" | "-h" => {
                    nightly_help();
                    std::process::exit(0);
                }
                "--dry-run" => dry_run = true,
                "--approve" => {
                    i += 1;
                    if i < args.len() {
                        approve = Some(args[i].clone());
                    }
                }
                "--deny" => {
                    i += 1;
                    if i < args.len() {
                        deny = Some(args[i].clone());
                    }
                }
                s if sub.is_none() => sub = Some(s),
                s => rest.push(s.to_string()),
            }
            i += 1;
        }

        if let Some(id) = approve {
            match decide(data_dir, &id, true) {
                Ok(true) => println!("approved {id}"),
                Ok(false) => {
                    eprintln!("error: no pending proposal '{id}'");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        if let Some(id) = deny {
            match decide(data_dir, &id, false) {
                Ok(true) => println!("denied {id}"),
                Ok(false) => {
                    eprintln!("error: no pending proposal '{id}'");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }

        match sub {
            Some("on") => match persist_nightly_enabled(data_dir, true) {
                Ok(()) => {
                    println!("nightly pass: on (persisted to [nightly])");
                    if !nightly_pin_present(data_dir) {
                        println!("{}", pin_guidance());
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            },
            Some("off") => match persist_nightly_enabled(data_dir, false) {
                Ok(()) => println!("nightly pass: off (persisted to [nightly])"),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            },
            Some("status") => println!("{}", status_line(data_dir)),
            Some("log") => match read_audit(data_dir) {
                Ok(events) => println!("{}", render_log(&events)),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            },
            Some("pending") => match load_pending(data_dir) {
                Ok(p) => println!("{}", render_pending(&p)),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            },
            Some("escalations") => match load_escalated(data_dir) {
                Ok(e) => println!("{}", render_escalations(&e)),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            },
            Some("replay-tasks") => cmd_replay_tasks(&rest, data_dir),
            Some("report") => {
                let path = data_dir.join("nightly").join("nightly-report.md");
                match std::fs::read_to_string(&path) {
                    Ok(md) => println!("{md}"),
                    Err(_) => {
                        eprintln!("no nightly report yet - run `pantheon nightly` first");
                        std::process::exit(1);
                    }
                }
            }
            Some(other) => {
                eprintln!("unknown nightly subcommand: {other}");
                eprintln!("usage: pantheon nightly [--dry-run] [on|off|status|log|pending|escalations|report|replay-tasks ...] [--approve ID] [--deny ID]");
                std::process::exit(2);
            }
            None => {
                if dry_run {
                    println!("dry run - nothing will be applied or queued; the pass is still audited and reported");
                }
                match run_one_pass(data_dir, dry_run) {
                    Ok(out) => {
                        println!("{}", summarize_pass(&out));
                        let pending = load_pending(data_dir).unwrap_or_default();
                        if !pending.is_empty() {
                            println!();
                            println!("{}", render_pending(&pending));
                        }
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

pub mod nightly_repair {
    //! Production repair targets for the nightly repair phase.
    //!
    //! [`pantheon_nightly::run_repair_phase`] is snapshot-driven and
    //! host-agnostic: it sees broken MCP servers, scheduled jobs, and tools
    //! through the [`McpRepairTarget`], [`ScheduleRepairTarget`], and
    //! [`ToolRepairTarget`] traits. These adapters wire those traits to the
    //! real hosts:
    //!
    //! - [`McpRepairAdapter`] drives the live [`McpManager`]: health snapshots
    //!   from `health()`, real reconnects through a backoff-bypassing
    //!   [`McpManager::retry_now`], config re-resolution through
    //!   [`McpManager::configure`], and disable persisted to config.toml (or
    //!   the declaring migration file) plus the live manager.
    //! - [`ScheduleRepairAdapter`] reads/writes `schedule.json` through the
    //!   scheduler core's [`load_jobs`]/[`save_jobs`] and merges per-job
    //!   failure counters from [`RunHistory`]. It deliberately does not touch
    //!   the tick loop's locking, pause propagation, or fire recording.
    //! - [`ToolRepairAdapter`] smoke-probes tools with empty args behind the
    //!   capability gate (allowlist-only), re-resolves MCP-backed tools
    //!   through their owning server's config, and disables through
    //!   [`ToolRegistry::remove`] plus the durable disable list in
    //!   [`pantheon_runtime::nightly_tools`] so the next registry build skips
    //!   the name.
    //!
    //! Behavioral coverage of the phase itself lives in
    //! `eval/tests/nightly_repair.rs` (fakes); the tests here cover the
    //! adapter mapping and persistence that need no live infrastructure.

    use pantheon_api::capability::Policy;
    use pantheon_mcp::manager::{sanitize_segment, McpManager, McpServerSpec, ServerHealth};
    use pantheon_migration::read_mcp_declarations;
    use pantheon_nightly::{
        McpRepairTarget, McpServerSnapshot, ScheduleRepairTarget, ScheduledJobSnapshot,
        ToolRepairTarget,
    };
    use pantheon_scheduler::{load_jobs, save_jobs, RunHistory, ScheduleKind};
    use pantheon_tools::builtins::{register_builtins_with, BuiltinOptions};
    use pantheon_tools::tools::ToolRegistry;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    pub use pantheon_runtime::nightly_tools::{load_disabled_tools, record_disabled_tool};

    // ---------------------------------------------------------------------------
    // MCP
    // ---------------------------------------------------------------------------

    /// [`McpRepairTarget`] over the live [`McpManager`].
    pub struct McpRepairAdapter {
        manager: Arc<McpManager>,
        data_dir: PathBuf,
    }

    impl McpRepairAdapter {
        pub fn new(manager: Arc<McpManager>, data_dir: PathBuf) -> Self {
            Self { manager, data_dir }
        }
    }

    /// One server's health as the repair phase sees it.
    fn snapshot_of(h: &ServerHealth) -> McpServerSnapshot {
        McpServerSnapshot {
            name: h.name.clone(),
            status: h.status.as_str().to_string(),
            failures: h.failures,
            last_error: h.last_error.clone(),
        }
    }

    /// Re-read one server's spec from the on-disk config + declarations and
    /// push it through [`McpManager::configure`].
    ///
    /// A changed spec drops the live connection (the manager reconnects
    /// lazily on next use); an unchanged spec is a no-op by `configure`'s
    /// idempotence contract - callers that need a real attempt follow up
    /// with [`McpManager::retry_now`]. Approval records are untouched: this
    /// never approves, un-approves, or otherwise touches the launcher's
    /// approval logic.
    fn reresolve_mcp_server(
        manager: &McpManager,
        data_dir: &Path,
        name: &str,
    ) -> Result<(), String> {
        let cfg = crate::config::Config::load_or_report(data_dir);
        let declarations = read_mcp_declarations(data_dir);
        let mcfg = crate::config::resolve_mcp_section(
            cfg.as_ref().and_then(|c| c.mcp.as_ref()),
            &declarations,
        );
        let spec = mcfg
            .servers
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("mcp server '{name}': not declared (or disabled) in config"))?;
        let mut specs: Vec<McpServerSpec> = manager
            .spec_names()
            .into_iter()
            .filter_map(|n| manager.spec(&n))
            .collect();
        match specs.iter_mut().find(|s| s.name == name) {
            Some(slot) => *slot = spec,
            None => specs.push(spec),
        }
        manager.configure(specs);
        Ok(())
    }

    /// Persist `enabled = false` for one MCP server: `[mcp.servers.<name>]`
    /// in config.toml when the server is config-defined, else the
    /// declaration file that declares it (mirroring `pantheon mcp disable`,
    /// whose verb refuses to rewrite config-defined servers and vice
    /// versa).
    fn persist_mcp_disabled(data_dir: &Path, name: &str) -> Result<(), String> {
        let mut cfg = crate::config::Config::load_or_report(data_dir).unwrap_or_default();
        if let Some(entry) = cfg.mcp.as_mut().and_then(|m| m.servers.get_mut(name)) {
            entry.enabled = false;
            return cfg
                .save(data_dir)
                .map_err(|e| format!("save config: {}: {}", e.code, e.cause));
        }
        // Declaration-sourced server: rewrite its declaration file.
        let declarations = read_mcp_declarations(data_dir);
        let source = declarations
            .iter()
            .find(|d| d.servers.iter().any(|s| s.name == name))
            .map(|d| d.source.clone())
            .ok_or_else(|| {
                format!("mcp server '{name}': not declared in config or declarations")
            })?;
        let path = data_dir.join("mcp").join(format!("{source}.json"));
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        let mut touched = false;
        if let Some(servers) = v.get_mut("servers").and_then(|s| s.as_array_mut()) {
            for s in servers {
                if s.get("name").and_then(|n| n.as_str()) == Some(name) {
                    s["enabled"] = serde_json::Value::Bool(false);
                    touched = true;
                }
            }
        }
        if !touched {
            return Err(format!(
                "mcp server '{name}': not found in {}",
                path.display()
            ));
        }
        // Atomic rewrite: temp file + rename.
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&v)
            .map_err(|e| format!("encode {}: {e}", path.display()))?;
        std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("publish {}: {e}", path.display()))?;
        Ok(())
    }

    impl McpRepairTarget for McpRepairAdapter {
        fn servers(&mut self) -> Vec<McpServerSnapshot> {
            // Cheap liveness only, no probing: the repair ladder owns every
            // mutating attempt (retry_connect / re_resolve), so snapshotting
            // must not spend them as a side effect.
            self.manager.health().iter().map(snapshot_of).collect()
        }

        fn retry_connect(&mut self, name: &str) -> Result<(), String> {
            // One real connect attempt: retry_now bypasses the reconnect
            // backoff cooldown, which the lazy paths would otherwise honor
            // (a server inside its cooldown would never actually be
            // retried). Re-resolving + reconfiguring cannot do this:
            // configure is idempotent on an unchanged spec and only
            // reconnects lazily on a changed one.
            self.manager.retry_now(name).map_err(|e| e.to_string())
        }

        fn re_resolve(&mut self, name: &str) -> Result<(), String> {
            // Re-resolve the server's config - env vars and paths may have
            // changed since it was configured - then make the retry real
            // instead of lazy: the ladder counts this step as
            // re-resolve + retry.
            reresolve_mcp_server(&self.manager, &self.data_dir, name)?;
            self.manager.retry_now(name).map_err(|e| e.to_string())
        }

        fn disable(&mut self, name: &str, _reason: &str) -> Result<(), String> {
            // The reason rides the phase's escalation; the adapter's job is
            // containment: persist the disable and stop the live manager
            // reconnecting.
            persist_mcp_disabled(&self.data_dir, name)?;
            let mut specs: Vec<McpServerSpec> = self
                .manager
                .spec_names()
                .into_iter()
                .filter_map(|n| self.manager.spec(&n))
                .collect();
            let spec = specs
                .iter_mut()
                .find(|s| s.name == name)
                .ok_or_else(|| format!("mcp server '{name}': not managed"))?;
            spec.enabled = false;
            // The changed-spec branch drops the live connection (Drop shuts
            // the child down) and marks the server Disabled, so nothing
            // reconnects it. The approval record is kept: re-enabling later
            // does not need re-approval.
            self.manager.configure(specs);
            Ok(())
        }
    }

    // ---------------------------------------------------------------------------
    // Schedules
    // ---------------------------------------------------------------------------

    /// [`ScheduleRepairTarget`] over `schedule.json` + [`RunHistory`].
    pub struct ScheduleRepairAdapter {
        data_dir: PathBuf,
    }

    impl ScheduleRepairAdapter {
        pub fn new(data_dir: PathBuf) -> Self {
            Self { data_dir }
        }
    }

    impl ScheduleRepairTarget for ScheduleRepairAdapter {
        fn jobs(&mut self) -> Vec<ScheduledJobSnapshot> {
            let jobs = load_jobs(&self.data_dir).unwrap_or_default();
            let history = RunHistory::open(&self.data_dir).ok();
            jobs.into_iter()
                .map(|s| {
                    let stats = history.as_ref().and_then(|h| h.stats(&s.job.id));
                    let (kind, cron_expr) = match &s.job.kind {
                        ScheduleKind::Cron { expr } => ("cron", Some(expr.clone())),
                        ScheduleKind::Interval { .. } => ("interval", None),
                        ScheduleKind::OneShot { .. } => ("one-shot", None),
                    };
                    ScheduledJobSnapshot {
                        id: s.job.id.clone(),
                        kind: kind.to_string(),
                        cron_expr,
                        paused: s.job.paused,
                        consecutive_failures: stats.map(|st| st.consecutive_failures).unwrap_or(0),
                        last_error: stats.and_then(|st| st.last_error.clone()),
                    }
                })
                .collect()
        }

        fn repair_cron(&mut self, id: &str, new_expr: &str) -> Result<(), String> {
            // The expression was already validated by the repair phase; the
            // host persists it and resets the job's run history, per the
            // trait contract.
            let mut jobs = load_jobs(&self.data_dir).map_err(|e| format!("load schedule: {e}"))?;
            let job = jobs
                .iter_mut()
                .find(|j| j.job.id == id)
                .ok_or_else(|| format!("scheduled job '{id}': not found"))?;
            job.job.kind = ScheduleKind::Cron {
                expr: new_expr.to_string(),
            };
            save_jobs(&self.data_dir, &jobs).map_err(|e| format!("save schedule: {e}"))?;
            let mut history =
                RunHistory::open(&self.data_dir).map_err(|e| format!("open run history: {e}"))?;
            history.reset(id)
        }

        fn pause(&mut self, id: &str, _reason: &str) -> Result<(), String> {
            // The reason rides the phase's escalation and audit events; the
            // store carries only the paused flag (same shape as
            // `pantheon schedule pause`).
            let mut jobs = load_jobs(&self.data_dir).map_err(|e| format!("load schedule: {e}"))?;
            let job = jobs
                .iter_mut()
                .find(|j| j.job.id == id)
                .ok_or_else(|| format!("scheduled job '{id}': not found"))?;
            job.job.paused = true;
            save_jobs(&self.data_dir, &jobs).map_err(|e| format!("save schedule: {e}"))
        }
    }

    // ---------------------------------------------------------------------------
    // Tools
    // ---------------------------------------------------------------------------

    /// Owning server for an `mcp_<server>_<tool>` projected tool name: the
    /// longest sanitized-server prefix wins. `None` when the name is not
    /// MCP-backed.
    fn mcp_server_for_tool(servers: &[String], tool: &str) -> Option<String> {
        let rest = tool.strip_prefix("mcp_")?;
        servers
            .iter()
            .filter_map(|s| {
                let san = sanitize_segment(s);
                rest.strip_prefix(san.as_str())
                    .filter(|r| r.starts_with('_'))
                    .map(|_| (san.len(), s.clone()))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, s)| s)
    }

    /// [`ToolRepairTarget`] over a [`ToolRegistry`] plus the durable disable
    /// list.
    ///
    /// The registry is the probe surface (builtins per `[tools]` enablement
    /// plus the MCP-projected tools); it is built lazily on the first
    /// allowlisted probe so a pass that never probes never connects to
    /// anything. `disable` removes the name from the built registry when
    /// there is one, but the durable disable list is the real containment:
    /// every future [`Session::build_tool_registry`] skips the name.
    pub struct ToolRepairAdapter {
        data_dir: PathBuf,
        allowlist: Vec<String>,
        mcp: Option<Arc<McpManager>>,
        registry: Option<ToolRegistry>,
    }

    impl ToolRepairAdapter {
        pub fn new(
            data_dir: PathBuf,
            allowlist: Vec<String>,
            mcp: Option<Arc<McpManager>>,
        ) -> Self {
            Self {
                data_dir,
                allowlist,
                mcp,
                registry: None,
            }
        }

        /// The probe registry: builtins per `[tools]` enablement plus the
        /// MCP-projected tools, mirroring the session's registry closely
        /// enough for smoke probes. Built lazily: probing is allowlist-gated,
        /// so a pass that never probes never connects to anything.
        fn probe_registry(&mut self) -> &ToolRegistry {
            if self.registry.is_none() {
                let mut reg = ToolRegistry::new();
                let file_cfg = crate::config::Config::load_or_report(&self.data_dir);
                let tools = crate::config::tool_enablement(file_cfg.as_ref());
                register_builtins_with(
                    &mut reg,
                    BuiltinOptions {
                        safewrite_state_dir: Some(self.data_dir.join("safewrite")),
                        // Probe runs read the process cwd; there is no session
                        // to move, so the default root is correct here.
                        workspace: None,
                        enable_terminal: tools.terminal,
                        enable_files: tools.files,
                        enable_ask_user: tools.ask_user,
                        enable_plugins: tools.plugins,
                        data_dir: Some(self.data_dir.clone()),
                        // Drive-by compile fix (another leaf added this field to
                        // BuiltinOptions): None matches the struct's Default.
                        shell_child_hook: None,
                        // The nightly repair host never runs `cf` commands
                        // and never injects child env: None matches Default.
                        shell_env_hook: None,
                        // Probe runs must not rewrite files as a side effect.
                        formatter: None,
                        // Replay/probe runs get no persistent terminals.
                        pty_manager: None,
                    },
                );
                if let Some(mcp) = &self.mcp {
                    // Project the servers' tools so allowlisted `mcp_*` names
                    // probe the real thing. This connects to approved servers
                    // - the same thing session startup does - which is why it
                    // stays behind the lazy build.
                    let _ = mcp.register_tools(&mut reg);
                }
                self.registry = Some(reg);
            }
            self.registry.as_ref().expect("built above")
        }
    }

    impl ToolRepairTarget for ToolRepairAdapter {
        fn probe(&mut self, name: &str) -> Result<(), String> {
            // Probing executes the tool: the allowlist is the safety gate,
            // and the adapter refuses anything outside it even though the
            // phase already enforces the same list (defense in depth).
            if !self.allowlist.iter().any(|a| a == name) {
                return Err(format!(
                    "refusing to probe '{name}': not in the nightly tool_probe_allowlist"
                ));
            }
            // Empty-args smoke probe behind the capability gate. The policy is
            // deliberately read-only: a probe must never mutate, and a tool
            // that cannot run read-only fails closed - the ladder then
            // contains it instead of trusting it.
            let policy = Policy::researcher_readonly();
            let reg = self.probe_registry();
            reg.execute_gated(&policy, name, "{}")
                .map(|_| ())
                .map_err(|e| format!("probe '{name}' failed: {}: {}", e.code, e.cause))
        }

        fn re_resolve(&mut self, name: &str) -> Result<(), String> {
            // The only re-resolution path in this host: MCP-projected tools
            // (`mcp_<server>_<tool>`) re-resolve through their owning
            // server's config (env vars and paths may have changed). Anything
            // else has no backing config to re-resolve - and returning Ok here
            // would be a lie: the ladder treats Ok as "recovered" and would
            // skip containment for a still-broken tool.
            let mcp = self
                .mcp
                .clone()
                .ok_or_else(|| format!("tool '{name}': no MCP manager; nothing to re-resolve"))?;
            let server = mcp_server_for_tool(&mcp.spec_names(), name)
                .ok_or_else(|| format!("tool '{name}': not MCP-backed; no re-resolution path"))?;
            reresolve_mcp_server(&mcp, &self.data_dir, &server)?;
            mcp.retry_now(&server).map_err(|e| e.to_string())?;
            // Confirmation probe, still allowlist-gated: Ok only when the
            // tool demonstrably works again.
            self.probe(name)
        }

        fn disable(&mut self, name: &str, reason: &str) -> Result<(), String> {
            if let Some(reg) = self.registry.as_mut() {
                reg.remove(name);
            }
            record_disabled_tool(&self.data_dir, name, reason)
        }
    }

    // ---------------------------------------------------------------------------
    // Tests (no live infrastructure: mapping, gating, persistence)
    // ---------------------------------------------------------------------------
}

pub mod repair {
    //! `pantheon repair` - find and fix anything wrong with this install.
    //!
    //! `doctor` is diagnosis: it reports every problem and a fix hint per check.
    //! `repair` is the fixing half, and it is deliberately a separate verb so
    //! neither one can quietly do the other's job - a `doctor` that mutated state
    //! would be unsafe to run in a loop, and a repair that diagnosed would be
    //! useless.
    //!
    //! The mechanism is one check body with two callers, not two implementations.
    //! Each entry below pairs a `diagnose` (read-only, always runs first) with a
    //! `repair` (mutates, only when `diagnose` found something). This is the shape
    //! The established doctor/report shape, where
    //! `should_fix` is a bool handed to the same function. The first version of
    //! this file was a `repair check|runs` subcommand pair that re-implemented a
    //! narrow slice of `doctor` and called it coverage; that is the failure this
    //! registry exists to prevent.
    //!
    //! Rules every fixer obeys:
    //!
    //! 1. **Back up before mutating.** A repair that can make things worse leaves
    //!    the original recoverable, and names the copy in its report.
    //! 2. **Diagnose first, repair second.** A fixer never runs unconditionally,
    //!    so `repair` on a healthy install changes nothing.
    //! 3. **A problem with no safe automatic fix is reported as manual**, not
    //!    skipped. "Repair found nothing it could fix" has to be an honest
    //!    statement about the install.

    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};

    /// One thing repair looked at and what it did about it.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RepairOutcome {
        /// Stable id, and the name of the check that produced it.
        pub check: String,
        /// "clean" | "fixed" | "manual" | "failed"
        pub status: String,
        pub detail: String,
        /// The exact action a human takes when this was not automatic.
        pub manual: String,
        /// Where the pre-repair copy landed, when one was taken.
        pub backup: Option<PathBuf>,
    }

    /// A check that can also repair what it finds.
    pub struct Fixer {
        pub check: &'static str,
        /// Diagnose without touching anything.
        pub diagnose: fn(&Path) -> Result<Option<String>, String>,
        /// Apply the repair. `Err` means it was attempted and did not succeed; a
        /// backup taken on the way is named in the message.
        pub repair: fn(&Path) -> Result<String, String>,
    }

    /// Checks that deliberately have no automatic fix, because the only correct
    /// action needs a human decision. A fixer that declines one of these is
    /// working, not failing.
    const FIXERS_WITHOUT_A_SAFE_FIX: &[&str] = &["skills", "config"];

    /// Registry order is report order.
    pub fn registry() -> Vec<Fixer> {
        vec![
            data_dir_layout(),
            config(),
            ledger_integrity(),
            stranded_runs(),
            search_index(),
            memory_index(),
            skills(),
        ]
    }

    pub fn usage() -> &'static str {
        "usage: pantheon repair [--dry-run] [--json]\n  \
         \n  \
         Finds anything wrong with this install and fixes what can be fixed\n  \
         safely. Diagnose only, no changes:  pantheon doctor\n  \
         Back up everything before repairing:   pantheon repair --dry-run"
    }

    /// Run every fixer against `dd`. Exits 1 when something was found that repair
    /// could not resolve, so it is usable as a gate in a script.
    pub fn cmd_repair(args: &[String]) {
        let dry_run = args.iter().any(|a| a == "--dry-run");
        let as_json = args.iter().any(|a| a == "--json");
        // A stray positional is almost always `check` or `runs` left over from the
        // old subcommand surface. Say so, because silently ignoring it would look
        // like the command worked.
        if let Some(bad) = args.iter().skip(2).find(|a| !a.starts_with('-')) {
            eprintln!(
                "repair: '{bad}' is not a flag. `repair` takes no subcommands: it finds \
                 and fixes everything it can. Diagnose without changing anything with \
                 `pantheon doctor`."
            );
            std::process::exit(2);
        }
        if args.iter().any(|a| a == "--help" || a == "-h") {
            println!("{}", usage());
            return;
        }

        let dd = crate::terminal::data_dir();
        // Load config so a broken config is surfaced here rather than as a
        // confusing "no such table" from a ledger query.
        let _ = crate::config::Config::load_or_report(&dd);

        let mut outcomes = Vec::new();
        for fixer in registry() {
            let finding = match (fixer.diagnose)(&dd) {
                Ok(None) => {
                    outcomes.push(RepairOutcome {
                        check: fixer.check.to_string(),
                        status: "clean".into(),
                        detail: String::new(),
                        manual: String::new(),
                        backup: None,
                    });
                    continue;
                }
                Ok(Some(what)) => what,
                // A diagnose that cannot run is itself a finding, not a crash:
                // the rest of the install is still worth repairing.
                Err(e) => {
                    outcomes.push(RepairOutcome {
                        check: fixer.check.to_string(),
                        status: "failed".into(),
                        detail: format!("check could not run: {e}"),
                        manual: "run `pantheon doctor` for the failing section".into(),
                        backup: None,
                    });
                    continue;
                }
            };

            if dry_run {
                outcomes.push(RepairOutcome {
                    check: fixer.check.to_string(),
                    status: "manual".into(),
                    detail: finding,
                    manual: "re-run without --dry-run to apply the automatic fix".into(),
                    backup: None,
                });
                continue;
            }

            match (fixer.repair)(&dd) {
                Ok(what) => outcomes.push(RepairOutcome {
                    check: fixer.check.to_string(),
                    status: "fixed".into(),
                    detail: format!("{finding} → {what}"),
                    manual: String::new(),
                    backup: None,
                }),
                Err(e) => {
                    // A fixer that names a backup in its message left one behind.
                    // Surfacing the path is the difference between a recoverable
                    // failure and a scary one.
                    //
                    // A fixer that *declined* has not failed. It correctly refused
                    // something unsafe and left the file alone, which is exactly
                    // the outcome the registry is designed to produce. Calling
                    // that "failed" would train the operator to ignore the word,
                    // and would make a deliberate refusal exit non-zero.
                    let declined = FIXERS_WITHOUT_A_SAFE_FIX.contains(&fixer.check);
                    let backup = extract_backup(&e);
                    outcomes.push(RepairOutcome {
                        check: fixer.check.to_string(),
                        status: if declined || backup.is_some() {
                            "manual".into()
                        } else {
                            "failed".into()
                        },
                        detail: format!("{finding} → {e}"),
                        manual: "see the message above for the exact recovery step".into(),
                        backup,
                    });
                }
            }
        }

        if as_json {
            // One document, one shape. The previous version printed a bare `true`
            // and then the array on a second call, which is two JSON documents and
            // parses as neither.
            #[derive(Serialize)]
            struct Report<'a> {
                ok: bool,
                fixed: usize,
                manual: usize,
                failed: usize,
                outcomes: &'a [RepairOutcome],
            }
            let ok = outcomes
                .iter()
                .all(|o| o.status == "clean" || o.status == "fixed");
            let report = Report {
                ok,
                fixed: outcomes.iter().filter(|o| o.status == "fixed").count(),
                manual: outcomes.iter().filter(|o| o.status == "manual").count(),
                failed: outcomes.iter().filter(|o| o.status == "failed").count(),
                outcomes: &outcomes,
            };
            crate::terminal::print_json("repair", &report);
            if !ok {
                std::process::exit(1);
            }
            return;
        }

        render(&outcomes, dry_run);
        // Only a genuine failure exits 1. A `manual` finding is something the
        // operator has to decide about, and `repair` did its job - a registry
        // whose expected outcome set a non-zero exit would be useless in a script.
        if outcomes.iter().any(|o| o.status == "failed") {
            std::process::exit(1);
        }
    }

    fn render(outcomes: &[RepairOutcome], dry_run: bool) {
        if outcomes.iter().all(|o| o.status == "clean") {
            println!("ok - nothing to repair");
            return;
        }
        for o in outcomes {
            match o.status.as_str() {
                "clean" => continue,
                "fixed" => println!("fixed  {}: {}", o.check, o.detail),
                "manual" => println!("manual {}: {}", o.check, o.detail),
                _ => println!("failed {}: {}", o.check, o.detail),
            }
            if let Some(b) = &o.backup {
                println!("       backup: {}", b.display());
            }
            if !o.manual.is_empty() {
                println!("       next:  {}", o.manual);
            }
        }
        let fixed = outcomes.iter().filter(|o| o.status == "fixed").count();
        let manual = outcomes.iter().filter(|o| o.status == "manual").count();
        let failed = outcomes.iter().filter(|o| o.status == "failed").count();
        if dry_run {
            println!("\n{dry_run} dry run - nothing was changed");
        } else {
            println!("\n{fixed} fixed, {manual} need a human, {failed} failed");
        }
    }

    /// Pull a backup path out of a fixer's message. Fixers report the path in
    /// prose (it is also what the operator reads), so the structured field is
    /// recovered from it rather than threading a second return value through
    /// every signature.
    fn extract_backup(msg: &str) -> Option<PathBuf> {
        const MARK: &str = "backup: ";
        let start = msg.to_lowercase().find(MARK)? + MARK.len();
        let rest = &msg[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ')')
            .unwrap_or(rest.len());
        Some(PathBuf::from(&rest[..end]))
    }

    // ---------------------------------------------------------------- config

    /// A missing or unparseable `config.toml`.
    ///
    /// The absent case is fixable and the malformed case is not: a config that
    /// does not parse may contain anything, and writing over it would destroy a
    /// file the user hand-edited and cannot reconstruct. So the fixer creates a
    /// default when the file is simply missing, and for a parse failure it backs
    /// the file up and reports - never overwrites.
    fn config() -> Fixer {
        Fixer {
            check: "config",
            // A missing file and a malformed file are different problems with
            // different fixes, so they must not arrive as the same finding.
            // `Config::load` reports ENOENT as a load error, which made every
            // fresh install look like a corrupt config.
            diagnose: |dd| {
                if !crate::config::Config::path(dd).exists() {
                    return Ok(Some("no config.toml; run `pantheon setup`".to_string()));
                }
                match crate::config::Config::load(dd) {
                    Ok(c) => {
                        let problems = c.validate();
                        if problems.is_empty() {
                            Ok(None)
                        } else {
                            Ok(Some(problems.join("; ")))
                        }
                    }
                    Err(e) => Ok(Some(format!(
                        "{} does not parse: {}",
                        crate::config::Config::path(dd).display(),
                        e.cause
                    ))),
                }
            },
            repair: |dd| {
                let path = crate::config::Config::path(dd);
                if !path.exists() {
                    // A default config is a working starting point, not a guess:
                    // `Config::default` is the same one `setup` starts from, so
                    // repair leaves the user in the state setup would have.
                    let cfg = crate::config::Config::default();
                    cfg.save(dd)
                        .map_err(|e| format!("cannot write {}: {}", path.display(), e.cause))?;
                    // The default has no [model] section, which `validate()`
                    // rejects - a config with no provider cannot run a turn. So
                    // this fix is partial by nature and says so, rather than
                    // implying the install is now runnable.
                    return Ok(format!(
                        "wrote a default {} (it has no [model] section yet, so \
                         `pantheon setup` is still needed before a turn will run)",
                        path.display()
                    ));
                }
                // It exists. If it loads but does not validate, the parse succeeded
                // and the message must say that, not "does not parse" - the two
                // have completely different fixes (add a model section vs. repair
                // TOML by hand), and conflating them sent the operator to the
                // wrong one.
                if let Ok(c) = crate::config::Config::load(dd) {
                    let problems = c.validate();
                    if !problems.is_empty() {
                        return Err(format!(
                            "config.toml is valid TOML but not a runnable config: {}. \
                             Repair is yours to make here: `pantheon setup` writes a \
                             [model] section, or set provider and model in the file.",
                            problems.join("; ")
                        ));
                    }
                }
                // It exists and does not parse. Preserve it: the user's own edits
                // are in there, and a fixer that overwrites a config it merely
                // failed to understand is the most destructive thing this command
                // could do.
                let backup = backup_file(&path, "config.toml")?;
                Err(format!(
                    "{} does not parse, and its contents are preserved at {}. Repair it by \
                     hand, or delete it and re-run `pantheon repair` to get a default.",
                    path.display(),
                    backup.display()
                ))
            },
        }
    }

    // ---------------------------------------------------------------- memory

    /// The memory FTS5 index. Unlike the ledger's sidecar this one is fully
    /// rebuildable, because it is an external-content table kept in sync by
    /// triggers with `memories` as the source of truth.
    fn memory_index() -> Fixer {
        Fixer {
            check: "memory-index",
            diagnose: |dd| {
                let path = dd.join("memory.db");
                if !path.exists() {
                    return Ok(None);
                }
                match pantheon_memory::fts_health(&path) {
                    Ok(healthy) if healthy => Ok(None),
                    Ok(_) => Ok(Some("the memory FTS index is missing or empty".to_string())),
                    Err(e) => Ok(Some(format!(
                        "the memory FTS index is unusable: {}",
                        e.cause
                    ))),
                }
            },
            repair: |dd| {
                let path = dd.join("memory.db");
                if !path.exists() {
                    return Ok(format!(
                        "{} does not exist; it is created on the first memory write",
                        path.display()
                    ));
                }
                let backup = backup_file(&path, "memory.db")?;
                let rows = pantheon_memory::rebuild_fts(&path)
                    .map_err(|e| format!("rebuild failed: {}", e.cause))?;
                Ok(format!(
                    "rebuilt the memory FTS index from {rows} record(s). Backup: {}",
                    backup.display()
                ))
            },
        }
    }

    // ---------------------------------------------------------------- skills

    /// Skills that discovery rejected. The report names each one; the repair
    /// removes the broken files after a backup.
    ///
    /// This is destructive to the user's own skill files, so it is the one fixer
    /// that needs a human decision: a rejected skill is usually a typo in front
    /// matter, and deleting the file discards work rather than fixing it. It is
    /// therefore reported as manual and NOT deleted.
    fn skills() -> Fixer {
        Fixer {
            check: "skills",
            diagnose: |dd| {
                let rejected = crate::skills::rejected_at(dd);
                if rejected.is_empty() {
                    return Ok(None);
                }
                // `reason` already embeds the file path for parse failures, so
                // prefixing `path` again printed it twice. Use the reason alone
                // and fall back to the path only when the reason has none.
                Ok(Some(
                    rejected
                        .iter()
                        .map(|(path, reason)| {
                            if reason.contains(&path.display().to_string()) {
                                reason.clone()
                            } else {
                                format!("{}: {}", path.display(), reason)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("; "),
                ))
            },
            repair: |_dd| {
                Err(
                    "a rejected skill is usually a typo in its front matter, so the file is kept \
                 and the problem is reported instead"
                        .to_string(),
                )
            },
        }
    }

    // ---------------------------------------------------------------- layout

    /// Directories the runtime writes into. A missing one is not a fault (most are
    /// created lazily), but a *file* where a directory belongs is the user's data
    /// and is reported, never clobbered.
    fn data_dir_layout() -> Fixer {
        Fixer {
            check: "data-dir-layout",
            diagnose: |dd| {
                if !dd.exists() {
                    return Ok(Some(format!("{} does not exist", dd.display())));
                }
                let mut bad = Vec::new();
                for name in ["skills", "gateway", "extensions"] {
                    let p = dd.join(name);
                    if p.exists() && !p.is_dir() {
                        bad.push(format!("{name} exists but is not a directory"));
                    }
                }
                if bad.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(bad.join("; ")))
                }
            },
            repair: |dd| {
                if !dd.exists() {
                    std::fs::create_dir_all(dd)
                        .map_err(|e| format!("cannot create {}: {e}", dd.display()))?;
                    return Ok(format!("created {}", dd.display()));
                }
                let mut created = Vec::new();
                for name in ["skills", "gateway", "extensions"] {
                    let p = dd.join(name);
                    if p.exists() {
                        continue; // a file here is reported, never overwritten
                    }
                    std::fs::create_dir_all(&p)
                        .map_err(|e| format!("cannot create {}: {e}", p.display()))?;
                    created.push(name);
                }
                if created.is_empty() {
                    Ok("nothing to create".to_string())
                } else {
                    Ok(format!("created {}", created.join(", ")))
                }
            },
        }
    }

    // ---------------------------------------------------------------- ledger

    fn ledger_integrity() -> Fixer {
        Fixer {
            check: "ledger-integrity",
            diagnose: |dd| {
                let path = dd.join("ledger.db");
                if !path.exists() {
                    return Ok(None); // nothing built yet; not a fault
                }
                let ledger = match pantheon_storage::Ledger::open(&path) {
                    Ok(l) => l,
                    Err(e) => return Ok(Some(format!("ledger.db does not open: {}", e.cause))),
                };
                // `PRAGMA integrity_check` returns `["ok"]` when healthy, not an
                // empty list. Treating "one row that says ok" as a problem would
                // make every healthy install look damaged.
                match ledger.integrity_check() {
                    Ok(rows) if rows.iter().all(|r| r.trim() == "ok") => Ok(None),
                    Ok(rows) if rows.is_empty() => Ok(None),
                    Ok(rows) => Ok(Some(format!("PRAGMA integrity_check: {}", rows.join("; ")))),
                    Err(e) => Ok(Some(format!("integrity_check could not run: {}", e.cause))),
                }
            },
            repair: |dd| {
                let path = dd.join("ledger.db");
                let backup = backup_file(&path, "ledger.db")?;
                // Structural damage cannot be repaired by writing to the damaged
                // file, and `VACUUM` on one can make it worse. The only honest
                // repair is a backup plus an explicit statement of what is left.
                Err(format!(
                    "ledger.db is structurally damaged and cannot be repaired in place. \
                     Backup: {}. Restore a known-good ledger.db over it, or run \
                     `pantheon reset --state` to start clean (that discards run history).",
                    backup.display()
                ))
            },
        }
    }

    fn stranded_runs() -> Fixer {
        Fixer {
            check: "stranded-runs",
            diagnose: |dd| {
                let path = dd.join("ledger.db");
                if !path.exists() {
                    return Ok(None);
                }
                let ledger = match pantheon_storage::Ledger::open(&path) {
                    Ok(l) => l,
                    Err(e) => return Ok(Some(format!("ledger.db does not open: {}", e.cause))),
                };
                let stuck = match ledger.stuck_runs() {
                    Ok(s) => s,
                    Err(e) => return Ok(Some(format!("cannot read runs: {}", e.cause))),
                };
                if stuck.is_empty() {
                    return Ok(None);
                }
                Ok(Some(
                    stuck
                        .iter()
                        .map(|(id, status, ..)| format!("{id} is still '{status}'"))
                        .collect::<Vec<_>>()
                        .join("; "),
                ))
            },
            repair: |dd| {
                let path = dd.join("ledger.db");
                let ledger = match pantheon_storage::Ledger::open(&path) {
                    Ok(l) => l,
                    Err(e) => return Err(format!("cannot open ledger: {}", e.cause)),
                };
                let stuck = match ledger.stuck_runs() {
                    Ok(s) => s,
                    Err(e) => return Err(format!("cannot read runs: {}", e.cause)),
                };
                let mut settled = 0;
                let mut skipped: Vec<String> = Vec::new();
                for (run_id, status, ..) in stuck {
                    // `stuck_runs` already excludes runs with a live lease, so
                    // everything here is a corpse. Settling appends real events
                    // rather than rewriting the status, so the ledger still
                    // explains how the run ended.
                    match ledger.settle_stuck_run(&run_id, "repaired by `pantheon repair`") {
                        Ok(()) => settled += 1,
                        Err(e) => skipped.push(format!("{run_id} ({status}): {}", e.cause)),
                    }
                }
                if !skipped.is_empty() {
                    return Err(format!(
                        "settled {settled} run(s); {} could not be settled: {}",
                        skipped.len(),
                        skipped.join("; ")
                    ));
                }
                Ok(format!("settled {settled} stranded run(s)"))
            },
        }
    }

    // ---------------------------------------------------------------- search

    /// The FTS5 sidecar is a derived index over the ledger. A partial write, a
    /// restored ledger, or a schema change can leave it permanently wrong:
    /// search silently returns nothing and no command can fix it. The chunk
    /// table keeps the indexed text, so the repair rebuilds the FTS contents
    /// from it rather than leaving an empty table behind.
    fn search_index() -> Fixer {
        Fixer {
            check: "search-index",
            diagnose: |dd| {
                let path = dd.join("ledger.db");
                if !path.exists() {
                    return Ok(None);
                }
                // Healthy means the index answers a MATCH at all. A broken index
                // raises; a merely empty one returns nothing, which is why this
                // probes for a query working rather than for rows existing.
                match pantheon_storage::search_index_health(&path) {
                    Ok(healthy) if healthy => Ok(None),
                    Ok(_) => Ok(Some("FTS index is missing or empty".to_string())),
                    Err(e) => Ok(Some(format!("FTS index is unusable: {}", e.cause))),
                }
            },
            repair: |dd| {
                let path = dd.join("ledger.db");
                let backup = backup_file(&path, "ledger.db")?;
                let restored = pantheon_storage::recreate_search_index(&path)
                    .map_err(|e| format!("recreate failed: {}", e.cause))?;
                Ok(format!(
                    "rebuilt the FTS index and restored {restored} indexed chunk(s) \
                     from the chunk store. Backup: {}",
                    backup.display()
                ))
            },
        }
    }

    /// Copy `name` next to itself with a timestamp suffix. Fails rather than
    /// overwriting an existing backup.
    fn backup_file(path: &Path, name: &str) -> Result<PathBuf, String> {
        if !path.exists() {
            return Err(format!(
                "{} does not exist, nothing to back up",
                path.display()
            ));
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dest = path.with_file_name(format!("{name}.{stamp}.bak"));
        if dest.exists() {
            return Err(format!("backup {} already exists", dest.display()));
        }
        std::fs::copy(path, &dest)
            .map_err(|e| format!("cannot back up {}: {e}", path.display()))?;
        Ok(dest)
    }
}

pub mod schedule_self_heal {
    //! Self-healing scheduled task runs.
    //!
    //! When a scheduled task's run fails, the outcome sink built by
    //! [`SelfHealer`] investigates the failure in a bounded agent session,
    //! retries the task exactly once when the cause was fixed, and always
    //! leaves a durable, user-visible ledger event:
    //! [`Event::ScheduledTaskFailed`] or [`Event::ScheduledTaskRecovered`].
    //!
    //! This is per-task runtime handling, separate from the nightly repair
    //! sweep (which repairs MCP servers / tools / scheduled-task *configs*
    //! themselves on a schedule). The investigator may fix the underlying
    //! cause (config, broken dependency, stale state) but never touches
    //! other scheduled jobs, never starts schedulers or daemons, and never
    //! approves anything on the user's behalf.
    //!
    //! Kill switch: `PANTHEON_SCHEDULER_SELF_HEAL=0` (or `false`/`no`/`off`)
    //! disables the investigate+retry pipeline. Failure recording and the
    //! user-visible alert stay on - a silenced failure with no alert would
    //! be worse than no self-heal at all.

    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use pantheon_agent::{Budget, LoopOutcome};
    use pantheon_api::capability::Policy;
    use pantheon_api::events::Event;
    use pantheon_api::logging::now_ms;
    use pantheon_gateway::scheduler::{OutcomeSink, TaskOutcome};
    use pantheon_runtime::session::Session;
    use pantheon_scheduler::{Job, JobRunStats, RunHistory, RunOutcome, TemplateStore};
    use pantheon_storage::Ledger;

    use crate::config;
    use crate::schedule::{load_schedulable, run_job_now, JobRunReport};

    /// Investigator wall-clock bound. A diagnosis that needs longer than
    /// this is a hung investigation, not a diagnosis.
    const INVESTIGATE_TIMEOUT: Duration = Duration::from_secs(300);
    /// Cap on alert detail text so one bad run cannot bloat a ledger row.
    const MAX_DETAIL_CHARS: usize = 2000;

    /// Verdict from the investigator session.
    #[derive(Debug, Clone)]
    pub struct HealVerdict {
        /// True when the investigator found and fixed the root cause.
        pub fixed: bool,
        /// Short paragraph: what failed, what changed, what to watch.
        pub summary: String,
        /// The investigator run's id, so the alert can attach to a run even
        /// when the original job type creates none (e.g. the nightly pass).
        pub run_id: String,
    }

    /// Input to the investigator. Test seam: production builds a real
    /// bounded [`Session`]; tests inject a fake.
    pub struct InvestigateInput<'a> {
        pub data_dir: &'a Path,
        pub job: &'a Job,
        pub task_text: &'a str,
        pub error: &'a str,
        pub stats: Option<&'a JobRunStats>,
    }

    /// The investigator: diagnose + fix, return a verdict. Production
    /// implementation runs a bounded agent session; tests inject a fake.
    pub type InvestigateFn =
        Arc<dyn for<'a> Fn(&'a InvestigateInput<'a>) -> HealVerdict + Send + Sync>;

    /// The retry: re-run the task once. Production delegates to
    /// [`run_job_now`]; tests inject a fake. Takes `(job, task_text,
    /// data_dir)`.
    pub type RetryFn = Arc<dyn Fn(&Job, &str, &Path) -> JobRunReport + Send + Sync>;

    /// Self-heal kill switch. On by default; `PANTHEON_SCHEDULER_SELF_HEAL`
    /// set to `0`/`false`/`no`/`off` disables investigate+retry. Failure
    /// recording and the user-visible alert are unaffected.
    pub(crate) fn scheduler_self_heal_enabled() -> bool {
        match std::env::var("PANTHEON_SCHEDULER_SELF_HEAL") {
            Err(_) => true,
            Ok(v) => !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
        }
    }

    /// Builds the outcome sink that records every fired run and heals the
    /// failed ones. Cheap to construct; share one per scheduler loop.
    pub struct SelfHealer {
        data_dir: PathBuf,
        heal_enabled: bool,
        investigator: InvestigateFn,
        retry: RetryFn,
    }

    impl SelfHealer {
        /// Production constructor: real investigator session, real retry,
        /// kill switch read from the environment.
        pub fn new(data_dir: &Path) -> Arc<Self> {
            Self::with_options(
                data_dir,
                scheduler_self_heal_enabled(),
                Arc::new(investigate_real),
                Arc::new(|job, task, dd| run_job_now(task, job, dd)),
            )
        }

        /// Test seam: explicit kill switch, investigator, and retry.
        pub fn with_options(
            data_dir: &Path,
            heal_enabled: bool,
            investigator: InvestigateFn,
            retry: RetryFn,
        ) -> Arc<Self> {
            Arc::new(Self {
                data_dir: data_dir.to_path_buf(),
                heal_enabled,
                investigator,
                retry,
            })
        }

        /// The outcome sink: records every fired run into run history,
        /// then investigates+retries+alerts on failures. Runs on the
        /// watcher's thread (one per fired run); the one-shot `tick` path
        /// joins those watchers, so healing completes before exit there.
        pub fn outcome_sink(self: &Arc<Self>) -> OutcomeSink {
            let me = Arc::clone(self);
            Arc::new(
                move |job_id: &str, driver_outcome: RunOutcome, task: Option<TaskOutcome>| {
                    me.handle_outcome(job_id, driver_outcome, task);
                },
            )
        }

        fn handle_outcome(&self, job_id: &str, driver: RunOutcome, task: Option<TaskOutcome>) {
            // Task-level truth wins when the executor reported one: a task
            // that errored while its thread returned normally is a failure,
            // and its error text is what history (and the alert) carries.
            // No task report (panic / timeout abandonment) falls back to the
            // driver's outcome.
            let (record_outcome, error, run_id) = match &task {
                Some(t) if t.error.is_some() => {
                    (RunOutcome::Failed, t.error.clone(), t.run_id.clone())
                }
                Some(t) => (driver, None, t.run_id.clone()),
                None => (driver, None, String::new()),
            };

            if let Err(e) = self.record(job_id, record_outcome, error.clone()) {
                eprintln!("self-heal: could not record run history for {job_id}: {e}");
            }

            let failed = matches!(
                record_outcome,
                RunOutcome::Failed | RunOutcome::TimedOut | RunOutcome::Panicked
            );
            if !failed {
                return;
            }
            let error_text = error.unwrap_or_default();

            if !self.heal_enabled {
                self.alert_failed(
                    job_id,
                    &run_id,
                    &error_text,
                    "self-heal is disabled (PANTHEON_SCHEDULER_SELF_HEAL). Manual attention needed.",
                );
                return;
            }

            // A panic inside the pipeline must not swallow the failure:
            // it becomes the not-fixed alert.
            let healed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.heal(job_id, &run_id, &error_text)
            }));
            match healed {
                Ok(outcome) => self.alert(job_id, &run_id, &error_text, &outcome),
                Err(_) => self.alert_failed(
                    job_id,
                    &run_id,
                    &error_text,
                    "the self-heal pipeline panicked; the original failure is unhandled.",
                ),
            }
        }

        fn record(
            &self,
            job_id: &str,
            outcome: RunOutcome,
            error: Option<String>,
        ) -> Result<(), String> {
            let mut history = RunHistory::open(&self.data_dir)?;
            history.record(job_id, outcome, error, now_ms())
        }

        /// Investigate the failure, fix if possible, retry once, and
        /// describe the result. Never silently skips: every path returns a
        /// [`HealOutcome`] that the caller turns into a ledger alert.
        fn heal(&self, job_id: &str, run_id: &str, error: &str) -> HealOutcome {
            let jobs = match load_schedulable(&self.data_dir) {
                Ok(j) => j,
                Err(e) => {
                    return HealOutcome::not_fixed(
                        format!("could not load the schedule to investigate: {e}"),
                        run_id,
                    );
                }
            };
            let Some(entry) = jobs.iter().find(|j| j.job.id == job_id) else {
                return HealOutcome::not_fixed(
                    format!("job {job_id} is gone from the schedule; nothing to investigate"),
                    run_id,
                );
            };
            let job = &entry.job;
            // Repair spend cap: `[repair].max_repairs_per_task_per_day`
            // (default 3). A flapping task must not burn API budget on
            // investigator sessions without bound. Exhausted -> skip the
            // investigation and return not-fixed, which the caller turns into
            // the same disable-with-escalation alert as an unfixable failure.
            let max_repairs = crate::config::Config::load_or_report(&self.data_dir)
                .map(|c| {
                    c.repair
                        .as_ref()
                        .map(|r| r.repair_cap_per_day())
                        .unwrap_or(3)
                })
                .unwrap_or(3);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let mut budget =
                match pantheon_scheduler::repair_budget::RepairBudget::open(&self.data_dir) {
                    Ok(b) => b,
                    Err(e) => {
                        return HealOutcome::not_fixed(
                            format!("repair budget unavailable, skipping investigation: {e}"),
                            run_id,
                        );
                    }
                };
            if !budget.try_consume(job_id, max_repairs, now_ms) {
                return HealOutcome::not_fixed(
                    format!(
                        "repair budget exhausted: job {job_id} already used its {max_repairs} repair investigations today"
                    ),
                    run_id,
                );
            }
            let task_text = job.resolve_task(&TemplateStore::open(&self.data_dir));
            let stats = match RunHistory::open(&self.data_dir) {
                Ok(h) => h.stats(job_id).cloned(),
                Err(_) => None,
            };

            // The investigation itself is fallible: a broken model config,
            // a parked-on-approval turn, a panic - all become the not-fixed
            // alert, never a silent skip.
            let verdict = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (self.investigator)(&InvestigateInput {
                    data_dir: &self.data_dir,
                    job,
                    task_text: &task_text,
                    error,
                    stats: stats.as_ref(),
                })
            }));
            let verdict = match verdict {
                Ok(v) => v,
                Err(_) => HealVerdict {
                    fixed: false,
                    summary: "the investigator session panicked.".to_string(),
                    run_id: String::new(),
                },
            };

            if !verdict.fixed {
                return HealOutcome::not_fixed(verdict.summary, run_id);
            }

            // Exactly one retry, as a direct call - not through the tick
            // driver - so the retry can never re-enter self-heal.
            let report = (self.retry)(job, &task_text, &self.data_dir);
            let retry_outcome = if report.error.is_none() {
                RunOutcome::Completed
            } else {
                RunOutcome::Failed
            };
            if let Err(e) = self.record(job_id, retry_outcome, report.error.clone()) {
                eprintln!("self-heal: could not record retry outcome for {job_id}: {e}");
            }
            match report.error {
                None => HealOutcome::recovered(verdict.summary, &verdict.run_id),
                Some(retry_error) => HealOutcome::not_fixed(
                    format!(
                        "{}. A single retry was attempted and also failed: {}",
                        verdict.summary, retry_error
                    ),
                    &verdict.run_id,
                ),
            }
        }

        fn alert(&self, job_id: &str, run_id: &str, error: &str, outcome: &HealOutcome) {
            // Prefer the original run id; the investigator's run is the
            // fallback for job types that create no run of their own.
            let rid = pick_run_id(run_id, &outcome.run_id);
            if outcome.recovered {
                let detail = format!(
                    "The task failed with: {}. Investigation: {}. The cause was fixed and the retry ran cleanly.",
                    truncate(error, 500),
                    truncate(&outcome.detail, MAX_DETAIL_CHARS)
                );
                self.append_event(Event::ScheduledTaskRecovered {
                    run_id: rid,
                    job_id: job_id.to_string(),
                    detail,
                });
                eprintln!("self-heal: job {job_id} failed, was fixed, and the retry succeeded");
            } else {
                self.alert_failed(job_id, &rid, error, &outcome.detail);
            }
        }

        fn alert_failed(&self, job_id: &str, run_id: &str, error: &str, detail: &str) {
            let detail = truncate(
                &format!(
                    "The task failed with: {}. Investigation: {}",
                    truncate(error, 500),
                    detail
                ),
                MAX_DETAIL_CHARS,
            );
            self.append_event(Event::ScheduledTaskFailed {
                run_id: run_id.to_string(),
                job_id: job_id.to_string(),
                error: truncate(error, 500),
                detail,
            });
            eprintln!("self-heal: job {job_id} failed and needs attention");
        }

        fn append_event(&self, event: Event) {
            match Ledger::open(&self.data_dir.join("ledger.db")) {
                Ok(ledger) => {
                    if let Err(e) = ledger.append(&event) {
                        eprintln!("self-heal: could not append alert event: {e}");
                    }
                }
                Err(e) => eprintln!("self-heal: could not open ledger for alert: {e}"),
            }
        }
    }

    /// Prefer the investigator's run when the original run id is empty
    /// (e.g. nightly-pass jobs create no run of their own).
    fn pick_run_id(original: &str, investigator_run: &str) -> String {
        if !original.is_empty() {
            original.to_string()
        } else {
            investigator_run.to_string()
        }
    }

    fn truncate(s: &str, max_chars: usize) -> String {
        let mut out: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        if s.chars().count() > max_chars {
            out.push_str("...");
        }
        out
    }

    /// What one heal attempt produced. `detail` is the human-readable
    /// summary that goes into the ledger alert.
    struct HealOutcome {
        recovered: bool,
        detail: String,
        /// The run to attach the alert to when the original has no run id.
        run_id: String,
    }

    impl HealOutcome {
        fn recovered(detail: String, run_id: &str) -> Self {
            Self {
                recovered: true,
                detail,
                run_id: run_id.to_string(),
            }
        }

        fn not_fixed(detail: String, run_id: &str) -> Self {
            Self {
                recovered: false,
                detail,
                run_id: run_id.to_string(),
            }
        }
    }

    /// The production investigator: a bounded, non-spawning agent session
    /// over the failing job's context. Verdict comes from a trailing
    /// `VERDICT: FIXED` / `VERDICT: NOT_FIXED` line; anything unparseable
    /// or erroring is NOT_FIXED - fail closed.
    fn investigate_real(input: &InvestigateInput) -> HealVerdict {
        let file_cfg = config::Config::load_or_report(input.data_dir);
        let model_policy = config::build_scheduled_model_policy(
            file_cfg.as_ref(),
            input.job.provider.clone(),
            input.job.model.clone(),
        );
        let policy = Policy::coder();
        let secrets = config::chat_secrets(file_cfg.as_ref());
        let session =
            match Session::new(input.data_dir.to_path_buf(), policy, model_policy, secrets) {
                Ok(s) => s,
                Err(e) => {
                    return HealVerdict {
                        fixed: false,
                        summary: format!("could not start the investigator session: {e}"),
                        run_id: String::new(),
                    };
                }
            };
        config::apply_tool_enablement(&session, file_cfg.as_ref());
        session.set_budget(Budget {
            max_turns: 12,
            max_tool_calls: 24,
            max_tokens: Some(32_000),
            max_delegate_depth: 0,
            allow_child_spawn: false,

            warn_consecutive_tool_failures: 3,
            max_cost_cents: None,
        });

        let history_line = match input.stats {
            Some(s) => format!(
                "{} consecutive failure(s); last error: {}; {} total runs, {} total failures",
                s.consecutive_failures,
                s.last_error.as_deref().unwrap_or("none"),
                s.total_runs,
                s.total_failures
            ),
            None => "no run history available".to_string(),
        };
        let prompt = format!(
            "A scheduled task run just failed. Investigate the failure, fix the root cause if you can, and report a verdict.\n\
    \n\
    Job id: {job_id}\n\
    Task (as configured):\n\
    {task}\n\
    \n\
    Failure: {error}\n\
    \n\
    Recent history: {history}\n\
    \n\
    Relevant paths (data dir {data_dir}): schedule.json (job definitions), schedule-run-history.json (per-job outcomes). \
    The event ledger is queryable through the `pantheon` CLI; do not write it directly.\n\
    \n\
    Rules:\n\
    1. Diagnose before fixing. Only fix the root cause you actually found.\n\
    2. You may repair config files, restart MCP servers, or correct schedule state - whatever the diagnosis supports.\n\
    3. Do NOT create, modify, pause, or delete any OTHER scheduled jobs. Do NOT start schedulers, daemons, or background services. \
    Do NOT approve anything on the user's behalf.\n\
    4. End your final message with exactly one line `VERDICT: FIXED` or `VERDICT: NOT_FIXED`, then a short paragraph: \
    what failed, what you changed, and what to watch.\n\
    \n\
    Begin.",
            job_id = input.job.id,
            task = input.task_text,
            error = input.error,
            history = history_line,
            data_dir = input.data_dir.display(),
        );

        let run_id = pantheon_runtime::new_run_id();
        let chat_run_id = run_id.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(session.chat(&chat_run_id, &prompt));
        });
        let outcome = match rx.recv_timeout(INVESTIGATE_TIMEOUT) {
            Ok(o) => o,
            Err(_) => {
                return HealVerdict {
                    fixed: false,
                    summary:
                        "the investigator session hit the 5-minute wall clock and was abandoned."
                            .to_string(),
                    run_id: String::new(),
                };
            }
        };

        match outcome {
            Ok(LoopOutcome::Answered { text, .. }) => {
                let fixed = parse_verdict(&text);
                HealVerdict {
                    fixed: fixed.unwrap_or(false),
                    run_id: run_id.clone(),
                    summary: if fixed.is_some() {
                        verdict_summary(&text)
                    } else {
                        format!(
                            "the investigator finished without a parseable verdict; treated as NOT_FIXED. Last words: {}",
                            truncate(&text, 500)
                        )
                    },
                }
            }
            Ok(other) => HealVerdict {
                fixed: false,
                summary: format!("the investigator session ended without an answer ({other:?}); treated as NOT_FIXED"),
                run_id: run_id.clone(),
            },
            Err(e) => HealVerdict {
                fixed: false,
                summary: format!("the investigator session errored: {e}"),
                run_id,
            },
        }
    }

    /// The investigator's verdict: the last `VERDICT: FIXED` /
    /// `VERDICT: NOT_FIXED` line wins; nothing found means fail closed.
    fn parse_verdict(text: &str) -> Option<bool> {
        text.lines().rev().find_map(|line| match line.trim() {
            "VERDICT: FIXED" => Some(true),
            "VERDICT: NOT_FIXED" => Some(false),
            _ => None,
        })
    }

    /// The paragraph after the verdict line; the whole text when the
    /// verdict is the last line.
    fn verdict_summary(text: &str) -> String {
        let mut after = false;
        let mut out = Vec::new();
        for line in text.lines() {
            if after {
                out.push(line);
            } else if matches!(line.trim(), "VERDICT: FIXED" | "VERDICT: NOT_FIXED") {
                after = true;
            }
        }
        let summary = out.join("\n").trim().to_string();
        if summary.is_empty() {
            truncate(text.trim(), 500)
        } else {
            truncate(&summary, 500)
        }
    }
}

pub mod schedule {
    //! `pantheon schedule` - durable cron/interval/one-shot task scheduling.
    //!
    //! Jobs live in the data dir (`schedule.json`) and are read/written through
    //! the scheduler core's [`load_jobs`]/[`save_jobs`]. Occurrence idempotency
    //! is handled by the scheduler's `DurableClaimLedger` over the `ClaimStore`.
    //!
    //! Jobs created with `--template <name>` re-render their prompt from the
    //! template at fire time ([`Job::resolve_task`]), so editing the template
    //! updates every job built from it. The rendered snapshot in `task` stays
    //! as the fallback when the template is deleted or fails to render.

    use crate::schedule_self_heal::SelfHealer;
    use pantheon_api::logging::now_ms;
    use pantheon_gateway::schedule_delivery::{self, Deliver};
    use pantheon_gateway::scheduler::{
        ExecuteFn, FireOutcome, SchedulerLoop, TaskOutcome, TickReport,
    };
    use pantheon_scheduler::{
        after_manual_run, expand_template, job_store_path, load_jobs, load_jobs_with_warnings,
        parse_duration, retention, update_jobs, Job, OverlapPolicy, RunOutcome, ScheduleKind,
        ScheduleTemplate, ScheduledJob, TemplateSchedule, TemplateStore, TemplateVar,
    };
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    /// Parse `--at <time>` into epoch millis:
    /// - epoch millis (`1790845200000`),
    /// - an RFC3339 datetime (`2026-10-01T09:00:00+01:00`),
    /// - a relative offset from now (`+30m`, `+2h`, `+1d`; same units as
    ///   `--every`).
    fn parse_at(s: &str) -> Result<i64, String> {
        let s = s.trim();
        if let Ok(ms) = s.parse::<i64>() {
            return Ok(ms);
        }
        if let Some(rest) = s.strip_prefix('+') {
            let delta = parse_duration(rest)?;
            return Ok(now_ms().saturating_add(delta as i64));
        }
        chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.timestamp_millis())
            .map_err(|_| {
                format!(
                    "expected epoch millis, an RFC3339 datetime, or +<duration> (e.g. +30m), got {s:?}"
                )
            })
    }

    /// Public wrapper for the TUI's /schedule command. Returns an empty vec
    /// when no schedule file exists; reports corruption as an error string.
    pub fn load_jobs_public(data_dir: &Path) -> Vec<ScheduledJob> {
        load_jobs(data_dir).unwrap_or_default()
    }

    /// Load jobs or exit. A corrupt schedule file is a stop, not an empty list.
    fn load_or_exit(data_dir: &Path) -> Vec<ScheduledJob> {
        let (result, warnings) = load_jobs_with_warnings(data_dir);
        for w in warnings {
            eprintln!("schedule: warning: {w}");
        }
        match result {
            Ok(j) => j,
            Err(e) => {
                eprintln!("schedule: {e}");
                eprintln!(
                    "fix: delete or repair {}",
                    job_store_path(data_dir).display()
                );
                std::process::exit(1);
            }
        }
    }

    /// Schedule a task to run repeatedly.
    pub fn cmd_schedule(args: &[String], data_dir: &Path) {
        if args.len() < 3 {
            eprintln!("usage: pantheon schedule <task> (--every 30m | --cron '0 9 * * *' | --at <time>) [--agent nyx] [--timeout 10m] [--overlap skip|replace|queue] [--deliver telegram|discord|notify|file:<path>|log] [--model M] [--provider P]");
            eprintln!("       pantheon schedule create --template <name> [--var k=v ...] [--deliver ...] [--every ...|--cron ...|--at ...]");
            eprintln!("       pantheon schedule template list|get <name>|save|delete  - prompt templates (<data_dir>/templates.json)");
            eprintln!("       pantheon schedule nightly (--every 30m | --cron '0 3 * * *' | --at <time>)  - scheduled nightly self-improvement pass (default: [nightly] cron)");
            eprintln!("       pantheon schedule reflect ... | pantheon schedule consolidate ...  - legacy aliases for `schedule nightly`");
            eprintln!("       pantheon schedule list|pause|resume|cancel|run|tick|prune <id>");
            eprintln!();
            eprintln!("--at <time>: epoch millis, RFC3339 ('2026-10-01T09:00:00+01:00'), or relative +30m/+2h/+1d - fires once, then the job is removed");
            eprintln!("model rule: --model/--provider pin (or the template's `model` var) > the [scheduled] auxiliary model > never the interactive default.");
            eprintln!("deliver targets: log (default) | telegram (needs PANTHEON_TELEGRAM_BOT_TOKEN + PANTHEON_DELIVER_TELEGRAM_TO) | discord (needs PANTHEON_DISCORD_TOKEN + PANTHEON_DELIVER_DISCORD_TO) | notify (notify-send) | file:<path>");
            std::process::exit(2);
        }

        let subcommand = args[2].as_str();
        if subcommand == "nightly" || subcommand == "reflect" || subcommand == "consolidate" {
            cmd_schedule_nightly(&args[3..], data_dir);
            return;
        }
        if subcommand == "template" {
            cmd_schedule_template(&args[3..], data_dir);
            return;
        }
        if subcommand == "create" {
            // `schedule create` is the explicit form of `schedule <task>`; both
            // accept the same flags (including --template/--deliver).
            match build_create_job(&args[3..], data_dir) {
                Err(e) => {
                    eprintln!("error: {e}");
                    eprintln!("not scheduled: fix the arguments and retry");
                    std::process::exit(2);
                }
                Ok(stored) => persist_new_job(stored, data_dir),
            }
            return;
        }
        if matches!(
            subcommand,
            "list" | "pause" | "resume" | "cancel" | "run" | "tick" | "prune"
        ) {
            handle_subcommand(&args[2..], data_dir);
            return;
        }

        // Create: pantheon schedule <task> [--every|N<unit>] [--agent NAME] [--cron EXPR] [--at TIME]
        //          [--model M] [--provider P] [--timeout 10m] [--overlap skip|replace|queue]
        //          [--deliver T] [--template NAME] [--var k=v]
        match build_create_job(&args[2..], data_dir) {
            Err(e) => {
                eprintln!("error: {e}");
                eprintln!("not scheduled: fix the arguments and retry");
                std::process::exit(2);
            }
            Ok(stored) => persist_new_job(stored, data_dir),
        }
    }

    /// Persist a newly created job and print the confirmation line. Shared by
    /// `schedule <task>` and `schedule create`.
    fn persist_new_job(stored: ScheduledJob, data_dir: &Path) {
        let job_id = stored.job.id.clone();
        let kind = stored.job.kind.clone();
        let deliver = stored.job.deliver.clone();
        // Single locked rewrite (load/mutate/save under the store lock) so a
        // concurrent dashboard edit can't be clobbered by this CLI process.
        if let Err(e) = update_jobs(data_dir, |jobs| {
            jobs.push(stored);
            Ok(())
        }) {
            eprintln!("save failed: {e}");
            std::process::exit(1);
        }

        let deliver_note = deliver
            .as_deref()
            .map(|d| format!(" → {d}"))
            .unwrap_or_default();
        println!(
            "scheduled {} [{}]{deliver_note} - runs | cancel: pantheon schedule cancel {}",
            job_id,
            format_kind(&kind),
            job_id
        );
    }

    /// `pantheon schedule template <list|get|save|delete>`: built-ins plus the
    /// user's `<data_dir>/templates.json` overlay.
    fn cmd_schedule_template(args: &[String], data_dir: &Path) {
        match args.first().map(String::as_str) {
            Some("list") => {
                let store = TemplateStore::open(data_dir);
                let mut any = false;
                for t in store.list() {
                    any = true;
                    let sched = format_template_schedule(&t.schedule);
                    let vars = if t.vars.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "  vars: {}",
                            t.vars
                                .iter()
                                .map(|v| v.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    println!("{}\n  {}  ({sched}){vars}", t.name, t.description);
                }
                if !any {
                    println!("no templates");
                }
            }
            Some("get") => {
                let name = args.get(1).unwrap_or_else(|| {
                    eprintln!("usage: pantheon schedule template get <name>");
                    std::process::exit(2);
                });
                let store = TemplateStore::open(data_dir);
                let t = store.get(name).unwrap_or_else(|| {
                    eprintln!("unknown template '{name}'; see `pantheon schedule template list`");
                    std::process::exit(1);
                });
                let builtin = if store.is_builtin(name) {
                    " (built-in)"
                } else {
                    ""
                };
                println!(
                    "{name}{builtin}\n  {}\n  schedule: {}",
                    t.description,
                    format_template_schedule(&t.schedule)
                );
                println!("  prompt:");
                for line in t.prompt.lines() {
                    println!("    {line}");
                }
                if t.vars.is_empty() {
                    println!("  vars: none");
                } else {
                    for v in &t.vars {
                        let def = v
                            .default
                            .as_deref()
                            .map(|d| format!(" [default: {d}]"))
                            .unwrap_or_default();
                        println!("  var {}: {}{def}", v.name, v.question);
                    }
                }
            }
            Some("save") => cmd_schedule_template_save(&args[1..], data_dir),
            Some("delete") => {
                let name = args.get(1).unwrap_or_else(|| {
                    eprintln!("usage: pantheon schedule template delete <name>");
                    std::process::exit(2);
                });
                let mut store = TemplateStore::open(data_dir);
                match store.delete(name) {
                    Ok(()) => println!("deleted template '{name}'"),
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            _ => {
                eprintln!("usage: pantheon schedule template list");
                eprintln!("       pantheon schedule template get <name>");
                eprintln!("       pantheon schedule template save --name <name> [--desc <text>] (--every 30m | --cron '<expr>') --prompt '<text>' [--var name:question[:default] ...]");
                eprintln!("       pantheon schedule template delete <name>");
                std::process::exit(2);
            }
        }
    }

    fn format_template_schedule(schedule: &TemplateSchedule) -> String {
        match schedule {
            TemplateSchedule::Every(d) => format!("every {d}"),
            TemplateSchedule::Cron(e) => format!("cron '{e}'"),
        }
    }

    /// `pantheon schedule template save --name <n> [--desc <d>]
    /// (--every 30m | --cron '<expr>') --prompt '<text>'
    /// [--var name:question[:default] ...]`
    ///
    /// A bad cron, a bad duration, or a missing required flag is a loud error
    /// here - the template must be usable the moment it is saved.
    fn cmd_schedule_template_save(args: &[String], data_dir: &Path) {
        let usage = "usage: pantheon schedule template save --name <name> [--desc <text>] (--every 30m | --cron '<expr>') --prompt '<text>' [--var name:question[:default] ...]";
        let mut name: Option<String> = None;
        let mut desc = String::new();
        let mut every: Option<String> = None;
        let mut cron: Option<String> = None;
        let mut prompt: Option<String> = None;
        let mut var_specs: Vec<String> = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--name" => {
                    i += 1;
                    if i < args.len() {
                        name = Some(args[i].clone());
                    }
                }
                "--desc" => {
                    i += 1;
                    if i < args.len() {
                        desc = args[i].clone();
                    }
                }
                "--every" => {
                    i += 1;
                    if i < args.len() {
                        every = Some(args[i].clone());
                    }
                }
                "--cron" => {
                    i += 1;
                    if i < args.len() {
                        cron = Some(args[i].clone());
                    }
                }
                "--prompt" => {
                    i += 1;
                    if i < args.len() {
                        prompt = Some(args[i].clone());
                    }
                }
                "--var" => {
                    i += 1;
                    if i < args.len() {
                        var_specs.push(args[i].clone());
                    }
                }
                other => {
                    eprintln!("unknown flag: {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        let name = name.unwrap_or_else(|| {
            eprintln!("{usage}\nerror: --name is required");
            std::process::exit(2);
        });
        let prompt = prompt.unwrap_or_else(|| {
            eprintln!("{usage}\nerror: --prompt is required");
            std::process::exit(2);
        });
        let schedule = match (cron, every) {
            (Some(e), _) => TemplateSchedule::Cron(e),
            (None, Some(d)) => {
                if let Err(e) = parse_duration(&d) {
                    eprintln!("error: bad --every: {e}");
                    std::process::exit(2);
                }
                TemplateSchedule::Every(d)
            }
            (None, None) => {
                eprintln!("{usage}\nerror: need --every <duration> or --cron <expr>");
                std::process::exit(2);
            }
        };
        let mut vars = Vec::new();
        for spec in &var_specs {
            // name:question[:default]; extra colons stay in the default.
            let mut parts = spec.splitn(3, ':');
            let vname = parts.next().unwrap_or("").trim();
            let question = parts.next().unwrap_or("").trim();
            let default = parts.next().map(str::trim).filter(|d| !d.is_empty());
            if vname.is_empty() || question.is_empty() {
                eprintln!("error: bad --var {spec:?}: want name:question[:default]");
                std::process::exit(2);
            }
            vars.push(TemplateVar {
                name: vname.to_string(),
                question: question.to_string(),
                default: default.map(str::to_string),
            });
        }
        let mut store = TemplateStore::open(data_dir);
        match store.save(ScheduleTemplate {
            name: name.clone(),
            description: desc,
            schedule,
            prompt,
            vars,
        }) {
            Ok(()) => println!("saved template '{name}'"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    /// `pantheon schedule nightly [--every 30m | --cron '0 3 * * *' | --at <time>]`:
    /// a scheduled unified nightly pass. Registers a normal stored job whose
    /// task is the nightly marker; `run_job_now` runs the pass directly
    /// instead of a chat turn. `schedule reflect` / `schedule consolidate`
    /// are legacy aliases for this command.
    fn cmd_schedule_nightly(args: &[String], data_dir: &Path) {
        match build_nightly_job(data_dir, args) {
            Err(e) => {
                eprintln!("error: {e}");
                eprintln!("not scheduled: fix the arguments and retry");
                std::process::exit(2);
            }
            Ok(stored) => {
                let job_id = stored.job.id.clone();
                // Single locked rewrite (load/mutate/save under the store lock)
                // so a concurrent dashboard edit can't be clobbered.
                if let Err(e) = update_jobs(data_dir, |jobs| {
                    jobs.push(stored);
                    Ok(())
                }) {
                    eprintln!("save failed: {e}");
                    std::process::exit(1);
                }
                println!(
                    "scheduled nightly {job_id} - runs | cancel: pantheon schedule cancel {job_id}"
                );
            }
        }
    }

    /// Testable core of `pantheon schedule nightly`: the default-cron
    /// resolution, without the persistence.
    pub fn build_nightly_job(data_dir: &Path, args: &[String]) -> Result<ScheduledJob, String> {
        let file_cfg = crate::config::Config::load_or_report(data_dir);
        let mut parsed = parse_create_args(args);
        if !parsed.task.is_empty() {
            return Err(
                "`schedule nightly` takes no task text: it always runs a nightly pass".into(),
            );
        }
        // No schedule given = the `[nightly] cron` value (default
        // "0 3 * * *").
        if parsed.every.is_none() && parsed.cron.is_none() && parsed.at.is_none() {
            parsed.cron = Some(crate::config::nightly_cron(file_cfg.as_ref()));
        }
        parsed.task = NIGHTLY_TASK_MARKER.to_string();
        build_job_from_create(&parsed)
    }

    /// Marker task for nightly jobs. `run_job_now` intercepts it and runs
    /// the unified nightly pass instead of a chat turn - a scheduled
    /// nightly job never spends an agent turn. Re-exported from
    /// [`pantheon_scheduler`] so every client shares one marker string.
    pub use pantheon_scheduler::NIGHTLY_TASK_MARKER;

    /// Marker task for reflection jobs (legacy). `run_job_now` intercepts it
    /// and runs the unified nightly pass instead of a chat turn.
    pub const REFLECT_TASK_MARKER: &str = "__pantheon_reflect__";

    /// Marker task for consolidation jobs (legacy). `run_job_now`
    /// intercepts it and runs the unified nightly pass instead of a chat
    /// turn.
    pub const CONSOLIDATE_TASK_MARKER: &str = "__pantheon_consolidate__";

    /// Build the stored job for `schedule <task>` / `schedule create`, resolving
    /// `--template` first: the template's rendered prompt becomes the task and
    /// its default schedule fills in when no `--every`/`--cron`/`--at` is given.
    /// A positional task always overrides the template prompt.
    fn build_create_job(args: &[String], data_dir: &Path) -> Result<ScheduledJob, String> {
        let mut parsed = parse_create_args(args);
        if let Some(tname) = parsed.template.clone() {
            let store = TemplateStore::open(data_dir);
            let t = store.get(&tname).ok_or_else(|| {
                format!("unknown template '{tname}'; see `pantheon schedule template list`")
            })?;
            let mut vars: HashMap<String, String> = parsed.vars.iter().cloned().collect();
            let has_schedule =
                parsed.every.is_some() || parsed.cron.is_some() || parsed.at.is_some();
            // Interactive missing-var resolution: prompt on a TTY, fail loud
            // otherwise. An empty answer is an error, like before.
            let mut prompt = |name: &str, question: &str| -> Result<String, String> {
                let answer = prompt_for_var(question, None)
                    .map_err(|_| format!("template '{tname}' needs --var {name}=<value>"))?;
                if answer.is_empty() {
                    return Err(format!("template '{tname}' needs --var {name}=<value>"));
                }
                Ok(answer)
            };
            expand_template(
                t,
                pantheon_scheduler::templates::TemplateParams {
                    vars: &mut vars,
                    model: &mut parsed.model,
                    provider: &mut parsed.provider,
                    task: &mut parsed.task,
                    every: &mut parsed.every,
                    cron: &mut parsed.cron,
                },
                has_schedule,
                &mut prompt,
            )?;
            // Remember the template on the job: at fire time the prompt is
            // re-rendered from it, so editing the template updates the job.
            // `task` stays as the snapshot for when the template is later
            // deleted or fails to render.
            parsed.template = Some(tname);
            parsed.template_vars = vars;
        }
        if parsed.task.is_empty() {
            return Err(
                "need a task: pantheon schedule <task> --every 30m|--cron '0 9 * * *'|--at <time> (or --template <name>)"
                    .into(),
            );
        }
        build_job_from_create(&parsed)
    }

    /// Ask the operator for a template variable. Uses the default on empty
    /// input; errors when stdin is not a TTY (scripts must pass --var).
    fn prompt_for_var(question: &str, default: Option<&str>) -> Result<String, String> {
        use std::io::{BufRead, IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            return Err("no TTY to prompt on".into());
        }
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        match default {
            Some(d) if !d.is_empty() => write!(out, "{question} [{d}]: "),
            _ => write!(out, "{question}: "),
        }
        .map_err(|e| format!("prompt: {e}"))?;
        out.flush().map_err(|e| format!("prompt: {e}"))?;
        drop(out);
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| format!("prompt: {e}"))?;
        Ok(line.trim().to_string())
    }

    /// Build the stored job from `schedule create` arguments, validating
    /// everything before anything is persisted.
    ///
    /// A bad cron expression used to be stored without a murmur and then
    /// silently never fire. Now it is rejected here, with the field and the
    /// reason, so a broken schedule is a loud error at registration instead
    /// of a quiet no-show at 3am.
    fn build_job_from_create(parsed: &CreateArgs) -> Result<ScheduledJob, String> {
        // Kind precedence: --cron > --every > --at. One of them is required:
        // a job with no schedule could never fire and must not be stored.
        let kind = if let Some(expr) = &parsed.cron {
            ScheduleKind::Cron { expr: expr.clone() }
        } else if let Some(dur) = &parsed.every {
            match parse_duration(dur) {
                Ok(ms) => ScheduleKind::Interval { every_ms: ms },
                Err(e) => return Err(format!("bad --every: {e}")),
            }
        } else if let Some(at) = &parsed.at {
            match parse_at(at) {
                Ok(at_ms) => ScheduleKind::OneShot { at_ms },
                Err(e) => return Err(format!("bad --at: {e}")),
            }
        } else {
            return Err("need --every <duration>, --cron <expr>, or --at <time>".into());
        };

        // Validate the delivery target now: a typo must fail here, not vanish
        // into a job that never delivers.
        if let Some(d) = &parsed.deliver {
            Deliver::parse(d).map_err(|e| format!("bad --deliver: {e}"))?;
        }

        let job_id = format!("job_{}", pantheon_runtime::new_run_id());
        let mut job = Job::new(&job_id, kind, &parsed.agent);
        job.task = parsed.task.clone();
        job.template = parsed.template.clone();
        job.template_vars = parsed.template_vars.clone();
        // Registration-time validation: reject the broken expression now.
        job.validate()
            .map_err(|e| format!("invalid --cron expression: {e}"))?;
        if let Some(m) = parsed.model.as_deref() {
            job.pin_model(m, parsed.provider.as_deref())
                .map_err(|e| format!("bad pin: {e}"))?;
        } else if parsed.provider.is_some() {
            return Err("--provider without --model pins nothing; add --model to pin".into());
        }
        job.timeout_secs = match parsed.timeout.as_deref() {
            None => None,
            Some(d) => {
                let ms = parse_duration(d).map_err(|e| format!("bad --timeout: {e}"))?;
                let secs = ms / 1000;
                if secs == 0 {
                    return Err("bad --timeout: must be at least 1s".into());
                }
                Some(secs)
            }
        };
        job.overlap = match parsed.overlap.as_deref() {
            None => OverlapPolicy::default(),
            Some(o) => o
                .parse::<OverlapPolicy>()
                .map_err(|e| format!("bad --overlap: {e}"))?,
        };
        job.deliver = parsed.deliver.clone();
        Ok(ScheduledJob {
            job,
            last_run: None,
        })
    }

    fn handle_subcommand(parts: &[String], data_dir: &Path) {
        match parts[0].as_str() {
            "list" => {
                let jobs = load_or_exit(data_dir);
                if jobs.is_empty() {
                    println!("no scheduled jobs");
                    return;
                }
                for j in &jobs {
                    let job = &j.job;
                    let status = if job.paused { "paused" } else { "active" };
                    let pin = match (&job.model, &job.provider) {
                        (Some(m), Some(p)) => format!("  model: {m} via {p}"),
                        (Some(m), None) => format!("  model: {m}"),
                        _ => String::new(),
                    };
                    let deliver = job
                        .deliver
                        .as_deref()
                        .map(|d| format!("  deliver: {d}"))
                        .unwrap_or_default();
                    let template = job
                        .template
                        .as_deref()
                        .map(|t| format!("  template: {t}"))
                        .unwrap_or_default();
                    println!(
                        "{}  {}  {}  [{}]{}{}{}  last: {}  | cancel: pantheon schedule cancel {}",
                        // Full id, not a byte-prefix: ids are `job_run_<ts>_<n>`,
                        // so the first 8 bytes are the shared `job_run_` and
                        // the truncated form was identical for every job.
                        job.id.as_str(),
                        status,
                        format_kind(&job.kind),
                        job.task,
                        pin,
                        deliver,
                        template,
                        format_last(j.last_run),
                        job.id
                    );
                }
            }
            "pause" | "resume" | "cancel" => {
                let id = if parts.len() > 1 {
                    &parts[1]
                } else {
                    eprintln!("usage: pantheon schedule {} <id>", parts[0]);
                    std::process::exit(2);
                };
                // Single locked rewrite (load/mutate/save under the store lock)
                // so a concurrent dashboard edit can't be clobbered.
                let outcome = update_jobs(data_dir, |jobs| -> Result<bool, String> {
                    let Some(pos) = jobs.iter().position(|j| j.job.id == *id) else {
                        return Ok(false);
                    };
                    match parts[0].as_str() {
                        "pause" => jobs[pos].job.paused = true,
                        "resume" => jobs[pos].job.paused = false,
                        // "cancel" removes; the outer guard allows only
                        // pause|resume|cancel here.
                        _ => {
                            jobs.remove(pos);
                        }
                    }
                    Ok(true)
                });
                match outcome {
                    Ok(true) => println!("{} {}", parts[0], id),
                    Ok(false) => {
                        eprintln!("not found: {id}");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("schedule: {e}");
                        std::process::exit(1);
                    }
                }
            }
            "run" => {
                let id = if parts.len() > 1 {
                    &parts[1]
                } else {
                    eprintln!("usage: pantheon schedule run <id>");
                    std::process::exit(2);
                };
                let jobs = load_or_exit(data_dir);
                match jobs.iter().find(|j| j.job.id == *id) {
                    Some(j) => {
                        // The prompt is re-rendered from the template at fire
                        // time, so edits to the template apply to already
                        // scheduled jobs.
                        let is_oneshot = matches!(j.job.kind, ScheduleKind::OneShot { .. });
                        let task = j.job.resolve_task(&TemplateStore::open(data_dir));
                        // A manual `run` keeps the old hard-fail behavior: the
                        // tick loop instead logs and continues with other jobs.
                        let report = run_job_now(&task, &j.job, data_dir);
                        if let Some(e) = report.error {
                            eprintln!("{e}");
                            std::process::exit(1);
                        }
                        // stays for retry. Single locked rewrite so a concurrent
                        // dashboard edit can't be clobbered.
                        if let Err(e) = update_jobs(data_dir, |jobs| {
                            after_manual_run(jobs, id, now_ms());
                            Ok(())
                        }) {
                            eprintln!("warning: could not update job store after run: {e}");
                        } else if is_oneshot {
                            println!("removed one-shot job {id}");
                        }
                    }
                    None => {
                        eprintln!("not found: {id}");
                        std::process::exit(1);
                    }
                }
            }
            "tick" => {
                // Fire every job that is due. Jobs used to be written to
                // schedule.json and nothing ever read it against a clock, so no
                // job could ever fire on its own. `tick` is the primitive a
                // daemon, cron entry, or CI step calls; `--watch` keeps it
                // running in the foreground.
                //
                // This shares the gateway service's scheduler loop
                // (pantheon_gateway::scheduler::SchedulerLoop): same claim
                // ledger, same job store, so the two can never double-fire.
                // Every fire goes through the durable claim ledger first: the
                // claim is an atomic first-wins INSERT, so two ticks racing the
                // same due job - two threads, two processes, or a restart
                // replaying a minute - agree on exactly one winner instead of
                // double-executing. A claim that cannot be persisted fails
                // closed: the run does not start.
                let watch = parts.iter().any(|a| a == "--watch");
                let mut sched = match SchedulerLoop::open(data_dir, 30) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("tick: {e}");
                        eprintln!("tick: refusing to fire without durable claims");
                        std::process::exit(1);
                    }
                };
                // Record every fired run's outcome and self-heal the failed
                // ones: investigate, retry once when fixed, alert always.
                let healer = SelfHealer::new(data_dir);
                sched.set_outcome_sink(healer.outcome_sink());
                install_scheduler_hooks(&mut sched, data_dir);
                let dd = data_dir.to_path_buf();
                let execute: ExecuteFn = Arc::new(move |j: &ScheduledJob| {
                    let task = j.job.resolve_task(&TemplateStore::open(&dd));
                    let report = run_job_now(&task, &j.job, &dd);
                    if let Some(e) = &report.error {
                        eprintln!("{e}");
                    }
                    TaskOutcome {
                        run_id: report.run_id,
                        error: report.error,
                    }
                });
                loop {
                    // Automatic retention: at most one prune pass per day,
                    // so --watch daemons keep the ledger bounded on their own.
                    maybe_run_retention(data_dir);
                    let jobs = match load_schedulable(data_dir) {
                        Ok(j) => j,
                        Err(e) => {
                            eprintln!("schedule: {e}");
                            eprintln!(
                                "fix: delete or repair {}",
                                job_store_path(data_dir).display()
                            );
                            std::process::exit(1);
                        }
                    };
                    let now = now_ms();
                    let (reports, watchers) = sched.tick_once(now, &jobs, execute.clone());
                    let mut any_fired = false;
                    for r in &reports {
                        match &r.outcome {
                            FireOutcome::Fired => {
                                println!("tick {now}: fired {}", r.id);
                                any_fired = true;
                            }
                            FireOutcome::Queued => {
                                println!(
                                    "tick {now}: {} still running, queued (overlap=queue)",
                                    r.id
                                );
                                any_fired = true;
                            }
                            FireOutcome::Skipped(reason) => {
                                println!("tick {now}: {} {reason}", r.id);
                            }
                        }
                    }
                    if !any_fired {
                        println!("tick {now}: nothing due");
                    }
                    record_fires(data_dir, &reports, now);
                    if !watch {
                        // One-shot tick waits for the fired runs' watchers:
                        // the watcher threads run the outcome sink (run
                        // history + self-heal) and the process must not exit
                        // before they finish. Bounded by the jobs' timeouts.
                        for w in watchers {
                            let _ = w.join();
                        }
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(30));
                }
            }
            "prune" => {
                // Manual trigger for the retention pass (the tick loop runs it
                // automatically at most once per day). `--days N` overrides the
                // configured `[retention] keep_days` for this run only.
                let mut days: Option<u32> = None;
                let mut it = parts[1..].iter();
                while let Some(a) = it.next() {
                    if a == "--days" {
                        let v = it.next().unwrap_or_else(|| {
                            eprintln!("usage: pantheon schedule prune [--days N]");
                            std::process::exit(2);
                        });
                        days = Some(v.parse().unwrap_or_else(|_| {
                            eprintln!("prune: bad --days {v:?}");
                            std::process::exit(2);
                        }));
                    } else {
                        eprintln!("unknown flag: {a}");
                        std::process::exit(2);
                    }
                }
                let keep_days = days.unwrap_or_else(|| {
                    crate::config::Config::load_or_report(data_dir)
                        .map(|c| c.retention_days())
                        .unwrap_or(crate::config::DEFAULT_RETENTION_DAYS)
                });
                if keep_days == 0 {
                    println!("retention disabled (keep_days = 0); nothing pruned");
                    return;
                }
                match run_retention(data_dir, keep_days) {
                    Ok(r) => {
                        retention::record_retention_pass(data_dir);
                        println!(
                            "retention: pruned {} events, {} search chunks, {} claims older than {}d (active runs kept)",
                            r.events_pruned,
                            r.search_chunks_pruned,
                            r.claims_pruned,
                            r.keep_days
                        );
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            }
            _ => {
                // Guarded by cmd_schedule's subcommand allow-list; fail loud
                // (not unreachable) so a new subcommand can't silently no-op.
                eprintln!("usage: pantheon schedule list|pause|resume|cancel|run <id>");
                eprintln!("       pantheon schedule tick [--watch]");
                eprintln!("       pantheon schedule prune [--days N]");
                std::process::exit(2);
            }
        }
    }

    /// What one retention pass removed. Returned so the caller can log it
    /// pruning must be visible, never silent.
    #[derive(Debug, Default)]
    pub struct RetentionReport {
        pub keep_days: u32,
        pub events_pruned: usize,
        pub search_chunks_pruned: usize,
        pub claims_pruned: usize,
    }

    /// Run the retention pass now. Thin wrapper over the scheduler crate's
    /// pass ([`retention::run_retention`]): the TUI used to carry its own
    /// copy of this prune, and the two copies shared only a doc promise to
    /// keep them in sync. There is one implementation now; the TUI keeps
    /// its own report type so existing callers are untouched.
    pub fn run_retention(data_dir: &Path, keep_days: u32) -> Result<RetentionReport, String> {
        let r = retention::run_retention(data_dir, keep_days)?;
        Ok(RetentionReport {
            keep_days: r.keep_days,
            events_pruned: r.events_pruned,
            search_chunks_pruned: r.search_chunks_pruned,
            claims_pruned: r.claims_pruned,
        })
    }

    /// The automatic pass, called from the tick loop. Delegates to the
    /// scheduler crate's gated pass, so the TUI tick loop and the gateway
    /// daemon share one 24h gate (`retention.json`) and one prune rule.
    fn maybe_run_retention(data_dir: &Path) {
        retention::maybe_run_retention(data_dir);
    }

    /// What `schedule <task> ...` parsed out of the command line. Named because
    /// a six-element tuple gave no clue what any position meant at the call
    /// site.
    struct CreateArgs {
        task: String,
        every: Option<String>,
        agent: String,
        cron: Option<String>,
        at: Option<String>,
        model: Option<String>,
        provider: Option<String>,
        timeout: Option<String>,
        overlap: Option<String>,
        deliver: Option<String>,
        template: Option<String>,
        template_vars: HashMap<String, String>,
        vars: Vec<(String, String)>,
    }

    fn parse_create_args(args: &[String]) -> CreateArgs {
        let mut task = String::new();
        let mut every = None;
        let mut agent = "nyx".to_string();
        let mut cron = None;
        let mut at = None;
        let mut model = None;
        let mut provider = None;
        let mut timeout = None;
        let mut overlap = None;
        let mut deliver = None;
        let mut template = None;
        let mut vars: Vec<(String, String)> = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--every" | "-e" => {
                    i += 1;
                    if i < args.len() {
                        every = Some(args[i].clone());
                    }
                }
                "--agent" => {
                    i += 1;
                    if i < args.len() {
                        agent = args[i].clone();
                    }
                }
                "--cron" => {
                    i += 1;
                    if i < args.len() {
                        cron = Some(args[i].clone());
                    }
                }
                "--at" => {
                    i += 1;
                    if i < args.len() {
                        at = Some(args[i].clone());
                    }
                }
                "--model" => {
                    i += 1;
                    if i < args.len() {
                        model = Some(args[i].clone());
                    }
                }
                "--provider" => {
                    i += 1;
                    if i < args.len() {
                        provider = Some(args[i].clone());
                    }
                }
                "--timeout" => {
                    i += 1;
                    if i < args.len() {
                        timeout = Some(args[i].clone());
                    }
                }
                "--overlap" => {
                    i += 1;
                    if i < args.len() {
                        overlap = Some(args[i].clone());
                    }
                }
                "--deliver" => {
                    i += 1;
                    if i < args.len() {
                        deliver = Some(args[i].clone());
                    }
                }
                "--template" => {
                    i += 1;
                    if i < args.len() {
                        template = Some(args[i].clone());
                    }
                }
                "--var" => {
                    i += 1;
                    if i < args.len() {
                        let kv = &args[i];
                        match kv.split_once('=') {
                            Some((k, v)) => vars.push((k.to_string(), v.to_string())),
                            None => vars.push((kv.clone(), String::new())),
                        }
                    }
                }
                s if s.starts_with("--") => {
                    // Support --30m style shorthand
                    let dur = s.trim_start_matches("--");
                    every = Some(dur.to_string());
                }
                _ => {
                    if task.is_empty() {
                        task = args[i].clone();
                    }
                }
            }
            i += 1;
        }
        CreateArgs {
            task,
            every,
            agent,
            cron,
            at,
            model,
            provider,
            timeout,
            overlap,
            deliver,
            template,
            template_vars: HashMap::new(),
            vars,
        }
    }

    fn format_kind(kind: &ScheduleKind) -> String {
        match kind {
            ScheduleKind::Interval { every_ms } => format!("every {every_ms}ms"),
            ScheduleKind::Cron { expr } => format!("cron {expr}"),
            ScheduleKind::OneShot { at_ms } => {
                match chrono::DateTime::from_timestamp_millis(*at_ms) {
                    Some(t) => format!("at {}", t.to_rfc3339()),
                    None => format!("at {at_ms}ms"),
                }
            }
        }
    }

    /// Render `last_run` as elapsed time.
    ///
    /// `last_run` is a wall-clock millisecond timestamp, not a duration. It was
    /// printed as if it were elapsed seconds, so a job that fired a moment ago
    /// reported "last: 1790433286s ago".
    fn format_last(last: Option<i64>) -> String {
        let Some(fired_at) = last else {
            return "never".into();
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let secs = ((now - fired_at) / 1000).max(0);
        if secs < 60 {
            format!("{secs}s ago")
        } else if secs < 3600 {
            format!("{}m ago", secs / 60)
        } else if secs < 86_400 {
            format!("{}h ago", secs / 3600)
        } else {
            format!("{}d ago", secs / 86_400)
        }
    }

    /// Fire a job now: a real agent turn against the configured model, with the
    /// job's own model/provider pin applied when it has one.
    ///
    /// This used to open a run, log a progress line, and mark it complete
    /// without ever calling a model. The user saw "ran <task>" and an idle
    /// ledger. A scheduled task that never executes the task is worse than one
    /// that fails loudly.
    ///
    /// Returns `Err` instead of exiting: the tick loop runs jobs on worker
    /// threads, and a failing job must not take the whole daemon down with it.
    /// Callers that want the old hard-fail behavior (`schedule run`) exit on
    /// What one scheduled job's execution produced: the task-level truth.
    /// `error` is `None` when the task ran cleanly; `Some` when the task
    /// itself failed (or never started, in which case `run_id` is empty).
    /// The outcome sink maps `Some` to [`RunOutcome::Failed`] for run
    /// history, which feeds both the nightly repair loop and self-heal.
    pub(crate) struct JobRunReport {
        pub run_id: String,
        pub error: Option<String>,
    }

    /// the error themselves.
    /// Execute one scheduled job: a real agent turn, or the bounded nightly
    /// pass for nightly jobs. Shared by `pantheon schedule tick` and the
    /// gateway service's scheduler loop - one execution core, never duplicated.
    pub(crate) fn run_job_now(task: &str, sched: &Job, data_dir: &Path) -> JobRunReport {
        // Nightly jobs (the new `__pantheon_nightly__` marker and both old
        // `__pantheon_reflect__` / `__pantheon_consolidate__` markers) run the
        // unified nightly pass directly - no agent turn, no chat model. The
        // pass is off by default; a disabled pass refuses the run loudly via
        // the `run_one_pass` master gate, so a stale schedule entry can never
        // silently do nothing - or silently spend model calls.
        if task == NIGHTLY_TASK_MARKER
            || task == CONSOLIDATE_TASK_MARKER
            || task == REFLECT_TASK_MARKER
        {
            return match crate::nightly_cli::run_one_pass(data_dir, false) {
                Ok(out) => {
                    println!("{}", crate::nightly_cli::summarize_pass(&out));
                    if out.pending > 0 {
                        println!(
                            "note: {} proposal(s) await approval - run `pantheon nightly pending`",
                            out.pending
                        );
                    }
                    JobRunReport {
                        run_id: String::new(),
                        error: None,
                    }
                }
                Err(e) => JobRunReport {
                    run_id: String::new(),
                    error: Some(format!("scheduled nightly pass failed: {e}")),
                },
            };
        }
        use crate::config;
        use crate::config::build_scheduled_model_policy;
        use pantheon_runtime::session::Session;

        let file_cfg = config::Config::load_or_report(data_dir);
        // Unpinned jobs run on the `[scheduled]` auxiliary model, not the
        // interactive default: scheduled work is background work and should
        // burn cheap tokens. Explicit pin > template `model` var > aux.
        let model_policy = build_scheduled_model_policy(
            file_cfg.as_ref(),
            sched.provider.clone(),
            sched.model.clone(),
        );
        // Policy preset honored verbatim: a Reader config must stay
        // read-only for scheduled runs, not silently widen to coder.
        let policy = crate::config_schema::policy_for_config(&file_cfg);
        let secrets = config::chat_secrets(file_cfg.as_ref());
        // Honor the scheduled job's agent pin. An empty or default agent
        // leaves the session on the configured default profile.
        if !sched.agent.is_empty() && sched.agent != "default" {
            if let Some(ref cfg) = file_cfg {
                if let Err(e) = cfg.resolve_profile(Some(&sched.agent)) {
                    return JobRunReport {
                        run_id: String::new(),
                        error: Some(format!(
                            "agent '{}' not found: {}. fix: declare [agents.{}] in config.toml",
                            sched.agent, e, sched.agent
                        )),
                    };
                }
            }
            pantheon_api::config::set_profile_override(Some(sched.agent.clone()));
        }
        let session = match Session::new(data_dir.to_path_buf(), policy, model_policy, secrets) {
            Ok(s) => s,
            Err(e) => {
                return JobRunReport {
                    run_id: String::new(),
                    error: Some(format!("open session: {e}")),
                };
            }
        };
        config::apply_tool_enablement(&session, file_cfg.as_ref());
        // Scheduled runs are sessions too: honor `[budget]` exactly like the
        // interactive, /agui, and gateway paths (see `apply_budget_tiers`).
        config::apply_budget_tiers(&session, file_cfg.as_ref());
        config::apply_delegation_section(&session, file_cfg.as_ref());

        let run_id = pantheon_runtime::new_run_id();
        match session.chat(&run_id, task) {
            Ok(outcome) => {
                println!("ran {task} - run {run_id}");
                deliver_job_result(task, sched, data_dir, &run_id, &outcome);
                JobRunReport {
                    run_id,
                    error: None,
                }
            }
            Err(e) => {
                // A parked run is a real outcome, not a failure: it needs a
                // grant before it can finish.
                if e.code == "RUN_PARKED" {
                    // The error carries the copy-pasteable grant command with the
                    // scope already inlined, so a placeholder here was worse than
                    // useless: it looked actionable and was not.
                    println!("parked {task}");
                    println!("{e}");
                    JobRunReport {
                        run_id,
                        error: None,
                    }
                } else {
                    JobRunReport {
                        run_id,
                        error: Some(format!("scheduled task failed: {e}")),
                    }
                }
            }
        }
    }

    /// Route a completed job's summary to its delivery target.
    ///
    /// The summary is the run's final assistant message, redacted and
    /// truncated. Delivery failure never fails the job: it is a log line, not
    /// an error - the run already completed and sits in the ledger.
    fn deliver_job_result(
        task: &str,
        sched: &Job,
        data_dir: &Path,
        run_id: &str,
        outcome: &pantheon_agent::LoopOutcome,
    ) {
        let raw = sched.deliver.as_deref();
        // The home session is the default delivery target: no explicit target
        // and `--deliver mobile` both land in the pinned session the user
        // actually opens. `Deliver::parse` itself is untouched.
        if schedule_delivery::routes_via_home_session(raw) {
            deliver_job_result_home(task, sched, data_dir, run_id, outcome);
            return;
        }
        let target = match raw {
            // Validated at create; an unknown value here fails open to log.
            Some(s) => {
                schedule_delivery::Deliver::parse(s).unwrap_or(schedule_delivery::Deliver::Log)
            }
            None => return,
        };
        if target == schedule_delivery::Deliver::Log {
            return;
        }
        let summary = match outcome {
            pantheon_agent::LoopOutcome::Answered { text, .. } => Some(text.clone()),
            _ => last_assistant_text(data_dir, run_id),
        };
        let Some(text) = summary.filter(|t| !t.trim().is_empty()) else {
            return;
        };
        let body = schedule_delivery::build_summary(&text);
        if let Some(err) = schedule_delivery::deliver_best_effort(
            &schedule_delivery::RestChannelSender::with_data_dir(data_dir),
            &target,
            &short_task_label(task, &sched.id),
            &body,
        ) {
            eprintln!("delivery failed (job result kept): {err}");
        }
    }

    /// Route a completed job's summary into the home session: the default
    /// delivery target (and the `--deliver mobile` target). The summary is the
    /// run's final assistant message, redacted and truncated, exactly like the
    /// channel path above. Delivery failure never fails the job.
    fn deliver_job_result_home(
        task: &str,
        sched: &Job,
        data_dir: &Path,
        run_id: &str,
        outcome: &pantheon_agent::LoopOutcome,
    ) {
        let summary = match outcome {
            pantheon_agent::LoopOutcome::Answered { text, .. } => Some(text.clone()),
            _ => last_assistant_text(data_dir, run_id),
        };
        let Some(text) = summary.filter(|t| !t.trim().is_empty()) else {
            return;
        };
        let body = schedule_delivery::build_summary(&text);
        if let Some(err) = schedule_delivery::deliver_to_home_session(
            data_dir,
            &short_task_label(task, &sched.id),
            &body,
        ) {
            eprintln!("delivery failed (job result kept): {err}");
        }
    }

    /// Readable notification header: the task's first line, capped; the job id
    /// when the task has no usable first line.
    fn short_task_label(task: &str, job_id: &str) -> String {
        let first: String = task
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(60)
            .collect();
        if first.is_empty() {
            job_id.to_string()
        } else {
            first
        }
    }

    /// Fallback summary source: the last non-empty assistant message in the
    /// ledger, for runs that did not end in a final answer.
    fn last_assistant_text(data_dir: &Path, run_id: &str) -> Option<String> {
        let ledger = pantheon_storage::Ledger::open(&data_dir.join("ledger.db")).ok()?;
        let entries = ledger.replay(run_id).ok()?;
        let messages = pantheon_runtime::session::rebuild_messages(entries);
        messages
            .iter()
            .rev()
            .find(|m| {
                m.role == pantheon_api::message::Role::Assistant && !m.content.trim().is_empty()
            })
            .map(|m| m.content.clone())
    }

    /// Load jobs as the scheduler loop sees them. This is the core's shared
    /// shape already, so it is a straight pass-through. Corruption is an error
    /// for the caller to handle: the CLI tick exits, the service loop logs and
    /// keeps going (a bad schedule.json must not take the gateway down).
    pub(crate) fn load_schedulable(data_dir: &Path) -> Result<Vec<ScheduledJob>, String> {
        load_jobs(data_dir)
    }

    /// Persist fire times for jobs that fired, so an interval job does not
    /// come due again on the next tick. The durable claim already won is what
    /// makes this crash-safe; last_run just drives the due check.
    ///
    /// Only [`FireOutcome::Fired`] stamps `last_run`: a fire means the claim
    /// was won and the run started. [`FireOutcome::Queued`] is a deferred
    /// duplicate of an already-fired occurrence - stamping `last_run` there
    /// would reset the interval clock before anything ran.
    ///
    /// One-shot rows are NOT removed here. They are removed by the one-shot
    /// completion observer (installed via
    /// [`SchedulerLoop::set_oneshot_sink`]) only when the run actually
    /// completes: deleting on fire would lose panicked/timed-out runs
    /// silently. A one-shot with `last_run` set is inert (`Job::due` is
    /// false), so keeping the row can never cause a refire.
    ///
    /// The load/mutate/save runs under the store's exclusive lock
    /// ([`update_jobs`]) so a concurrent dashboard/CLI edit can't be
    /// clobbered by this pass's stale read.
    pub(crate) fn record_fires(data_dir: &Path, reports: &[TickReport], now_ms: i64) {
        let fired: Vec<&str> = reports
            .iter()
            .filter(|r| matches!(r.outcome, FireOutcome::Fired))
            .map(|r| r.id.as_str())
            .collect();
        if fired.is_empty() {
            return;
        }
        if let Err(e) = update_jobs(data_dir, |jobs| {
            for j in jobs.iter_mut() {
                if fired.contains(&j.job.id.as_str()) {
                    j.last_run = Some(now_ms);
                }
            }
            Ok(())
        }) {
            eprintln!("warning: could not record last_run: {e}");
        }
    }

    /// Seconds between scheduler passes. Default 60; override with
    /// `PANTHEON_SCHEDULER_TICK_SECS`.
    pub(crate) fn scheduler_tick_secs() -> u64 {
        std::env::var("PANTHEON_SCHEDULER_TICK_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(60)
    }

    /// Install the tick driver's safety hooks on a freshly opened loop:
    /// - a paused-state predicate, so a queued drain re-checks paused fresh
    ///   from the store instead of trusting the snapshot taken at fire time;
    /// - a one-shot completion observer, so one-shot rows are removed only
    ///   when the run actually completed - never on panic/timeout.
    fn install_scheduler_hooks(sched: &mut SchedulerLoop, data_dir: &Path) {
        let dd = data_dir.to_path_buf();
        sched.set_paused_check(Arc::new(move |id: &str| {
            match load_jobs(&dd) {
                Ok(jobs) => jobs.iter().any(|j| j.job.id == id && j.job.paused),
                // Fail closed: if the store can't be read, don't fire blind.
                Err(_) => true,
            }
        }));
        let dd = data_dir.to_path_buf();
        sched.set_oneshot_sink(Arc::new(
            move |id: &str, outcome: RunOutcome, _task: Option<TaskOutcome>| {
                if outcome != RunOutcome::Completed {
                    eprintln!(
                    "scheduler: one-shot job {id} ended as {outcome:?}; keeping the row for inspection"
                );
                    return;
                }
                let id_owned = id.to_string();
                if let Err(e) = update_jobs(&dd, |jobs| {
                    jobs.retain(|j| {
                        !(j.job.id == id_owned && matches!(j.job.kind, ScheduleKind::OneShot { .. }))
                    });
                    Ok(())
                }) {
                    eprintln!("scheduler: could not remove completed one-shot job {id}: {e}");
                }
            },
        ));
    }

    /// The gateway service's scheduler loop: ticks due jobs forever on the
    /// same claim ledger and job store `pantheon schedule tick` uses.
    /// Never returns; a ledger that cannot open is logged and the thread ends
    /// (firing without durable claims is refused).
    pub(crate) fn run_scheduler_loop(data_dir: &Path) {
        let tick_secs = scheduler_tick_secs();
        let mut sched = match SchedulerLoop::open(data_dir, tick_secs) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("scheduler: {e}; scheduled jobs will not fire");
                return;
            }
        };
        // Record every fired run's outcome and self-heal the failed ones:
        // investigate, retry once when fixed, alert always.
        let healer = SelfHealer::new(data_dir);
        sched.set_outcome_sink(healer.outcome_sink());
        install_scheduler_hooks(&mut sched, data_dir);
        eprintln!("scheduler: ticking every {tick_secs}s");
        let dd = data_dir.to_path_buf();
        let execute: ExecuteFn = Arc::new(move |j: &ScheduledJob| {
            // Re-render the template at fire time: edits to the template
            // apply to already-scheduled jobs.
            let task = j.job.resolve_task(&TemplateStore::open(&dd));
            let report = run_job_now(&task, &j.job, &dd);
            if let Some(e) = &report.error {
                eprintln!("scheduler: {e}");
            }
            TaskOutcome {
                run_id: report.run_id,
                error: report.error,
            }
        });
        let dd = data_dir.to_path_buf();
        let dd2 = data_dir.to_path_buf();
        sched.run_forever(
            &move || {
                // Automatic retention, same as the CLI tick loop: at most one
                // prune pass per day, so the long-running service keeps the
                // ledger bounded on its own.
                maybe_run_retention(&dd);
                match load_schedulable(&dd) {
                    Ok(jobs) => jobs,
                    Err(e) => {
                        eprintln!("scheduler: {e}");
                        Vec::new()
                    }
                }
            },
            execute,
            &move |reports: &[TickReport], now: i64| {
                for r in reports {
                    match &r.outcome {
                        FireOutcome::Fired => println!("scheduler {now}: fired {}", r.id),
                        FireOutcome::Queued => {
                            println!("scheduler {now}: {} still running, queued", r.id)
                        }
                        FireOutcome::Skipped(_) => {}
                    }
                }
                record_fires(&dd2, reports, now);
            },
        );
    }
}
