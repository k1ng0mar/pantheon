//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_extensions::hooks::Hook;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-jsrun-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(d: &std::path::Path, rel: &str, body: &str) {
    let p = d.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Build a JsPlugin from a directory, using the real detector.
fn plugin_from(dir: &std::path::Path) -> Option<pantheon_extensions::js_runner::JsPlugin> {
    use pantheon_extensions::compat::{detect_kind, entry_file, inspect, read_manifest};
    let kind = detect_kind(dir)?;
    let (name, _, _) = read_manifest(dir, kind)?;
    let entry = entry_file(dir, kind)?;
    let r = inspect(dir)?;
    Some(pantheon_extensions::js_runner::JsPlugin::load(
        dir,
        entry,
        name,
        r.mapped,
        r.unsupported,
    ))
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
