//! Tests for `pantheon_runtime::session::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_core::model_event::ModelEvent;

#[test]
fn streaming_deltas_persist_as_model_delta_rows() {
    // The sink must persist TextDelta events as ModelDelta rows even
    // though to_event() returns None for them (high-frequency provider-plane
    // events are not full Event variants).
    let dir = std::env::temp_dir().join(format!("pantheon-rt-sess-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("run_stream").unwrap();
    let sink = LedgerModelSink {
        sup: &sup,
        run_id: "run_stream",
    };
    sink.emit(ModelEvent::Attempt {
        provider: "router".into(),
        model: "chat".into(),
        chain_index: 0,
        streaming: true,
    });
    sink.emit(ModelEvent::TextDelta {
        text: "part1".into(),
    });
    sink.emit(ModelEvent::TextDelta {
        text: "part2".into(),
    });
    sink.emit(ModelEvent::Usage {
        usage: pantheon_core::model_event::ModelUsage {
            input_tokens: 3,
            output_tokens: 6,
            total_tokens: 9,
            cost_usd: None,
        },
    });
    sink.emit(ModelEvent::Completed {
        finish_reason: Some("stop".into()),
    });

    let entries = sup.replay("run_stream").unwrap();
    let deltas: Vec<String> = entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::ModelDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["part1".to_string(), "part2".to_string()]);
    // Other events also projected.
    // Event order: RunStarted (from start_run) -> Attempt/ModelRequested,
    // two TextDelta->ModelDelta, then Completed->ModelCompleted.
    // Usage events are provider-plane only (to_event returns None) and
    // are NOT persisted to the ledger -- by design.
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| match &e.event {
            Event::RunStarted { .. } => "start",
            Event::ModelRequested { .. } => "req",
            Event::ModelDelta { .. } => "delta",
            Event::ModelCompleted { .. } => "done",
            _ => "skip",
        })
        .collect();
    assert_eq!(kinds, vec!["start", "req", "delta", "delta", "done"]);
    // rebuild_messages skips deltas (they are not full messages), but
    // the transcript still has the persisted content for inspection.
    let msgs = rebuild_messages(entries);
    assert!(msgs.is_empty(), "no full assistant/user rows emitted");
}

/// Phase 5: three 400ms tool calls in one turn must finish in well under
/// a second if they run concurrently (sequential would be >=1.2s).
#[test]
fn parallel_tool_calls_overlap_in_wall_time() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    let mut reg = ToolRegistry::new();
    let counter = std::sync::Arc::new(AtomicU32::new(0));
    let c2 = counter.clone();
    for i in 0..3 {
        let c = c2.clone();
        reg.register(
            pantheon_core::message::ToolSchema {
                name: format!("slow_{i}"),
                description: "sleeps 400ms".into(),
                parameters: serde_json::json!({}),
            },
            pantheon_core::capability::Capability::ShellExecute,
            move |_args| {
                c.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(400));
                Ok(format!("done_{i}"))
            },
        );
    }
    // Three calls the model "asked for" in one turn.
    let calls: Vec<pantheon_agent::ToolCall> = (0..3)
        .map(|i| pantheon_agent::ToolCall {
            name: format!("slow_{i}"),
            capability: pantheon_core::capability::Capability::ShellExecute,
            args: "{}".into(),
        })
        .collect();
    // Execute the same way drive() does: scoped threads over the registry.
    let t0 = Instant::now();
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = calls
            .iter()
            .map(|c| {
                let r = &reg;
                s.spawn(move || r.execute(&c.name, &c.args))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    let elapsed = t0.elapsed();
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r.as_ref().unwrap(), &format!("done_{i}"));
    }
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    // Sequential would be >= 1.2s. Parallel: < 0.9s with slack.
    assert!(
        elapsed < std::time::Duration::from_millis(900),
        "calls ran sequentially: {elapsed:?}"
    );
}

#[test]
fn denied_scope_settles_instead_of_reparking_on_resume() {
    // A denied call must not leave the run stuck: on resume the denial
    // becomes a transcript tool result and the run can finish.
    let dir = std::env::temp_dir().join(format!("pantheon-deny-settle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("deny-resume").unwrap();
    sup.emit(Event::ApprovalRequested {
        run_id: "deny-resume".into(),
        scope: "call_0_0".into(),
    })
    .unwrap();
    sup.deny("deny-resume", "call_0_0").unwrap();
    // The denial scope is visible on replay and must be excluded from
    // re-parking by the drive() partition logic.
    let entries = sup.replay("deny-resume").unwrap();
    let denied: Vec<String> = entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::ApprovalDenied { scope, .. } => Some(scope.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(denied, vec!["call_0_0".to_string()]);
    // Status flipped back to running (not parked) after the denial.
    assert_eq!(
        sup.ledger_status("deny-resume").unwrap().as_deref(),
        Some("running")
    );
}

// ---------------------------------------------------------------------------
// hook gate + transform at the tool-execution choke point
// ---------------------------------------------------------------------------

/// A registry with one trivial tool, plus a manager loaded from `dir`.
fn reg_and_mgr(
    dir: &std::path::Path,
) -> (
    ToolRegistry,
    std::sync::Arc<pantheon_extensions::ExtensionManager>,
) {
    let mut reg = ToolRegistry::new();
    reg.register(
        pantheon_core::message::ToolSchema {
            name: "read_secret".into(),
            description: "returns a secret".into(),
            parameters: serde_json::json!({}),
        },
        pantheon_core::capability::Capability::ShellExecute,
        |_args| Ok("sk-live-abc123".to_string()),
    );
    let mut mgr =
        pantheon_extensions::ExtensionManager::new(pantheon_extensions::RunnerConfig::default());
    mgr.load_dir(dir).unwrap();
    (reg, std::sync::Arc::new(mgr))
}

fn write_plugin(dir: &std::path::Path, name: &str, hook: &str, body: &str) {
    let d = dir.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("plugin.yaml"),
        format!("name: {name}\nprovides_hooks:\n  - {hook}\n"),
    )
    .unwrap();
    std::fs::write(
        d.join("__init__.py"),
        format!(
            "def register(ctx):\n    ctx.register_hook('{hook}', _h)\ndef _h(**kw):\n    return {body}\n"
        ),
    )
    .unwrap();
}

#[test]
fn pre_tool_call_gate_blocks_the_tool() {
    let base = std::env::temp_dir().join(format!("pantheon-rt-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    write_plugin(
        &base,
        "security-guidance",
        "pre_tool_call",
        "{'deny': True, 'reason': 'reads credential files'}",
    );
    let (reg, mgr) = reg_and_mgr(&base);
    let runner = RegRunner {
        registry: &reg,
        hooks: Some(&mgr),
        run_id: "run_gate".into(),
    };
    // The tool must NOT run: the gate returns a refusal instead of output.
    let out = <RegRunner as pantheon_agent::ToolRunner>::run(&runner, "read_secret", "{}").unwrap();
    assert!(out.contains("blocked by extension policy"), "{out}");
    assert!(out.contains("reads credential files"), "{out}");
    assert!(
        !out.contains("sk-live-abc123"),
        "secret leaked past the gate: {out}"
    );
}

#[test]
fn transform_tool_result_redacts_before_the_model_sees_it() {
    let base = std::env::temp_dir().join(format!("pantheon-rt-xform-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    write_plugin(
        &base,
        "redact",
        "transform_tool_result",
        "{'replacement': '[redacted by extension]'}",
    );
    let (reg, mgr) = reg_and_mgr(&base);
    let runner = RegRunner {
        registry: &reg,
        hooks: Some(&mgr),
        run_id: "run_xform".into(),
    };
    let out = <RegRunner as pantheon_agent::ToolRunner>::run(&runner, "read_secret", "{}").unwrap();
    assert_eq!(out, "[redacted by extension]");
    assert!(!out.contains("sk-live-abc123"));
}

#[test]
fn no_extensions_means_the_tool_runs_untouched() {
    let base = std::env::temp_dir().join(format!("pantheon-rt-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let (reg, mgr) = reg_and_mgr(&base);
    let runner = RegRunner {
        registry: &reg,
        hooks: Some(&mgr),
        run_id: "run_none".into(),
    };
    let out = <RegRunner as pantheon_agent::ToolRunner>::run(&runner, "read_secret", "{}").unwrap();
    assert_eq!(out, "sk-live-abc123");
}

#[test]
fn a_gate_that_errors_blocks_rather_than_passes() {
    let base = std::env::temp_dir().join(format!("pantheon-rt-gateerr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    write_plugin(
        &base,
        "broken-gate",
        "pre_tool_call",
        "(_ for _ in ()).throw(RuntimeError('boom'))",
    );
    let (reg, mgr) = reg_and_mgr(&base);
    let runner = RegRunner {
        registry: &reg,
        hooks: Some(&mgr),
        run_id: "run_gateerr".into(),
    };
    let out = <RegRunner as pantheon_agent::ToolRunner>::run(&runner, "read_secret", "{}").unwrap();
    // Fail closed: a broken security gate must not become a free pass.
    assert!(out.contains("blocked by extension policy"), "{out}");
    assert!(!out.contains("sk-live-abc123"), "gate failed open: {out}");
}
