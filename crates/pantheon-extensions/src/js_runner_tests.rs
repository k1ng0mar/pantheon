//! Tests for the JavaScript runner. These execute a real interpreter, so they
//! skip cleanly when neither `bun` nor `node` is installed rather than failing
//! for an environmental reason.
use super::*;
use crate::compat::{detect_kind, entry_file, inspect, read_manifest, CompatKind};
use std::fs;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-jsrun-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write(d: &Path, rel: &str, body: &str) {
    let p = d.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn input() -> HookInput {
    HookInput {
        hook: "pre_llm_call".into(),
        session_id: "s1".into(),
        platform: "cli".into(),
        extra: Default::default(),
    }
}

/// Build a JsPlugin from a directory, using the real detector.
fn plugin_from(dir: &Path) -> Option<JsPlugin> {
    let kind = detect_kind(dir)?;
    let (name, _, _) = read_manifest(dir, kind)?;
    let entry = entry_file(dir, kind)?;
    let r = inspect(dir)?;
    Some(JsPlugin::load(dir, entry, name, r.mapped, r.unsupported))
}

fn runtime() -> Option<JsRunnerConfig> {
    JsRunnerConfig::detect().ok()
}

#[test]
fn shim_is_valid_esm_for_the_detected_runtime() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("shim-valid");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('before_prompt_build', ()=>({context:'x'})); }",
    );
    let p = plugin_from(&d).unwrap();
    // Must not error: a syntax error in the shim would surface here.
    let out = fire_hook_verbose(&p, Hook::PreLlmCall, &input(), &cfg).unwrap();
    assert!(out.error.is_none(), "shim error: {:?}", out.error);
}

#[test]
fn mapped_handler_context_reaches_the_caller() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("ctx");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('before_prompt_build', () => ({ context: 'injected' })); }",
    );
    let p = plugin_from(&d).unwrap();
    let got = fire_hook(&p, Hook::PreLlmCall, &input(), &cfg).unwrap();
    assert_eq!(got.as_deref(), Some("injected"));
}

#[test]
fn an_unmapped_event_produces_no_context() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("unmapped");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('before_tool_call', () => ({ context: 'should not run' })); }",
    );
    let p = plugin_from(&d).unwrap();
    // The handler is registered against a hook Pantheon never fires, so
    // pre_llm_call must come back silent.
    assert!(fire_hook(&p, Hook::PreLlmCall, &input(), &cfg)
        .unwrap()
        .is_none());
    assert!(p.provides(Hook::PreLlmCall) == false);
}

#[test]
fn unmapped_events_are_reported_in_the_envelope() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("dropped");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('before_tool_call', ()=>{}); api.on('message_sending', ()=>{}); }",
    );
    let p = plugin_from(&d).unwrap();
    let out = fire_hook_verbose(&p, Hook::PreLlmCall, &input(), &cfg).unwrap();
    // `before_tool_call` now maps to a real hook, so it binds rather than
    // being dropped; `message_sending` still has no equivalent.
    assert!(
        !out.dropped.contains(&"before_tool_call".to_string()),
        "{:?}",
        out.dropped
    );
    assert!(out.dropped.contains(&"message_sending".to_string()));
}

#[test]
fn refused_capabilities_are_reported_not_silently_ignored() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("refused");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ api.registerProvider({}); api.registerTool({}); }",
    );
    let p = plugin_from(&d).unwrap();
    let out = fire_hook_verbose(&p, Hook::PreLlmCall, &input(), &cfg).unwrap();
    assert!(
        out.refused.contains(&"registerProvider".to_string()),
        "{:?}",
        out.refused
    );
    assert!(out.refused.contains(&"registerTool".to_string()));
}

#[test]
fn a_throwing_plugin_fails_open() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("throws");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ throw new Error('boom'); }",
    );
    let p = plugin_from(&d).unwrap();
    // Must not propagate: a broken plugin never breaks a turn.
    assert!(fire_hook(&p, Hook::PreLlmCall, &input(), &cfg)
        .unwrap()
        .is_none());
}

#[test]
fn a_plugin_exiting_nonzero_fails_open() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("nonzero");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(&d, "index.js", "process.exit(3);");
    let p = plugin_from(&d).unwrap();
    assert!(fire_hook(&p, Hook::PreLlmCall, &input(), &cfg)
        .unwrap()
        .is_none());
}

#[test]
fn a_plugin_with_no_register_exports_fails_open() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("no-register");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(&d, "index.js", "export const nothing = 1;");
    let p = plugin_from(&d).unwrap();
    assert!(fire_hook(&p, Hook::PreLlmCall, &input(), &cfg)
        .unwrap()
        .is_none());
}

#[test]
fn an_async_handler_is_awaited() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("async");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('before_prompt_build', async () => { await new Promise(r=>setTimeout(r,10)); return { context: 'async-ok' }; }); }",
    );
    let p = plugin_from(&d).unwrap();
    assert_eq!(
        fire_hook(&p, Hook::PreLlmCall, &input(), &cfg)
            .unwrap()
            .as_deref(),
        Some("async-ok")
    );
}

#[test]
fn multiple_handlers_for_one_hook_are_concatenated() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("multi");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){\
           api.on('before_prompt_build', () => ({ context: 'a' }));\
           api.on('before_prompt_build', () => ({ context: 'b' }));\
         }",
    );
    let p = plugin_from(&d).unwrap();
    let got = fire_hook(&p, Hook::PreLlmCall, &input(), &cfg)
        .unwrap()
        .unwrap();
    assert!(got.contains('a') && got.contains('b'), "{got}");
}

#[test]
fn empty_context_is_treated_as_silent() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("empty");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('before_prompt_build', () => ({ context: '   ' })); }",
    );
    let p = plugin_from(&d).unwrap();
    assert!(fire_hook(&p, Hook::PreLlmCall, &input(), &cfg)
        .unwrap()
        .is_none());
}

#[test]
fn provides_reflects_the_mapped_set() {
    let d = tmp("provides");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('before_prompt_build', ()=>{}); api.on('before_tool_call', ()=>{}); }",
    );
    let p = plugin_from(&d).unwrap();
    assert!(p.provides(Hook::PreLlmCall));
    // `before_tool_call` now has a real fire site, so a foreign extension
    // binding it genuinely provides the gate hook.
    assert!(p.provides(Hook::PreToolCall));
    assert!(!p.provides(Hook::PreGatewayDispatch));
    assert_eq!(p.dropped_events().len(), 0);
}

#[test]
fn omp_extension_runs_through_the_same_path() {
    let Some(cfg) = runtime() else { return };
    let d = tmp("omp-run");
    write(
        &d,
        "package.json",
        r#"{"name":"omp-ext","omp":{"hooks":"index.js"}}"#,
    );
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('before_prompt_build', () => ({ context: 'from-omp' })); }",
    );
    let kind = detect_kind(&d).unwrap();
    assert_eq!(kind, CompatKind::Omp);
    let p = plugin_from(&d).unwrap();
    assert_eq!(
        fire_hook(&p, Hook::PreLlmCall, &input(), &cfg)
            .unwrap()
            .as_deref(),
        Some("from-omp")
    );
}
