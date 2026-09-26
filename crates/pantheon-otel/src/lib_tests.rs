//! Tests for `pantheon_otel::tests` — sibling file so sources stay test-free.
use super::*;
#[test]
fn tool_events_become_tool_spans() {
    let s = span_for(&Event::ToolStarted {
        run_id: "r".into(),
        call_id: "call_0_0".into(),
        tool: "shell".into(),
        args: String::new(),
        provenance: Provenance::system("test"),
    })
    .unwrap();
    assert_eq!(s.kind, SpanKind::Tool);
    assert_eq!(s.name, "tool:shell");
}
#[test]
fn deltas_are_not_spans() {
    assert!(span_for(&Event::ModelDelta {
        run_id: "r".into(),
        delta: "x".into()
    })
    .is_none());
}
#[test]
fn metrics_fold_over_replay() {
    let evs = vec![
        Event::RunStarted { run_id: "r".into() },
        Event::ToolStarted {
            run_id: "r".into(),
            call_id: "call_0_0".into(),
            tool: "a".into(),
            args: String::new(),
            provenance: Provenance::system("test"),
        },
        Event::ModelCompleted { run_id: "r".into() },
        Event::RunCompleted { run_id: "r".into() },
    ];
    let m = metrics_from(&evs);
    assert_eq!(
        (
            m.runs_started,
            m.runs_completed,
            m.tool_calls,
            m.model_turns
        ),
        (1, 1, 1, 1)
    );
    assert!(render_events(&evs).contains("1 completed"));
}
