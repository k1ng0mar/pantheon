//! Behavioral tests for schedule templates: the built-in library,
//! `{{var}}` substitution, missing/unknown var errors, and the user
//! `templates.json` overlay (managed via `TemplateStore::save`).
use pantheon_scheduler::{
    apply_defaults, builtin_templates, is_reserved_var, render_prompt, ScheduleTemplate,
    TemplateSchedule, TemplateStore, TemplateVar,
};
use std::collections::HashMap;

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn builtin_library_has_exactly_the_eight_expected_templates() {
    let builtins = builtin_templates();
    let names: Vec<&str> = builtins.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "morning-briefing",
            "inbox-triage",
            "repo-watch",
            "dep-audit",
            "weekly-review",
            "cost-report",
            "gmail-monitor",
            "cost-watch",
        ]
    );
}

#[test]
fn builtin_schedules_are_sane() {
    let dir = tempfile::tempdir().unwrap();
    let store = TemplateStore::open(dir.path());
    let cron = |n: &str| match &store.get(n).unwrap().schedule {
        TemplateSchedule::Cron(e) => e.clone(),
        s => panic!("{n} should be cron, got {s:?}"),
    };
    let every = |n: &str| match &store.get(n).unwrap().schedule {
        TemplateSchedule::Every(d) => d.clone(),
        s => panic!("{n} should be interval, got {s:?}"),
    };
    assert_eq!(cron("morning-briefing"), "0 7 * * *");
    assert_eq!(cron("dep-audit"), "0 9 * * 1");
    assert_eq!(cron("weekly-review"), "0 8 * * 1");
    assert_eq!(cron("cost-report"), "0 9 * * 1");
    assert_eq!(every("inbox-triage"), "2h");
    assert_eq!(every("repo-watch"), "6h");
    assert_eq!(every("gmail-monitor"), "30m");
    assert_eq!(every("cost-watch"), "6h");
}

#[test]
fn render_substitutes_all_vars() {
    let dir = tempfile::tempdir().unwrap();
    let store = TemplateStore::open(dir.path());
    let t = store.get("morning-briefing").unwrap();
    let out = render_prompt(t, &vars(&[("topic", "Rust async")])).unwrap();
    assert!(out.contains("Rust async"));
    assert!(!out.contains("{{topic}}"));
}

#[test]
fn render_errors_naming_the_missing_var() {
    let dir = tempfile::tempdir().unwrap();
    let store = TemplateStore::open(dir.path());
    let t = store.get("repo-watch").unwrap();
    let err = render_prompt(t, &vars(&[])).unwrap_err();
    assert!(err.contains("repo"), "unexpected: {err}");
    assert!(err.contains("missing"), "unexpected: {err}");
}

#[test]
fn render_errors_on_unknown_var() {
    let dir = tempfile::tempdir().unwrap();
    let store = TemplateStore::open(dir.path());
    let t = store.get("inbox-triage").unwrap();
    let err = render_prompt(t, &vars(&[("channels", "telegram"), ("topc", "x")])).unwrap_err();
    assert!(err.contains("topc"), "unexpected: {err}");
}

#[test]
fn reserved_model_var_is_not_an_unknown_var() {
    assert!(is_reserved_var("model"));
    assert!(is_reserved_var("provider"));
    assert!(!is_reserved_var("topic"));
    let dir = tempfile::tempdir().unwrap();
    let store = TemplateStore::open(dir.path());
    let t = store.get("morning-briefing").unwrap();
    // The CLI intercepts `model` as the job's pin; render must not reject it.
    let out = render_prompt(t, &vars(&[("topic", "agents"), ("model", "cheap")])).unwrap();
    assert!(out.contains("agents"));
}

#[test]
fn apply_defaults_fills_declared_defaults_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = TemplateStore::open(dir.path());
    let t = store.get("cost-watch").unwrap();
    let mut v = vars(&[]);
    apply_defaults(t, &mut v);
    assert_eq!(v.get("threshold_pct").map(String::as_str), Some("5"));
    assert_eq!(v.get("target_price").map(String::as_str), Some(""));
    assert!(!v.contains_key("item"), "required vars have no default");
}

#[test]
fn user_template_overrides_builtin_and_adds_new() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = TemplateStore::open(dir.path());

    // A user template with a built-in's name overrides it.
    store
        .save(ScheduleTemplate {
            name: "morning-briefing".into(),
            description: "MY custom briefing.".into(),
            schedule: TemplateSchedule::Cron("0 6 * * *".into()),
            prompt: "Brief me on {{topic}}.".into(),
            vars: vec![TemplateVar {
                name: "topic".into(),
                question: "Topic?".into(),
                default: None,
            }],
        })
        .unwrap();
    // A brand-new name is added.
    store
        .save(ScheduleTemplate {
            name: "my-own".into(),
            description: "Brand new.".into(),
            schedule: TemplateSchedule::Every("1h".into()),
            prompt: "Do {{thing}}.".into(),
            vars: vec![TemplateVar {
                name: "thing".into(),
                question: "What?".into(),
                default: Some("stuff".into()),
            }],
        })
        .unwrap();

    let b = store.get("morning-briefing").unwrap();
    assert_eq!(b.description, "MY custom briefing.");
    assert!(matches!(
        b.schedule,
        TemplateSchedule::Cron(ref e) if e == "0 6 * * *"
    ));
    let n = store.get("my-own").unwrap();
    assert_eq!(n.vars[0].default.as_deref(), Some("stuff"));
    // Untouched builtins survive the overlay.
    assert!(store.get("repo-watch").is_some());

    // The overlay persists through templates.json: a fresh open sees it.
    let reopened = TemplateStore::open(dir.path());
    assert_eq!(
        reopened.get("morning-briefing").unwrap().description,
        "MY custom briefing."
    );
    assert!(reopened.get("my-own").is_some());
}

#[test]
fn broken_user_templates_are_skipped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();

    // Corrupt templates.json: the store opens with built-ins only.
    std::fs::write(dir.path().join("templates.json"), "{{{{ not json").unwrap();
    let store = TemplateStore::open(dir.path());
    assert!(store.get("morning-briefing").is_some());
    assert_eq!(store.list().len(), 8, "only the builtins remain");

    // One invalid entry among valid ones: skipped, the valid one loads.
    std::fs::write(
        dir.path().join("templates.json"),
        r#"{"templates": [
            {"name": "bad-cron", "description": "x",
             "schedule": {"cron": "not a cron"}, "prompt": "hi", "vars": []},
            {"name": "good-one", "description": "y",
             "schedule": {"every": "1h"}, "prompt": "do it", "vars": []}
        ]}"#,
    )
    .unwrap();
    let store = TemplateStore::open(dir.path());
    assert!(store.get("bad-cron").is_none(), "invalid entry is skipped");
    assert!(store.get("good-one").is_some());
    assert!(store.get("repo-watch").is_some(), "builtins survive");

    // save() validates loudly instead of persisting garbage.
    let mut store = store;
    let err = store
        .save(ScheduleTemplate {
            name: "bad-cron".into(),
            description: "x".into(),
            schedule: TemplateSchedule::Cron("not a cron".into()),
            prompt: "hi".into(),
            vars: vec![],
        })
        .unwrap_err();
    assert!(err.contains("invalid cron"), "unexpected error: {err}");
}

#[test]
fn scheduled_model_policy_prefers_pin_then_scheduled_aux() {
    // The old `Job::effective_model` unit is gone: scheduled model resolution
    // now lives in `build_scheduled_model_policy` - explicit pin wins, then
    // the `[scheduled]` auxiliary, never the interactive default.
    use pantheon_api::config::{AuxSection, Config};

    let mut cfg = Config::default();
    cfg.scheduled = Some(AuxSection {
        provider: "openai".to_string(),
        model: "cheap".to_string(),
        ..Default::default()
    });

    // Explicit pin beats the aux.
    let p = pantheon_tui::config::build_scheduled_model_policy(
        Some(&cfg),
        Some("anthropic".to_string()),
        Some("pricey".to_string()),
    );
    assert_eq!(p.default.model, "pricey");
    assert_eq!(p.default.provider, "anthropic");

    // No pin: the `[scheduled]` aux wins, never the interactive default.
    let p = pantheon_tui::config::build_scheduled_model_policy(Some(&cfg), None, None);
    assert_eq!(p.default.model, "cheap");
    assert_eq!(p.default.provider, "openai");
}
