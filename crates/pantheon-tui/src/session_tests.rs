//! Tests for `crate::session::tests` — sibling file so sources stay test-free.
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
        usage: pantheon_providers::model_event::ModelUsage {
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

fn model_test_state() -> TuiState {
    let mut state = TuiState::new("sess1".into(), "openai/gpt-4o-mini".into(), 128_000);
    state.models = Some(build_model_rows());
    state
}

#[test]
fn models_overlay_lists_catalog_rows() {
    let state = model_test_state();
    let rows = state.filtered_models();
    assert!(!rows.is_empty(), "catalog yields browser rows");
    assert!(
        rows.iter().all(|r| !r.provider_id.is_empty()),
        "every row names a provider"
    );
}

#[test]
fn models_filter_matches_provider_and_model() {
    let mut state = model_test_state();
    let all = state.filtered_models().len();
    state.models_input = "zzz-no-such-model".into();
    assert!(
        state.filtered_models().is_empty(),
        "impossible filter matches nothing"
    );
    // The first row's provider id must match its own prefix: the filter
    // is a real substring match, not a no-op.
    let first_provider = state.models.as_ref().unwrap()[0].provider_id.clone();
    state.models_input = first_provider[..first_provider.len().min(3)].into();
    assert!(
        !state.filtered_models().is_empty() && state.filtered_models().len() <= all,
        "prefix filter narrows without emptying"
    );
    state.models_close();
    assert!(state.models.is_none(), "close clears the overlay");
    assert!(state.models_input.is_empty(), "close clears the filter");
    assert_eq!(state.models_sel, 0, "close resets selection");
}

#[test]
fn models_move_clamps_to_filtered_list() {
    let mut state = model_test_state();
    state.models_move(1);
    assert_eq!(state.models_sel, 1, "moves down one row");
    state.models_move(1_000_000);
    let last = state.filtered_models().len() - 1;
    assert_eq!(state.models_sel, last, "clamps at the end");
    state.models_move(-1_000_000);
    assert_eq!(state.models_sel, 0, "clamps at the start");
}

fn test_blocks() -> Vec<TranscriptBlock> {
    vec![
        TranscriptBlock {
            kind: BlockKind::UserMessage("hello".into()),
        },
        TranscriptBlock {
            kind: BlockKind::AssistantMessage("hi there".into()),
        },
        TranscriptBlock {
            kind: BlockKind::ToolCall {
                name: "shell.exec".into(),
                args: "ls".into(),
                ok: Some(true),
            },
        },
        TranscriptBlock {
            kind: BlockKind::Status("resumed run_1".into()),
        },
    ]
}

#[test]
fn export_markdown_keeps_roles_readable() {
    let out = export_transcript(&test_blocks(), "run_1", "markdown");
    assert!(out.contains("# Pantheon session run_1"), "titles the file");
    assert!(
        out.contains("## you") && out.contains("hello"),
        "user turn kept"
    );
    assert!(
        out.contains("## agent") && out.contains("hi there"),
        "assistant turn kept"
    );
    assert!(
        out.contains("shell.exec") && out.contains("done"),
        "tool call with outcome"
    );
}

#[test]
fn export_json_is_one_object_per_line() {
    let out = export_transcript(&test_blocks(), "run_1", "json");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "one object per block");
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert_eq!(v["session"], "run_1", "every row names the session");
        assert!(v["kind"].is_string() && v["text"].is_string());
    }
}

fn tui_test_session(tag: &str) -> std::sync::Arc<pantheon_runtime::session::Session> {
    let dir = std::env::temp_dir().join(format!("pantheon-tui-cmd-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let policy = pantheon_api::capability::Policy::coder_with_memory();
    let model_policy = pantheon_api::model::ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: pantheon_api::model::DefaultModel {
            provider: "test".into(),
            model: "test".into(),
        },
        fallbacks: pantheon_api::model::FallbackChain { fallbacks: vec![] },
        auxiliaries: vec![],
    };
    let secrets = pantheon_secrets::SecretsBroker::new();
    std::sync::Arc::new(
        pantheon_runtime::session::Session::new(dir, policy, model_policy, secrets).unwrap(),
    )
}

fn last_status(state: &TuiState) -> String {
    match state.blocks.last().map(|b| &b.kind) {
        Some(BlockKind::Status(t)) => t.clone(),
        other => panic!("expected trailing status, got {other:?}"),
    }
}

#[test]
fn slash_new_starts_an_unsaved_conversation() {
    let session = tui_test_session("new");
    let mut state = TuiState::new("old_run".into(), "test/test".into(), 0);
    state.blocks.push(TranscriptBlock {
        kind: BlockKind::UserMessage("old".into()),
    });
    handle_slash(&mut state, &session, "/new", &std::sync::mpsc::channel().0);
    assert_ne!(state.session_id, "old_run", "run id rotates");
    assert!(state.blocks.iter().any(|b| matches!(
        &b.kind,
        BlockKind::Status(t) if t.contains("new conversation")
    )));
    assert!(
        !state.blocks.iter().any(|b| matches!(
            &b.kind,
            BlockKind::UserMessage(t) if t == "old"
        )),
        "transcript cleared"
    );
}

#[test]
fn slash_remember_stores_user_memory() {
    let session = tui_test_session("remember");
    let mut state = TuiState::new("run_rem".into(), "test/test".into(), 0);
    handle_slash(
        &mut state,
        &session,
        "/remember launch-code up-up-down",
        &std::sync::mpsc::channel().0,
    );
    assert!(
        last_status(&state).contains("remembered launch-code"),
        "stored: {}",
        last_status(&state)
    );
}

#[test]
fn slash_remember_needs_key_and_text() {
    let session = tui_test_session("remember-usage");
    let mut state = TuiState::new("run_rem".into(), "test/test".into(), 0);
    handle_slash(
        &mut state,
        &session,
        "/remember",
        &std::sync::mpsc::channel().0,
    );
    assert!(last_status(&state).contains("usage"), "bare form teaches");
    handle_slash(
        &mut state,
        &session,
        "/remember lonelykey",
        &std::sync::mpsc::channel().0,
    );
    assert!(
        last_status(&state).contains("usage"),
        "key without text teaches"
    );
}

#[test]
fn slash_unknown_names_help() {
    let session = tui_test_session("unknown");
    let mut state = TuiState::new("run_x".into(), "test/test".into(), 0);
    handle_slash(
        &mut state,
        &session,
        "/frobnicate",
        &std::sync::mpsc::channel().0,
    );
    let s = last_status(&state);
    assert!(s.contains("unknown command") && s.contains("/help"), "{s}");
}

#[test]
fn slash_doctor_reports_healthy_or_names_fixes() {
    let session = tui_test_session("doctor");
    let mut state = TuiState::new("run_doc".into(), "test/test".into(), 0);
    handle_slash(
        &mut state,
        &session,
        "/doctor",
        &std::sync::mpsc::channel().0,
    );
    let s = last_status(&state);
    assert!(s.contains("doctor:"), "ends with a verdict: {s}");
    assert!(
        state.blocks.len() > 1,
        "checks render as their own lines, not one blob"
    );
}

#[test]
fn slash_gateway_reports_service_state() {
    let session = tui_test_session("gateway");
    let mut state = TuiState::new("run_gw".into(), "test/test".into(), 0);
    handle_slash(
        &mut state,
        &session,
        "/gateway",
        &std::sync::mpsc::channel().0,
    );
    let texts: Vec<String> = state
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Status(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|t| t.starts_with("gateway service:")),
        "service state shown: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.starts_with("outbox:")),
        "outbox depth shown: {texts:?}"
    );
}

fn reasoning_data_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pantheon-tui-rsn-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[model]\nprovider = \"test\"\nmodel = \"test\"\n",
    )
    .unwrap();
    dir
}

#[test]
fn persist_reasoning_writes_and_clears_the_key() {
    use pantheon_api::model::ReasoningLevel;
    let dd = reasoning_data_dir("persist");
    persist_reasoning(&dd, ReasoningLevel::High, None).unwrap();
    let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
    assert!(text.contains("reasoning"), "level written: {text}");
    persist_reasoning(&dd, ReasoningLevel::Off, None).unwrap();
    let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
    assert!(!text.contains("reasoning"), "off removes the key: {text}");
}

#[test]
fn persist_reasoning_refuses_without_a_model_section() {
    use pantheon_api::model::ReasoningLevel;
    let dd = std::env::temp_dir().join(format!("pantheon-tui-rsn-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dd);
    std::fs::create_dir_all(&dd).unwrap();
    let err = persist_reasoning(&dd, ReasoningLevel::High, None).unwrap_err();
    assert!(err.contains("[model]"), "says what is missing: {err}");
}

#[test]
fn slash_reasoning_shows_sets_and_rejects() {
    let _guard = crate::dotenv::test_support::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dd = reasoning_data_dir("slash");
    std::env::set_var("PANTHEON_DATA_DIR", &dd);
    let session = tui_test_session("reasoning");
    let mut state = TuiState::new("run_rsn".into(), "test/test".into(), 0);
    handle_slash(
        &mut state,
        &session,
        "/reasoning",
        &std::sync::mpsc::channel().0,
    );
    let texts: Vec<String> = state
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Status(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("reasoning: off")),
        "bare form shows the live level: {texts:?}"
    );
    handle_slash(
        &mut state,
        &session,
        "/reasoning high",
        &std::sync::mpsc::channel().0,
    );
    assert!(
        last_status(&state).contains("reasoning → high"),
        "set reports: {}",
        last_status(&state)
    );
    assert_eq!(
        session.reasoning(),
        pantheon_api::model::ReasoningLevel::High,
        "live session follows"
    );
    handle_slash(
        &mut state,
        &session,
        "/reasoning ultra",
        &std::sync::mpsc::channel().0,
    );
    assert!(
        last_status(&state).contains("usage"),
        "unknown level teaches"
    );
}

#[test]
fn persist_reasoning_round_trips_level_and_budget() {
    use pantheon_api::model::ReasoningLevel;
    let dd = reasoning_data_dir("budget");
    persist_reasoning(&dd, ReasoningLevel::High, Some(16_000)).unwrap();
    let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
    assert!(text.contains("reasoning"), "level written: {text}");
    assert!(text.contains("16000"), "budget written: {text}");
    // Setting a level preserves an existing budget.
    persist_reasoning(&dd, ReasoningLevel::Low, Some(16_000)).unwrap();
    let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
    assert!(
        text.contains("16000"),
        "budget survives a level change: {text}"
    );
    // Clearing both removes both keys.
    persist_reasoning(&dd, ReasoningLevel::Off, None).unwrap();
    let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
    assert!(!text.contains("reasoning"), "keys removed: {text}");
}

#[test]
fn slash_reasoning_budget_sets_and_clears() {
    let _guard = crate::dotenv::test_support::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dd = reasoning_data_dir("slash-budget");
    std::env::set_var("PANTHEON_DATA_DIR", &dd);
    let session = tui_test_session("reasoning-budget");
    let mut state = TuiState::new("run_rsnb".into(), "test/test".into(), 0);
    handle_slash(
        &mut state,
        &session,
        "/reasoning budget 16000",
        &std::sync::mpsc::channel().0,
    );
    assert_eq!(session.reasoning_budget(), Some(16_000));
    assert!(
        last_status(&state).contains("16000"),
        "reports the budget: {}",
        last_status(&state)
    );
    handle_slash(
        &mut state,
        &session,
        "/reasoning budget off",
        &std::sync::mpsc::channel().0,
    );
    assert_eq!(session.reasoning_budget(), None);
    handle_slash(
        &mut state,
        &session,
        "/reasoning budget banana",
        &std::sync::mpsc::channel().0,
    );
    assert!(last_status(&state).contains("usage"), "non-number teaches");
}

#[test]
fn scroll_stays_pinned_to_the_tail_by_default() {
    let mut s = TuiState::default();
    assert_eq!(s.scroll_offset, 0);
    s.scroll_down(10);
    assert_eq!(s.scroll_offset, 0, "cannot scroll below the tail");
}

#[test]
fn scroll_up_and_down_move_through_history() {
    let mut s = TuiState::default();
    s.scroll_up(10);
    s.scroll_up(5);
    assert_eq!(s.scroll_offset, 15);
    s.scroll_down(6);
    assert_eq!(s.scroll_offset, 9);
    s.scroll_to_bottom();
    assert_eq!(s.scroll_offset, 0);
}

#[test]
fn new_blocks_reset_the_scroll_to_the_tail() {
    let mut s = TuiState::default();
    s.scroll_up(50);
    s.add_status("fresh output".into());
    assert_eq!(
        s.scroll_offset, 0,
        "new output must be visible, not buried above the viewport"
    );
}

#[test]
fn begin_turn_starts_when_ready_and_queues_when_busy() {
    let mut s = TuiState::new("run_a".into(), "test/test".into(), 0);
    assert!(s.ready, "fresh state is ready");
    assert!(s.begin_turn("first".into()), "starts when ready");
    assert!(!s.ready, "marked busy for the turn");
    assert_eq!(s.status_line, "working");
    assert!(s.interrupt_armed_at.is_none(), "stale arm cleared");
    assert!(!s.interrupted, "stale interrupt cleared");
    assert!(s.queued_message.is_none(), "nothing queued on a clean start");

    // Second Enter while the turn runs: no second loop, message queues.
    assert!(!s.begin_turn("second".into()), "busy turn does not start");
    assert_eq!(s.queued_message.as_deref(), Some("second"), "message queued");
    assert!(!s.ready, "still busy, not clobbered");

    // A newer message replaces the older queued one: the single slot holds
    // the latest intent.
    assert!(!s.begin_turn("third".into()));
    assert_eq!(s.queued_message.as_deref(), Some("third"));
}

#[test]
fn queued_message_drains_once_and_clears_the_slot() {
    let mut s = TuiState::new("run_a".into(), "test/test".into(), 0);
    assert!(s.begin_turn("first".into()));
    assert!(!s.begin_turn("queued".into()));
    assert_eq!(s.take_queued().as_deref(), Some("queued"), "drains");
    assert!(s.queued_message.is_none(), "slot cleared after drain");
    assert!(s.take_queued().is_none(), "second drain is empty");
}

#[test]
fn send_path_resolves_the_currently_selected_run() {
    // Regression: the worker used to clone the immutable loop-open run id,
    // so a turn after history-resume wrote to the wrong run's history. The
    // send path now resolves state.session_id at send time; pin that the
    // resolution point follows the selection.
    let mut s = TuiState::new("run_old".into(), "test/test".into(), 0);
    assert_eq!(resolve_send_run_id(&s), "run_old");
    // history-resume, /resume and /new all write state.session_id:
    s.session_id = "run_new".into();
    assert_eq!(
        resolve_send_run_id(&s),
        "run_new",
        "send follows the selection"
    );
}

#[test]
fn interrupt_targets_the_in_flight_run() {
    // Regression: Esc used the same stale loop-open id as the send path.
    let mut s = TuiState::new("selected".into(), "test/test".into(), 0);
    assert_eq!(s.interrupt_target(), "selected", "falls back to selection");
    s.active_run = Some("in_flight".into());
    assert_eq!(
        s.interrupt_target(),
        "in_flight",
        "prefers the running turn's run"
    );
    s.active_run = None;
    assert_eq!(s.interrupt_target(), "selected", "back to selection");
}

// ---------- TUI-B: live status bar ----------

fn full_status_data() -> statusbar::StatusBarData {
    statusbar::StatusBarData {
        status_word: "working".into(),
        icon: "●".into(),
        model: "openai/gpt-4o".into(),
        context_frac: Some(0.125),
        context_label: Some("16.0k/128k".into()),
        turn_in: Some(1200),
        turn_out: Some(340),
        tokens_per_sec: Some(83.6),
        cache_hit_rate: None,
        turn_no: Some(3),
        session_prefix: "ab12cd".into(),
        cost_usd: Some(0.042),
    }
}

#[test]
fn status_bar_renders_real_values() {
    let line = statusbar::render(&full_status_data(), 200);
    assert!(line.contains("working"), "status word: {line}");
    assert!(line.contains("13%"), "context pct: {line}");
    assert!(line.contains("16.0k/128k"), "context label: {line}");
    assert!(line.contains("in 1.2k"), "turn in: {line}");
    assert!(line.contains("out 340"), "turn out: {line}");
    assert!(line.contains("84 tok/s"), "rate: {line}");
    assert!(line.contains("openai/gpt-4o"), "model: {line}");
    assert!(line.contains("t 3"), "turn no: {line}");
    assert!(line.contains("$0.04"), "cost: {line}");
    assert!(line.contains("sess ab12cd"), "session: {line}");
}

#[test]
fn status_bar_never_fakes_missing_values() {
    let mut d = full_status_data();
    d.context_frac = None;
    d.context_label = None;
    d.turn_in = None;
    d.turn_out = None;
    d.tokens_per_sec = None;
    d.turn_no = None;
    d.cost_usd = None;
    let line = statusbar::render(&d, 200);
    // Every unknown renders as —; no invented zeros anywhere.
    assert!(line.contains('—'), "missing values are dashes: {line}");
    assert!(!line.contains("0%"), "no fake 0%: {line}");
    assert!(!line.contains("0 tok/s"), "no fake rate: {line}");
    assert!(!line.contains("$0.00"), "no fake cost: {line}");
}

#[test]
fn status_bar_truncates_to_width() {
    let line = statusbar::render(&full_status_data(), 40);
    assert!(
        line.chars().count() <= 40,
        "fits the width: {}",
        line.chars().count()
    );
    assert!(line.ends_with('…'), "ellipsis marks truncation: {line}");
}

#[test]
fn status_bar_counts_format() {
    assert_eq!(statusbar::fmt_count(999), "999");
    assert_eq!(statusbar::fmt_count(1200), "1.2k");
    assert_eq!(statusbar::fmt_count(2_500_000), "2.5M");
    assert_eq!(statusbar::fmt_pct(0.125), "13%");
}

#[test]
fn usage_event_records_per_turn_tokens() {
    let mut s = TuiState::new("sess1".into(), "m".into(), 128_000);
    s.begin_turn("hi".into());
    s.handle_model_event(ModelEvent::Usage {
        usage: pantheon_providers::model_event::ModelUsage {
            input_tokens: 1200,
            output_tokens: 340,
            total_tokens: 1540,
            cost_usd: Some(0.0042),
        },
    });
    assert_eq!(s.turn_in, Some(1200));
    assert_eq!(s.turn_out, Some(340));
    assert_eq!(s.tokens_used, 1540, "authoritative snap still applies");
    // A new turn resets the per-turn counters so they never leak across.
    s.on_turn_complete(true);
    s.begin_turn("again".into());
    assert_eq!(s.turn_in, None, "turn in resets");
    assert_eq!(s.turn_out, None, "turn out resets");
}

#[test]
fn turn_rate_is_none_without_a_running_turn() {
    let s = TuiState::new("sess1".into(), "m".into(), 128_000);
    assert_eq!(s.turn_rate(), None, "idle: no rate to report");
}

#[test]
fn display_turn_no_is_none_before_the_first_turn() {
    let s = TuiState::new("sess1".into(), "m".into(), 128_000);
    assert_eq!(s.display_turn_no(), None, "nothing ran yet: —");
}

// ---------- TUI-B: draft editor ----------

#[test]
fn editor_roundtrips_text() {
    let ed = editor::DraftEditor::from_text("hello\nworld");
    assert_eq!(ed.text(), "hello\nworld");
    assert_eq!(ed.line_count(), 2);
    assert_eq!(ed.char_count(), 11);
}

#[test]
fn editor_insert_and_newline() {
    let mut ed = editor::DraftEditor::from_text("ab");
    ed.home();
    ed.insert_char('X');
    assert_eq!(ed.text(), "Xab");
    ed.end();
    ed.newline();
    ed.insert_char('c');
    assert_eq!(ed.text(), "Xab\nc");
}

#[test]
fn editor_backspace_joins_lines() {
    let mut ed = editor::DraftEditor::from_text("ab\ncd");
    ed.move_down();
    ed.home();
    ed.backspace();
    assert_eq!(ed.text(), "abcd", "backspace at col 0 joins");
    ed.home();
    ed.move_right();
    ed.delete();
    assert_eq!(ed.text(), "acd", "delete removes under cursor");
}

#[test]
fn editor_cursor_moves_clamp() {
    let mut ed = editor::DraftEditor::from_text("a\nbcdef");
    assert_eq!(ed.cursor(), (1, 5), "opens at the end of the draft");
    ed.move_up();
    assert_eq!(ed.cursor(), (0, 1), "up clamps col to the shorter line");
    ed.move_up();
    assert_eq!(ed.cursor(), (0, 0), "up from the first row goes home");
    ed.move_down();
    assert_eq!(ed.cursor(), (1, 0), "down keeps the column");
    ed.move_down();
    assert_eq!(ed.cursor(), (1, 5), "down past the end goes to line end");
    ed.move_left();
    ed.move_left();
    assert_eq!(ed.cursor(), (1, 3));
    ed.move_right();
    ed.move_right();
    ed.move_right();
    assert_eq!(ed.cursor(), (1, 5), "right clamps at line end");
}

#[test]
fn editor_is_multibyte_safe() {
    let mut ed = editor::DraftEditor::from_text("héllo ◈");
    ed.home();
    ed.move_right();
    ed.move_right();
    ed.insert_char('X');
    assert_eq!(ed.text(), "héXllo ◈", "char ops never split a character");
    ed.end();
    ed.backspace();
    assert_eq!(ed.text(), "héXllo ", "backspace removes one char");
}

#[test]
fn open_editor_carries_the_draft() {
    let mut s = TuiState::new("sess1".into(), "m".into(), 128_000);
    s.input = "partial thought".into();
    s.is_inputting = true;
    s.open_editor();
    assert!(s.editor.is_some());
    assert_eq!(s.editor.as_ref().unwrap().text(), "partial thought");
    assert!(s.input.is_empty(), "draft moved, not copied");
    assert!(!s.is_inputting, "inline input yields to the editor");
}

// ---------- TUI-B: turn timeline ----------

fn two_turn_blocks() -> Vec<TranscriptBlock> {
    vec![
        TranscriptBlock { kind: BlockKind::Status("boot".into()) },
        TranscriptBlock { kind: BlockKind::UserMessage("first question".into()) },
        TranscriptBlock { kind: BlockKind::AssistantMessage("first answer".into()) },
        TranscriptBlock { kind: BlockKind::UserMessage("second question".into()) },
        TranscriptBlock { kind: BlockKind::Thinking("hmm".into()) },
        TranscriptBlock { kind: BlockKind::AssistantMessage("second answer".into()) },
    ]
}

#[test]
fn timeline_builds_one_turn_per_user_message() {
    let turns = timeline::build_turns(&two_turn_blocks());
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].turn_no, 1);
    assert_eq!(turns[0].block_start, 1, "skips the pre-turn status block");
    assert_eq!(turns[1].block_start, 3);
    assert!(turns[0].preview.contains("first question"));
}

#[test]
fn timeline_nav_clamps_and_opens_on_latest() {
    let mut nav = timeline::TimelineNav::open(3);
    assert_eq!(nav.sel(), 2, "opens on the latest turn");
    nav.move_sel(5, 3);
    assert_eq!(nav.sel(), 2, "clamps at the end");
    nav.move_sel(-10, 3);
    assert_eq!(nav.sel(), 0, "clamps at the start");
    nav.move_sel(1, 3);
    assert_eq!(nav.sel(), 1);
    let mut empty = timeline::TimelineNav::open(0);
    empty.move_sel(1, 0);
    assert_eq!(empty.sel(), 0, "empty rail is a no-op");
}

#[test]
fn toggle_timeline_is_reversible() {
    let mut s = TuiState::new("sess1".into(), "m".into(), 128_000);
    s.blocks = two_turn_blocks();
    assert!(s.timeline.is_none());
    s.toggle_timeline();
    assert_eq!(s.timeline.as_ref().unwrap().sel(), 1, "opens on latest");
    s.toggle_timeline();
    assert!(s.timeline.is_none(), "toggles closed");
}

// ---------- TUI-B: double-Esc rewind ----------

fn rewound_state() -> TuiState {
    let mut s = TuiState::new("sess1".into(), "m".into(), 128_000);
    s.blocks = two_turn_blocks();
    s
}

#[test]
fn rewind_candidate_finds_the_last_turn() {
    let s = rewound_state();
    let offer = s.rewind_candidate().expect("two turns available");
    assert_eq!(offer.turn_no, 2);
    assert_eq!(offer.block_index, 3);
    assert!(offer.preview.contains("second question"));
}

#[test]
fn rewind_candidate_refuses_unsafe_states() {
    let mut s = rewound_state();
    s.ready = false;
    assert!(s.rewind_candidate().is_none(), "no rewind mid-turn");
    s.ready = true;
    s.pending_approval = Some(("r".into(), "scope".into()));
    assert!(s.rewind_candidate().is_none(), "no rewind while parked");
    s.pending_approval = None;
    s.active_run = Some("r".into());
    assert!(s.rewind_candidate().is_none(), "no rewind with an active run");
    let empty = TuiState::new("sess1".into(), "m".into(), 128_000);
    assert!(empty.rewind_candidate().is_none(), "no turns: no offer");
}

#[test]
fn idle_double_esc_offers_rewind() {
    let mut s = rewound_state();
    assert!(!s.press_esc(), "first esc only arms");
    assert!(!s.press_esc(), "second esc never interrupts while idle");
    let offer = s.rewind_offer.as_ref().expect("rewind offered");
    assert_eq!(offer.turn_no, 2);
    assert_eq!(s.status_line, "rewind last turn? [y]es [n]o");
}

#[test]
fn idle_double_esc_without_turns_disarms_as_before() {
    let mut s = TuiState::new("sess1".into(), "m".into(), 128_000);
    s.press_esc();
    s.press_esc();
    assert!(s.rewind_offer.is_none(), "nothing to rewind");
    assert_eq!(s.status_line, "ready");
}

// ---------- TUI-B: background sessions ----------

#[test]
fn background_completion_raises_attention_without_touching_input() {
    let mut s = TuiState::new("sess1".into(), "m".into(), 128_000);
    s.input = "typing here".into();
    s.is_inputting = true;
    s.begin_turn("work".into());
    s.on_turn_complete(false);
    assert!(s.attention, "badge raised for background finish");
    assert_eq!(s.attention_note.as_deref(), Some("turn 1 finished"));
    assert!(s.ready, "turn machinery still cycles");
    assert_eq!(s.input, "typing here", "input untouched");
    assert!(s.is_inputting, "focus never stolen");
    assert_eq!(s.turns_completed, 1);
}

#[test]
fn focused_completion_clears_attention() {
    let mut s = TuiState::new("sess1".into(), "m".into(), 128_000);
    s.begin_turn("work".into());
    s.on_turn_complete(false);
    assert!(s.attention);
    s.attention_clear();
    assert!(!s.attention, "tab focus clears the badge");
    assert!(s.attention_note.is_none());
    s.begin_turn("more".into());
    s.on_turn_complete(true);
    assert!(!s.attention, "focused finish raises no badge");
    assert_eq!(s.turns_completed, 2);
}

#[test]
fn begin_turn_guard_still_queues() {
    let mut s = TuiState::new("sess1".into(), "m".into(), 128_000);
    assert!(s.begin_turn("first".into()));
    assert!(!s.begin_turn("second".into()), "one turn per session");
    assert_eq!(s.queued_message.as_deref(), Some("second"));
    assert_eq!(s.take_queued().as_deref(), Some("second"), "queue drains once");
    assert!(s.take_queued().is_none());
}

#[test]
fn rewind_appends_marker_and_truncates_view() {
    let session = tui_test_session("rewind");
    session.supervisor.start_run("run_rewind").unwrap();
    let mut state = TuiState::new("run_rewind".into(), "test/test".into(), 128_000);
    state.blocks = vec![
        TranscriptBlock {
            kind: BlockKind::UserMessage("do the thing".into()),
        },
        TranscriptBlock {
            kind: BlockKind::AssistantMessage("did it".into()),
        },
    ];
    state.turns_completed = 1;
    let offer = state.rewind_candidate().expect("candidate");
    state.rewind_offer = Some(offer);
    do_rewind(&mut state, &session);
    // View truncated to the pre-turn state; the discarded message returns
    // as the draft so the turn can be redone.
    assert!(
        !state
            .blocks
            .iter()
            .any(|b| matches!(&b.kind, BlockKind::UserMessage(_))),
        "rewound turn hidden from the view"
    );
    assert_eq!(state.input, "do the thing", "draft restored");
    assert!(
        state
            .blocks
            .iter()
            .any(|b| matches!(&b.kind, BlockKind::Status(t) if t.contains("rewound"))),
        "honest marker in the transcript"
    );
    assert_eq!(state.turns_completed, 0, "turn count rolls back with the view");
    // The ledger is append-only: the marker landed, history was rewritten
    // nowhere.
    let entries = session.supervisor.replay("run_rewind").unwrap();
    assert!(
        entries.iter().any(|e| serde_json::to_string(&e.event)
            .map(|s| s.contains("rewind:"))
            .unwrap_or(false)),
        "rewind marker event is in the ledger"
    );
}
// (The emit-failure guard in do_rewind — keep the turn when the marker
// write fails — is not unit-testable: ledger append does not require a
// started run, and there is no clean way to force the write to fail.)

