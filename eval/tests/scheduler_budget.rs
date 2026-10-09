//! Scheduled runs must honor `[budget]`: `run_job_now` serves agent
//! turns from a fresh `Session`, and the codebase contract
//! (`apply_budget_tiers`: "call on every `Session::new` that serves
//! turns") covers it. These tests pin the helper every serving path -
//! interactive, /agui, gateway, and scheduled runs - funnels through.

use pantheon_api::capability::Policy;
use pantheon_runtime::session::Session;

fn test_session(tag: &str) -> Session {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-sched-budget-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
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
    Session::new(
        dir,
        Policy::coder(),
        model_policy,
        pantheon_secrets::SecretsBroker::from_system_env(),
    )
    .unwrap()
}

fn load_cfg(dir: &std::path::Path, body: &str) -> Option<pantheon_tui::config::Config> {
    std::fs::write(dir.join("config.toml"), body).unwrap();
    // `load`, not `load_or_report`: a bad fixture must fail the test,
    // never exit the whole test process (report calls process::exit).
    Some(pantheon_tui::config::Config::load(dir).expect("fixture config must parse"))
}

#[test]
fn budget_tiers_reach_a_fresh_session() {
    // A scheduled run's session is built exactly like this: fresh
    // Session, file config, apply_budget_tiers. Custom tiers must land
    // on the live budget.
    let s = test_session("custom");
    let data_dir = s.supervisor.data_dir().clone();
    let cfg = load_cfg(
        &data_dir,
        "[budget]\nmax_turns = 7\nmax_tool_calls = 9\nmax_tokens = 1234\n",
    );
    pantheon_tui::config::apply_budget_tiers(&s, cfg.as_ref());
    let snap = s.budget_snapshot();
    assert_eq!(snap.max_turns, 7);
    assert_eq!(snap.max_tool_calls, 9);
    assert_eq!(snap.max_tokens, Some(1234));
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn absent_budget_section_means_runtime_defaults() {
    // No `[budget]` table: the session keeps compiled defaults (16
    // turns, 32 tool calls, uncapped tokens) - the same values an
    // uncustomized interactive session gets.
    let s = test_session("absent");
    let data_dir = s.supervisor.data_dir().clone();
    let cfg = load_cfg(&data_dir, "# no [budget] table: defaults apply\n");
    pantheon_tui::config::apply_budget_tiers(&s, cfg.as_ref());
    let snap = s.budget_snapshot();
    assert_eq!(snap.max_turns, 16);
    assert_eq!(snap.max_tool_calls, 32);
    assert_eq!(snap.max_tokens, None);
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn zero_budget_values_fall_back_to_defaults() {
    // A `0` cap would end every run before it starts, so it counts as
    // unset - same rule the interactive path documents.
    let s = test_session("zero");
    let data_dir = s.supervisor.data_dir().clone();
    let cfg = load_cfg(&data_dir, "[budget]\nmax_turns = 0\n");
    pantheon_tui::config::apply_budget_tiers(&s, cfg.as_ref());
    assert_eq!(s.budget_snapshot().max_turns, 16);
    let _ = std::fs::remove_dir_all(&data_dir);
}
