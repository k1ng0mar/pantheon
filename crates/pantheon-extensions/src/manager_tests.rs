//! Tests for `pantheon_extensions::manager::tests` — sibling file so sources stay test-free.
use super::*;
use std::io::Write;

fn plug(dir: &std::path::Path, name: &str, once: bool, ctx_text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let man = format!(
        "name: {name}\nprovides_hooks:\n  - pre_llm_call\n{}\n",
        if once { "once_per_session: true" } else { "" }
    );
    std::fs::write(dir.join("plugin.yaml"), man).unwrap();
    let init = format!(
        "def register(ctx):\n    ctx.register_hook('pre_llm_call', _h)\n\
             def _h(**kw):\n    return {{'context': '{}'}} \n",
        ctx_text
    );
    let mut f = std::fs::File::create(dir.join("__init__.py")).unwrap();
    f.write_all(init.as_bytes()).unwrap();
}

#[test]
fn once_per_session_dedups() {
    let base = std::env::temp_dir().join(format!("pantheon-mgr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    plug(&base.join("p"), "p", true, "CTX");
    let mut m = ExtensionManager::new(RunnerConfig::default());
    m.load_dir(&base).unwrap();
    assert_eq!(
        m.fire(Hook::PreLlmCall, "s1", "cli", Default::default()),
        Some("CTX".into())
    );
    assert_eq!(
        m.fire(Hook::PreLlmCall, "s1", "cli", Default::default()),
        None
    );
    assert_eq!(
        m.fire(Hook::PreLlmCall, "s2", "cli", Default::default()),
        Some("CTX".into())
    );
}

#[test]
fn seen_sessions_sniff_dedups_without_manifest_flag() {
    let base = std::env::temp_dir().join(format!("pantheon-mgr2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let d = base.join("q");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("plugin.yaml"),
        "name: q\nprovides_hooks:\n  - pre_llm_call\n",
    )
    .unwrap();
    std::fs::write(
            d.join("__init__.py"),
            "_seen_sessions = set()\ndef register(ctx):\n    ctx.register_hook('pre_llm_call', _h)\ndef _h(**kw):\n    return {'context': 'Q'}\n",
        )
        .unwrap();
    let mut m = ExtensionManager::new(RunnerConfig::default());
    m.load_dir(&base).unwrap();
    assert_eq!(
        m.fire(Hook::PreLlmCall, "s1", "cli", Default::default()),
        Some("Q".into())
    );
    assert_eq!(
        m.fire(Hook::PreLlmCall, "s1", "cli", Default::default()),
        None
    );
}

#[test]
fn normal_plugin_fires_every_time() {
    let base = std::env::temp_dir().join(format!("pantheon-mgr3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    plug(&base.join("r"), "r", false, "R");
    let mut m = ExtensionManager::new(RunnerConfig::default());
    m.load_dir(&base).unwrap();
    assert_eq!(
        m.fire(Hook::PreLlmCall, "s1", "cli", Default::default()),
        Some("R".into())
    );
    assert_eq!(
        m.fire(Hook::PreLlmCall, "s1", "cli", Default::default()),
        Some("R".into())
    );
}

/// Write a plugin that binds one hook and returns a fixed dict body.
fn plug_hook(dir: &std::path::Path, name: &str, hook: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("plugin.yaml"),
        format!("name: {name}\nprovides_hooks:\n  - {hook}\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("__init__.py"),
        format!(
            "def register(ctx):\n    ctx.register_hook('{hook}', _h)\ndef _h(**kw):\n    return {body}\n"
        ),
    )
    .unwrap();
}

fn mgr_with(base: &std::path::Path) -> ExtensionManager {
    let mut m = ExtensionManager::new(RunnerConfig::default());
    m.load_dir(base).unwrap();
    m
}

#[test]
fn gate_allows_when_no_plugin_binds_it() {
    let base = std::env::temp_dir().join("pantheon-gate-empty");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let m = mgr_with(&base);
    assert_eq!(
        m.fire_gate(Hook::PreToolCall, "s", "cli", Default::default()),
        GateDecision::Allow
    );
}

#[test]
fn gate_allows_when_plugin_says_nothing() {
    let base = std::env::temp_dir().join("pantheon-gate-allow");
    let _ = std::fs::remove_dir_all(&base);
    plug_hook(&base.join("g"), "g", "pre_tool_call", "{}");
    let m = mgr_with(&base);
    assert_eq!(
        m.fire_gate(Hook::PreToolCall, "s", "cli", Default::default()),
        GateDecision::Allow
    );
}

#[test]
fn gate_denies_with_the_plugins_reason() {
    let base = std::env::temp_dir().join("pantheon-gate-deny");
    let _ = std::fs::remove_dir_all(&base);
    plug_hook(
        &base.join("sec"),
        "sec",
        "pre_tool_call",
        "{'deny': True, 'reason': 'secrets in args'}",
    );
    let m = mgr_with(&base);
    match m.fire_gate(Hook::PreToolCall, "s", "cli", Default::default()) {
        GateDecision::Deny { reason, plugin } => {
            assert_eq!(reason, "secrets in args");
            assert_eq!(plugin, "sec");
        }
        GateDecision::Allow => panic!("gate allowed a denied call"),
    }
}

#[test]
fn gate_fails_closed_when_the_plugin_raises() {
    let base = std::env::temp_dir().join("pantheon-gate-crash");
    let _ = std::fs::remove_dir_all(&base);
    // A plugin that raises on import never answers; a security gate must not
    // read that as consent.
    plug_hook(
        &base.join("boom"),
        "boom",
        "pre_tool_call",
        "(_ for _ in ()).throw(RuntimeError('x'))",
    );
    let m = mgr_with(&base);
    match m.fire_gate(Hook::PreToolCall, "s", "cli", Default::default()) {
        GateDecision::Deny { plugin, .. } => assert_eq!(plugin, "boom"),
        GateDecision::Allow => panic!("gate failed OPEN on a crashing plugin"),
    }
}

#[test]
fn transform_replaces_output() {
    let base = std::env::temp_dir().join("pantheon-xform");
    let _ = std::fs::remove_dir_all(&base);
    plug_hook(
        &base.join("redact"),
        "redact",
        "transform_tool_result",
        "{'replacement': 'REDACTED'}",
    );
    let m = mgr_with(&base);
    let out = m.fire_transform(
        Hook::TransformToolResult,
        "s",
        "cli",
        [("tool".to_string(), "read".to_string())]
            .into_iter()
            .collect(),
        "sk-abc123",
    );
    assert_eq!(out, "REDACTED");
}

#[test]
fn transform_fails_open_and_passes_the_payload() {
    let base = std::env::temp_dir().join("pantheon-xform-open");
    let _ = std::fs::remove_dir_all(&base);
    plug_hook(
        &base.join("noop"),
        "noop",
        "transform_tool_result",
        "(_ for _ in ()).throw(RuntimeError('x'))",
    );
    let m = mgr_with(&base);
    // Redaction must degrade to no-op, never to an outage or a blank result.
    let out = m.fire_transform(
        Hook::TransformToolResult,
        "s",
        "cli",
        Default::default(),
        "original",
    );
    assert_eq!(out, "original");
}

#[test]
fn transform_sees_the_original_result() {
    let base = std::env::temp_dir().join("pantheon-xform-sees");
    let _ = std::fs::remove_dir_all(&base);
    // Echo back what the plugin received, proving `result` arrives as a flat
    // kwarg (Hermes plugins read flat kwargs, not a nested `extra`).
    plug_hook(
        &base.join("echo"),
        "echo",
        "transform_tool_result",
        "{'replacement': 'saw:' + str(kw.get('result'))}",
    );
    let m = mgr_with(&base);
    let out = m.fire_transform(
        Hook::TransformToolResult,
        "s",
        "cli",
        Default::default(),
        "payload-here",
    );
    assert_eq!(out, "saw:payload-here");
}

#[test]
fn notify_refuses_a_gate_hook() {
    let base = std::env::temp_dir().join("pantheon-notify-gate");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let m = mgr_with(&base);
    // Must not panic and must not pretend to gate: nothing to assert beyond
    // it returning, since a gate fired as an observer would lose its power.
    m.notify(Hook::PreToolCall, "s", "cli", Default::default());
}
