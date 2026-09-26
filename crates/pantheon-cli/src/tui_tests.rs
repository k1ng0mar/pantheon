//! Tests for `pantheon_cli::tui::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn thinking_collapses_when_not_last() {
    let mut lines = Vec::new();
    let block = TranscriptBlock {
        kind: BlockKind::Thinking("first line of reasoning\nsecond line\nthird".into()),
    };
    render_block(&mut lines, &block, false, false);
    // Collapsed: exactly one line, contains the marker.
    assert_eq!(lines.len(), 1, "collapsed thinking is one line");
    let s = format!("{:?}", lines[0]);
    assert!(s.contains("Thought"), "summary marker present: {s}");
    assert!(s.contains("first line"), "summary carries head of text");
}

#[test]
fn thinking_expands_when_last() {
    let mut lines = Vec::new();
    let block = TranscriptBlock {
        kind: BlockKind::Thinking("line one\nline two".into()),
    };
    render_block(&mut lines, &block, true, false);
    // Expanded: header + 2 content lines.
    assert_eq!(lines.len(), 3, "expanded thinking shows all lines");
}

#[test]
fn tool_call_flips_status_glyph() {
    let mut lines = Vec::new();
    let mut block = TranscriptBlock {
        kind: BlockKind::ToolCall {
            name: "shell.exec".into(),
            args: "ls".into(),
            ok: None,
        },
    };
    render_block(&mut lines, &block, true, false);
    let running = format!("{:?}", lines[0]);
    assert!(
        running.contains('\u{25cf}'),
        "running glyph while ok=None: {running}"
    );

    if let BlockKind::ToolCall { ok, .. } = &mut block.kind {
        *ok = Some(true);
    }
    lines.clear();
    render_block(&mut lines, &block, true, false);
    let done = format!("{:?}", lines[0]);
    assert!(
        done.contains('\u{2713}'),
        "done glyph after completion: {done}"
    );
}

#[test]
fn live_estimate_accumulates_and_snaps() {
    let mut state = TuiState::new("sess1234".into(), "opus".into(), 200_000);
    state.handle_model_event(ModelEvent::TextDelta {
        text: "x".repeat(40),
    });
    assert_eq!(state.turn_estimate, 10, "40 chars / 4 = 10 tokens");
    state.handle_model_event(ModelEvent::Usage {
        usage: pantheon_core::model_event::ModelUsage {
            input_tokens: 100,
            output_tokens: 10,
            total_tokens: 110,
            cost_usd: Some(0.01),
        },
    });
    assert_eq!(state.tokens_used, 110, "snapped to authoritative");
    assert_eq!(state.turn_estimate, 0, "estimate reset after snap");
    assert_eq!(state.cost_cents, 1);
}
