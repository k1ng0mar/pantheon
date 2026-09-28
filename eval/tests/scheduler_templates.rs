//! Behavioral tests for schedule templates: the built-in library,
//! `{{var}}` substitution, missing/unknown var errors, and the user
//! `<data_dir>/templates/*.toml` overlay.
use pantheon_scheduler::{
    apply_defaults, builtin_templates, is_reserved_var, render_prompt, Job, ScheduleKind,
    TemplateSchedule, TemplateStore,
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
    let store = TemplateStore::builtins();
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
    let store = TemplateStore::builtins();
    let t = store.get("morning-briefing").unwrap();
    let out = render_prompt(t, &vars(&[("topic", "Rust async")])).unwrap();
    assert!(out.contains("Rust async"));
    assert!(!out.contains("{{topic}}"));
}

#[test]
fn render_errors_naming_the_missing_var() {
    let store = TemplateStore::builtins();
    let t = store.get("repo-watch").unwrap();
    let err = render_prompt(t, &vars(&[])).unwrap_err();
    assert!(err.contains("repo"), "unexpected: {err}");
    assert!(err.contains("missing"), "unexpected: {err}");
}

#[test]
fn render_errors_on_unknown_var() {
    let store = TemplateStore::builtins();
    let t = store.get("inbox-triage").unwrap();
    let err = render_prompt(t, &vars(&[("channels", "telegram"), ("topc", "x")])).unwrap_err();
    assert!(err.contains("topc"), "unexpected: {err}");
}

#[test]
fn reserved_model_var_is_not_an_unknown_var() {
    assert!(is_reserved_var("model"));
    assert!(is_reserved_var("provider"));
    assert!(!is_reserved_var("topic"));
    let store = TemplateStore::builtins();
    let t = store.get("morning-briefing").unwrap();
    // The CLI intercepts `model` as the job's pin; render must not reject it.
    let out = render_prompt(t, &vars(&[("topic", "agents"), ("model", "cheap")])).unwrap();
    assert!(out.contains("agents"));
}

#[test]
fn apply_defaults_fills_declared_defaults_only() {
    let store = TemplateStore::builtins();
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
    let tdir = dir.path().join("templates");
    std::fs::create_dir_all(&tdir).unwrap();
    std::fs::write(
        tdir.join("morning-briefing.toml"),
        r#"
name = "morning-briefing"
description = "MY custom briefing."
schedule_cron = "0 6 * * *"
prompt = "Brief me on {{topic}}."
[[vars]]
name = "topic"
question = "Topic?"
"#,
    )
    .unwrap();
    std::fs::write(
        tdir.join("my-own.toml"),
        r#"
name = "my-own"
description = "Brand new."
schedule_every = "1h"
prompt = "Do {{thing}}."
[[vars]]
name = "thing"
question = "What?"
default = "stuff"
"#,
    )
    .unwrap();

    let store = TemplateStore::load(dir.path());
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
}

#[test]
fn broken_user_templates_are_skipped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let tdir = dir.path().join("templates");
    std::fs::create_dir_all(&tdir).unwrap();
    // Bad cron.
    std::fs::write(
        tdir.join("bad-cron.toml"),
        "name = \"bad-cron\"\ndescription = \"x\"\nschedule_cron = \"not a cron\"\nprompt = \"hi\"\n",
    )
    .unwrap();
    // Not TOML at all.
    std::fs::write(tdir.join("garbage.toml"), "{{{{ not toml").unwrap();
    // Missing schedule.
    std::fs::write(
        tdir.join("no-schedule.toml"),
        "name = \"no-schedule\"\ndescription = \"x\"\nprompt = \"hi\"\n",
    )
    .unwrap();

    let store = TemplateStore::load(dir.path());
    assert!(store.get("bad-cron").is_none());
    assert!(store.get("garbage").is_none());
    assert!(store.get("no-schedule").is_none());
    assert_eq!(store.list().len(), 8, "only the builtins remain");
}

#[test]
fn effective_model_prefers_pin_then_scheduled_aux_then_default() {
    let mut j = Job::new("j1", ScheduleKind::Manual, "nyx");
    // Unpinned: the `[scheduled]` auxiliary wins over the interactive default.
    assert_eq!(j.effective_model(Some("cheap"), "big"), "cheap");
    // No aux configured: the default, as before.
    assert_eq!(j.effective_model(None, "big"), "big");
    // A pin beats both.
    j.pin_model("pricey", Some("openai")).unwrap();
    assert_eq!(j.effective_model(Some("cheap"), "big"), "pricey");
    assert_eq!(j.provider.as_deref(), Some("openai"));
}
