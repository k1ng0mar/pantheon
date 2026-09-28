//! Behavioral evals for run budgets and the `/goal`, `/tokens`, `/set`
//! slash commands.
//!
//! Covers: `[budget]`/`[goal]` config parsing and resolution (defaults,
//! zero-means-unset), the three commands' registry entries, the `/goal`
//! iteration gate on `TuiState`, and live budget retuning on a real
//! `Session` (`set_budget`/`budget_snapshot`).
//! Run with `cargo test -p pantheon-eval`.

use pantheon_api::capability::Policy;
use pantheon_tui::commands;
use pantheon_tui::config::Config;
use pantheon_tui::session::{ActiveGoal, TuiState};

fn write_config(dir: &std::path::Path, toml: &str) -> Config {
    std::fs::write(dir.join("config.toml"), toml).unwrap();
    Config::load(dir).expect("config must parse")
}

fn session_in(dir: &std::path::Path) -> pantheon_runtime::session::Session {
    let model_policy = pantheon_api::model::ModelPolicy {
        default: pantheon_api::model::DefaultModel {
            provider: "openai".into(),
            model: "test-model".into(),
        },
        fallbacks: pantheon_api::model::FallbackChain {
            fallbacks: Vec::new(),
        },
        auxiliaries: Vec::new(),
        reasoning: pantheon_api::model::ReasoningLevel::default(),
        reasoning_budget: None,
    };
    pantheon_runtime::session::Session::new(
        dir.to_path_buf(),
        Policy::coder(),
        model_policy,
        pantheon_secrets::SecretsBroker::new(),
    )
    .expect("session must open")
}

// ------------------------------------------------------------ config ---

#[test]
fn budget_section_resolves_config_values() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(
        dir.path(),
        r#"
[budget]
max_turns = 24
max_tool_calls = 40
max_delegate_depth = 3
max_iterations = 7
max_tokens = 50000

[goal]
max_iterations = 5
"#,
    );
    let b = cfg.budget();
    assert_eq!(b.max_turns, 24);
    assert_eq!(b.max_tool_calls, 40);
    assert_eq!(b.max_delegate_depth, 3);
    assert_eq!(b.max_tokens, Some(50000));
    assert_eq!(cfg.pipeline_iterations(), 7);
    assert_eq!(cfg.goal_iterations(), 5);
}

#[test]
fn budget_defaults_when_sections_absent() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path(), "# empty\n");
    let b = cfg.budget();
    assert_eq!(b.max_turns, 16);
    assert_eq!(b.max_tool_calls, 32);
    assert_eq!(b.max_delegate_depth, 2);
    assert_eq!(b.max_tokens, None, "token cap is strictly optional");
    assert_eq!(cfg.pipeline_iterations(), 3);
    assert_eq!(cfg.goal_iterations(), 10);
}

#[test]
fn budget_zero_is_treated_as_unset() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(
        dir.path(),
        "[budget]\nmax_turns = 0\nmax_tokens = 0\nmax_iterations = 0\n",
    );
    // A zero cap would end every run before it starts; it falls back to
    // the default (uncapped for tokens) instead of bricking the session.
    let b = cfg.budget();
    assert_eq!(b.max_turns, 16);
    assert_eq!(b.max_tokens, None);
    assert_eq!(cfg.pipeline_iterations(), 3);
}

#[test]
fn partial_budget_section_fills_rest_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path(), "[budget]\nmax_turns = 8\n");
    let b = cfg.budget();
    assert_eq!(b.max_turns, 8);
    assert_eq!(b.max_tool_calls, 32);
    assert_eq!(b.max_tokens, None);
}

// ---------------------------------------------------------- registry ---

#[test]
fn goal_tokens_set_are_registered_builtins() {
    let reg = commands::registry();
    for name in ["goal", "tokens", "set"] {
        assert!(reg.contains_key(name), "/{name} must be registered");
        assert!(commands::is_builtin(name), "/{name} must be builtin");
    }
    assert_eq!(
        reg["goal"].desc,
        "set or show the session goal (iteration-limited)"
    );
    let completions = commands::complete("/to");
    assert!(
        completions.contains(&"/tokens".to_string()),
        "palette suggests /tokens: {completions:?}"
    );
    let completions = commands::complete("/go");
    assert!(
        completions.contains(&"/goal".to_string()),
        "palette suggests /goal: {completions:?}"
    );
}

// -------------------------------------------------------- goal gate ---

fn goal(text: &str, used: u32, max: u32) -> ActiveGoal {
    ActiveGoal {
        text: text.into(),
        iterations_used: used,
        max_iterations: max,
    }
}

#[test]
fn goal_gate_refuses_at_cap() {
    let state = TuiState {
        goal: Some(goal("ship it", 10, 10)),
        ..Default::default()
    };
    let refusal = state.goal_refusal().expect("must refuse at cap");
    assert!(
        refusal.contains("10/10"),
        "refusal names the exhausted budget: {refusal}"
    );
    assert!(refusal.contains("/goal clear"), "refusal offers a way out");
}

#[test]
fn goal_gate_allows_below_cap_and_consumes() {
    let mut state = TuiState {
        goal: Some(goal("ship it", 9, 10)),
        ..Default::default()
    };
    assert!(state.goal_refusal().is_none(), "one iteration left");
    state.consume_goal_iteration();
    assert_eq!(state.goal.as_ref().unwrap().iterations_used, 10);
    assert!(
        state.goal_refusal().is_some(),
        "cap reached after consuming"
    );
}

#[test]
fn goal_gate_is_inert_without_a_goal() {
    let mut state = TuiState::default();
    assert!(state.goal_refusal().is_none());
    state.consume_goal_iteration(); // must not panic, must not create a goal
    assert!(state.goal.is_none());
}

#[test]
fn goal_consume_saturates_instead_of_wrapping() {
    let mut state = TuiState {
        goal: Some(goal("ship it", u32::MAX, u32::MAX)),
        ..Default::default()
    };
    state.consume_goal_iteration();
    assert_eq!(state.goal.as_ref().unwrap().iterations_used, u32::MAX);
}

// ----------------------------------------------------- live budget ---

#[test]
fn session_budget_defaults_match_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let session = session_in(dir.path());
    let b = session.budget_snapshot();
    assert_eq!(b.max_turns, 16);
    assert_eq!(b.max_tool_calls, 32);
    assert_eq!(b.max_delegate_depth, 2);
    assert_eq!(b.max_tokens, None);
}

#[test]
fn session_budget_retunes_live() {
    let dir = tempfile::tempdir().unwrap();
    let session = session_in(dir.path());
    let mut b = session.budget_snapshot();
    b.max_turns = 24;
    b.max_tokens = Some(1000);
    session.set_budget(b);
    let after = session.budget_snapshot();
    assert_eq!(after.max_turns, 24);
    assert_eq!(after.max_tokens, Some(1000));
    // Untouched keys keep their values.
    assert_eq!(after.max_tool_calls, 32);
}

#[test]
fn config_budget_applies_to_a_live_session() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path(), "[budget]\nmax_turns = 24\nmax_tokens = 50000\n");
    let session = session_in(dir.path());
    session.set_budget(cfg.budget());
    let b = session.budget_snapshot();
    assert_eq!(b.max_turns, 24);
    assert_eq!(b.max_tokens, Some(50000));
}

#[test]
fn session_goal_mirrors_without_affecting_budget() {
    let dir = tempfile::tempdir().unwrap();
    let session = session_in(dir.path());
    session.set_goal(Some("refactor the auth layer".into()));
    // Setting a goal must not disturb the budget, and clearing it must
    // not either.
    assert_eq!(session.budget_snapshot().max_turns, 16);
    session.set_goal(None);
    assert_eq!(session.budget_snapshot().max_turns, 16);
}
