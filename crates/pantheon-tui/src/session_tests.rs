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
