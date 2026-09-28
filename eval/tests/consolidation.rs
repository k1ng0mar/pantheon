//! Consolidation behavioral evals: stage → weigh → promote over a real
//! ledger and memory store.
//!
//! Covers: triple repetition promotes with ledger provenance, one-off
//! stays staged, rerun idempotency, dry-run writes nothing, disabled
//! default makes zero LLM calls, and every LLM call routes through the
//! `AuxiliaryKind::Consolidation` slot — never the chat model.

use pantheon_api::capability::Policy;
use pantheon_api::events::Event;
use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel, DefaultModel, ModelPolicy};
use pantheon_consolidate::weigh::LlmDistill;
use pantheon_consolidate::{now_ms, ConsolidationConfig, PassInput};
use pantheon_memory::LayerKind;
use pantheon_storage::Ledger;
use std::sync::Mutex;

struct Harness {
    dir: tempfile::TempDir,
    ledger: Ledger,
    backend: std::sync::Arc<dyn pantheon_memory::MemoryBackend>,
    policy: Policy,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
    let backend = pantheon_memory::open_selected(dir.path()).unwrap();
    Harness {
        dir,
        ledger,
        backend,
        policy: Policy::coder_with_memory(),
    }
}

fn steer(ledger: &Ledger, run_id: &str, text: &str) {
    // RunStarted registers the run in the `runs` table, which is what
    // the stage phase scans.
    ledger
        .append(&Event::RunStarted {
            run_id: run_id.into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnStarted {
            run_id: run_id.into(),
            turn_id: "turn_0".into(),
        })
        .unwrap();
    ledger
        .append(&Event::SteeringProvided {
            run_id: run_id.into(),
            text: text.into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnCompleted {
            run_id: run_id.into(),
            turn_id: "turn_0".into(),
            outcome: "answered".into(),
        })
        .unwrap();
}

fn policy_with_slot() -> ModelPolicy {
    ModelPolicy {
        default: DefaultModel {
            provider: "chat-provider".into(),
            model: "chat-model".into(),
        },
        fallbacks: Default::default(),
        auxiliaries: vec![AuxiliaryModel {
            kind: AuxiliaryKind::Consolidation,
            provider: "cheap-provider".into(),
            model: "cheap-model".into(),
        }],
        reasoning: Default::default(),
        reasoning_budget: None,
    }
}

fn policy_without_slot() -> ModelPolicy {
    let mut p = policy_with_slot();
    p.auxiliaries.clear();
    p
}

/// Spy distiller: records the exact auxiliary model it was handed.
#[derive(Default)]
struct SpyDistill {
    calls: Mutex<Vec<AuxiliaryModel>>,
}

impl LlmDistill for SpyDistill {
    fn distill(&self, model: &AuxiliaryModel, texts: &[String]) -> Result<Vec<String>, String> {
        self.calls.lock().unwrap().push(model.clone());
        Ok(texts.to_vec())
    }
}

fn run(
    h: &Harness,
    config: &ConsolidationConfig,
    dry_run: bool,
) -> pantheon_consolidate::ConsolidationReport {
    pantheon_consolidate::run_pass(PassInput {
        ledger: &h.ledger,
        backend: h.backend.as_ref(),
        policy: &h.policy,
        config,
        data_dir: h.dir.path(),
        dry_run,
        model_policy: None,
        llm: None,
        now_ms: now_ms(),
    })
    .unwrap()
}

fn promoted_records(h: &Harness) -> Vec<pantheon_memory::MemoryRecord> {
    h.backend
        .recall(&h.policy, &["nyx"], &[LayerKind::Agent], "tabs", 10)
        .unwrap()
        .into_iter()
        .map(|r| r.record)
        .filter(|r| r.key.starts_with("consolidated:"))
        .collect()
}

#[test]
fn triple_repetition_promotes_with_ledger_provenance() {
    let h = harness();
    for i in 1..=3 {
        steer(
            &h.ledger,
            &format!("run_{i}"),
            "always use tabs for indentation",
        );
    }
    let config = ConsolidationConfig::default();

    let report = run(&h, &config, false);

    assert_eq!(report.staged, 1, "same remark across runs = one candidate");
    assert_eq!(report.promoted, 1, "three sessions clear the bar");
    assert!(!report.llm_used);

    let records = promoted_records(&h);
    assert_eq!(records.len(), 1, "exactly one durable fact");
    let rec = &records[0];
    assert!(rec.value.contains("tabs"), "fact text preserved");
    assert_eq!(rec.provenance.source, "consolidation");
    for i in 1..=3 {
        assert!(
            rec.provenance.origin.contains(&format!("run_{i}")),
            "promotion traces to ledger run_{i}: {}",
            rec.provenance.origin
        );
    }
}

#[test]
fn one_off_remark_remains_staged() {
    let h = harness();
    steer(&h.ledger, "run_1", "always use tabs for indentation");

    let report = run(&h, &ConsolidationConfig::default(), false);

    assert_eq!(report.staged, 1);
    assert_eq!(report.promoted, 0, "one session never promotes");
    assert!(promoted_records(&h).is_empty());
}

#[test]
fn rerun_is_idempotent() {
    let h = harness();
    for i in 1..=3 {
        steer(
            &h.ledger,
            &format!("run_{i}"),
            "always use tabs for indentation",
        );
    }
    let config = ConsolidationConfig::default();

    let first = run(&h, &config, false);
    assert_eq!(first.promoted, 1);

    // Second pass: the incremental window advanced past the staged
    // events, so there is nothing new to promote — and no duplicate.
    let second = run(&h, &config, false);
    assert_eq!(second.promoted, 0);
    assert_eq!(promoted_records(&h).len(), 1, "no duplicate fact");

    // Third pass with the window rewound: the same candidate is staged
    // again, but the deterministic key hits the existing record and is
    // skipped — promotion stays append-only and idempotent.
    std::fs::remove_file(h.dir.path().join("consolidation").join("state.json")).unwrap();
    let third = run(&h, &config, false);
    assert_eq!(third.staged, 1);
    assert_eq!(third.promoted, 0);
    assert_eq!(third.skipped_duplicate, 1);
    assert_eq!(promoted_records(&h).len(), 1, "still exactly one fact");
}

#[test]
fn dry_run_writes_nothing() {
    let h = harness();
    for i in 1..=3 {
        steer(
            &h.ledger,
            &format!("run_{i}"),
            "always use tabs for indentation",
        );
    }

    let report = run(&h, &ConsolidationConfig::default(), true);

    assert!(report.dry_run);
    assert_eq!(report.promoted, 0);
    assert!(promoted_records(&h).is_empty(), "dry-run writes no memory");
    assert!(
        !h.dir
            .path()
            .join("consolidation")
            .join("state.json")
            .exists(),
        "dry-run persists no state"
    );
    // ...but the operator can still read what would happen.
    let day_dir = std::fs::read_dir(h.dir.path().join("consolidation").join("weigh")).unwrap();
    assert_eq!(day_dir.count(), 1, "dry-run still writes the weigh report");
}

#[test]
fn disabled_default_makes_zero_llm_calls() {
    let h = harness();
    for i in 1..=3 {
        steer(
            &h.ledger,
            &format!("run_{i}"),
            "always use tabs for indentation",
        );
    }
    let config = ConsolidationConfig::default();
    assert!(!config.enabled);
    let spy = SpyDistill::default();
    let model_policy = policy_with_slot();

    let report = pantheon_consolidate::run_pass(PassInput {
        ledger: &h.ledger,
        backend: h.backend.as_ref(),
        policy: &h.policy,
        config: &config,
        data_dir: h.dir.path(),
        dry_run: false,
        model_policy: Some(&model_policy),
        llm: Some(&spy),
        now_ms: now_ms(),
    })
    .unwrap();

    assert!(
        spy.calls.lock().unwrap().is_empty(),
        "disabled = zero LLM calls"
    );
    assert!(!report.llm_used);
    assert_eq!(report.promoted, 1, "deterministic pipeline unaffected");
}

#[test]
fn enabled_without_slot_stays_deterministic() {
    let h = harness();
    for i in 1..=3 {
        steer(
            &h.ledger,
            &format!("run_{i}"),
            "always use tabs for indentation",
        );
    }
    let config = ConsolidationConfig { enabled: true, ..Default::default() };
    let spy = SpyDistill::default();
    let model_policy = policy_without_slot();

    let report = pantheon_consolidate::run_pass(PassInput {
        ledger: &h.ledger,
        backend: h.backend.as_ref(),
        policy: &h.policy,
        config: &config,
        data_dir: h.dir.path(),
        dry_run: false,
        model_policy: Some(&model_policy),
        llm: Some(&spy),
        now_ms: now_ms(),
    })
    .unwrap();

    assert!(
        spy.calls.lock().unwrap().is_empty(),
        "no resolvable slot = no LLM call"
    );
    assert!(!report.llm_used);
    assert_eq!(report.promoted, 1, "falls back to deterministic distill");
}

#[test]
fn enabled_with_slot_routes_through_auxiliary_never_chat() {
    let h = harness();
    for i in 1..=3 {
        steer(
            &h.ledger,
            &format!("run_{i}"),
            "always use tabs for indentation",
        );
    }
    let config = ConsolidationConfig { enabled: true, ..Default::default() };
    let spy = SpyDistill::default();
    let model_policy = policy_with_slot();

    let report = pantheon_consolidate::run_pass(PassInput {
        ledger: &h.ledger,
        backend: h.backend.as_ref(),
        policy: &h.policy,
        config: &config,
        data_dir: h.dir.path(),
        dry_run: false,
        model_policy: Some(&model_policy),
        llm: Some(&spy),
        now_ms: now_ms(),
    })
    .unwrap();

    let calls = spy.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "one distill call for the one group");
    let used = &calls[0];
    assert_eq!(used.kind, AuxiliaryKind::Consolidation);
    assert_eq!(used.model, "cheap-model", "aux model, not the chat model");
    assert_ne!(used.model, "chat-model");
    assert!(report.llm_used);
    assert_eq!(report.promoted, 1);
}

// ---------------------------------------------------------------------------
// CLI / scheduler / production-distiller seams
//
// These exercise the `pantheon consolidate` entrypoints in
// `pantheon-tui` and the production `DistillClient` in
// `pantheon-providers` — the layers above the `run_pass` core covered
// above.
// ---------------------------------------------------------------------------

/// Moved out of `pantheon-consolidate/src/state.rs`: loading state from
/// a directory with no state file yields the default. Filesystem
/// behavior belongs in eval, not beside the source.
#[test]
fn missing_state_is_default() {
    let dir = tempfile::tempdir().unwrap();
    let st = pantheon_consolidate::state::load(dir.path()).unwrap();
    assert!(st.last_run_ms.is_none());
    assert!(st.promoted_keys.is_empty());
}

fn steer_triple(h: &Harness) {
    for i in 1..=3 {
        steer(
            &h.ledger,
            &format!("run_{i}"),
            "always use tabs for indentation",
        );
    }
}

/// `pantheon consolidate --dry-run` (the `run_one_pass` seam the CLI and
/// the TUI worker share): stages and weighs, writes reports, but
/// promotes nothing and persists no state.
#[test]
fn cli_dry_run_pass_writes_no_memory() {
    let h = harness();
    steer_triple(&h);

    let report = pantheon_tui::consolidate_cli::run_one_pass(h.dir.path(), true).unwrap();

    assert!(report.dry_run);
    assert_eq!(report.promoted, 0);
    assert_eq!(
        report.would_promote,
        1,
        "dry-run reports what would promote: {}",
        report.summary_line()
    );
    assert!(
        report.summary_line().contains("would promote 1"),
        "dry-run summary names the count: {}",
        report.summary_line()
    );
    assert!(promoted_records(&h).is_empty(), "dry-run writes no memory");
    assert!(
        !h.dir
            .path()
            .join("consolidation")
            .join("state.json")
            .exists(),
        "dry-run persists no state"
    );
}

/// `pantheon consolidate status`: the enabled flag plus the last-pass
/// summary, with no pass required.
#[test]
fn cli_status_reports_toggle_and_last_pass() {
    let h = harness();

    let line = pantheon_tui::consolidate_cli::status_line(h.dir.path());
    assert!(line.contains("enabled: no"), "default is disabled: {line}");
    assert!(line.contains("never"), "no pass ran yet: {line}");

    steer_triple(&h);
    let report = pantheon_tui::consolidate_cli::run_one_pass(h.dir.path(), false).unwrap();
    assert_eq!(report.promoted, 1);

    let line = pantheon_tui::consolidate_cli::status_line(h.dir.path());
    assert!(
        line.contains("facts promoted (all time): 1"),
        "status counts the promotion: {line}"
    );
    assert!(
        line.contains("last pass: staged"),
        "status shows explicit last-pass counts: {line}"
    );
    assert!(
        line.contains("promoted 1"),
        "status shows the promoted count: {line}"
    );
}

fn write_consolidation_config(dir: &std::path::Path, enabled: bool) {
    std::fs::write(
        dir.join("config.toml"),
        format!("[consolidation]\nenabled = {enabled}\n"),
    )
    .unwrap();
}

/// `pantheon schedule consolidate`: refused when `[consolidation]
/// enabled = false`, registered with the `[consolidation] cron` default
/// (`0 3 * * *`) when enabled.
#[test]
fn schedule_consolidate_only_when_enabled_with_default_cron() {
    use pantheon_tui::schedule::{build_consolidate_job, CONSOLIDATE_TASK_MARKER};

    let dir = tempfile::tempdir().unwrap();

    write_consolidation_config(dir.path(), false);
    let err = build_consolidate_job(dir.path(), &[]).unwrap_err();
    assert!(err.contains("disabled"), "refused when disabled: {err}");

    write_consolidation_config(dir.path(), true);
    let job = build_consolidate_job(dir.path(), &[]).unwrap();
    assert_eq!(job.task, CONSOLIDATE_TASK_MARKER);
    match &job.kind {
        pantheon_scheduler::ScheduleKind::Cron { expr } => {
            assert_eq!(expr, "0 3 * * *", "default [consolidation] cron")
        }
        other => panic!("expected the default cron job, got {other:?}"),
    }

    // An explicit --cron wins over the default.
    let job = build_consolidate_job(dir.path(), &["--cron".to_string(), "0 4 * * *".to_string()])
        .unwrap();
    match &job.kind {
        pantheon_scheduler::ScheduleKind::Cron { expr } => assert_eq!(expr, "0 4 * * *"),
        other => panic!("expected the explicit cron job, got {other:?}"),
    }
}

/// Fake transport: records the request, replays a canned OpenAI-style
/// completion. `openai` is catalogued, so the wire resolves and the
/// canned body parses. Uses a channel (not a mutex) to record calls.
struct FakeTransport {
    tx: std::sync::mpsc::Sender<pantheon_providers::http::WireRequest>,
    body: String,
}

impl pantheon_providers::http::ChatTransport for FakeTransport {
    fn post(
        &self,
        req: &pantheon_providers::http::WireRequest,
    ) -> Result<String, pantheon_api::error::PantheonError> {
        self.tx.send(req.clone()).unwrap();
        Ok(self.body.clone())
    }

    fn post_stream(
        &self,
        _req: &pantheon_providers::http::WireRequest,
        _on_payload: &mut dyn FnMut(&str) -> Result<(), pantheon_api::error::PantheonError>,
    ) -> Result<(), pantheon_api::error::PantheonError> {
        Ok(())
    }
}

/// The production distiller calls the resolved Consolidation auxiliary
/// model through the provider stack — and refuses a mismatched model
/// instead of silently falling back to the chat model.
#[test]
fn production_distiller_calls_aux_model_not_chat() {
    use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel, DefaultModel};
    use pantheon_consolidate::weigh::LlmDistill;
    use pantheon_providers::DistillClient;

    let (tx, rx) = std::sync::mpsc::channel();
    let body = r#"{"choices":[{"message":{"role":"assistant","content":"merged fact one\nmerged fact two"},"finish_reason":"stop"}]}"#;
    let client = DistillClient::new(
        DefaultModel {
            provider: "openai".into(),
            model: "aux-model".into(),
        },
        None,
    )
    .with_transport(Box::new(FakeTransport {
        tx,
        body: body.to_string(),
    }));

    let aux = AuxiliaryModel {
        kind: AuxiliaryKind::Consolidation,
        provider: "openai".into(),
        model: "aux-model".into(),
    };
    let facts = client
        .distill(
            &aux,
            &["note one".to_string(), "note one again".to_string()],
        )
        .unwrap();
    assert_eq!(
        facts,
        vec!["merged fact one".to_string(), "merged fact two".to_string()]
    );

    let req = rx.try_recv().expect("exactly one provider call");
    assert!(
        req.body.contains("aux-model"),
        "calls the aux model: {}",
        req.body
    );
    assert!(rx.try_recv().is_err(), "no extra provider calls");

    // A mismatched model is a wiring bug: refuse, don't fall back.
    let chat = AuxiliaryModel {
        kind: AuxiliaryKind::Consolidation,
        provider: "openai".into(),
        model: "chat-model".into(),
    };
    let err = client.distill(&chat, &["x".to_string()]).unwrap_err();
    assert!(err.contains("mismatch"), "refuses the wrong model: {err}");
    assert!(rx.try_recv().is_err(), "no provider call on mismatch");
}
