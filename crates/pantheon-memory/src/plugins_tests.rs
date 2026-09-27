//! Tests for `pantheon_memory::plugins::tests` — sibling file so sources stay test-free.
use super::*;
use crate::MemoryBackend;
use std::path::PathBuf;

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pantheon-memplug-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn manifest_validation_reports_the_missing_field() {
    let err = parse_manifest("name = \"x\"\nkind = \"http\"\n").unwrap_err();
    assert_eq!(err.code, "MEM_PLUGIN_MANIFEST");
    assert!(err.cause.contains("url"), "{}", err.cause);
    let err2 = parse_manifest("name = \"x\"\nkind = \"stdio\"\n").unwrap_err();
    assert!(err2.cause.contains("command"), "{}", err2.cause);
    let err3 = parse_manifest("name = \"x\"\nkind = \"carrier-pigeon\"\n").unwrap_err();
    assert!(err3.cause.contains("unknown kind"), "{}", err3.cause);
}

#[test]
fn load_dir_registers_user_plugins_and_skips_broken_ones() {
    let dir = tmp_dir("load");
    let plug = dir.join("memory-plugins");
    std::fs::create_dir_all(&plug).unwrap();
    std::fs::write(
            plug.join("mybrain.toml"),
            "name = \"ignored-name\"\nlabel = \"MyBrain\"\nkind = \"http\"\nurl = \"http://127.0.0.1:9000\"\nprefix = \"/memory\"\n",
        )
        .unwrap();
    std::fs::write(plug.join("broken.toml"), "name = \"\"\n").unwrap();
    let mut reg = BackendRegistry::with_defaults();
    let loaded = load_dir(&mut reg, &dir);
    // File stem wins; broken file skipped without killing the load.
    assert_eq!(loaded, vec!["mybrain".to_string()]);
    assert!(reg.contains("mybrain"));
    let info = reg.info("mybrain").unwrap();
    assert_eq!(info.label, "MyBrain");
    assert_eq!(info.kind, BackendKind::Http);
    let backend = reg
        .instantiate_selected(&BackendSelection {
            name: "mybrain".into(),
            options: Default::default(),
        })
        .unwrap();
    // Offline: construction succeeds, first call fails structured.
    let err = backend.list_agent("nyx").unwrap_err();
    assert_eq!(err.code, "MEM_HTTP_CONN");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn selection_options_override_manifest_url() {
    let dir = tmp_dir("override");
    let plug = dir.join("memory-plugins");
    std::fs::create_dir_all(&plug).unwrap();
    std::fs::write(
        plug.join("mybrain.toml"),
        "name = \"mybrain\"\nkind = \"http\"\nurl = \"http://127.0.0.1:9/old\"\n",
    )
    .unwrap();
    let mut reg = BackendRegistry::with_defaults();
    load_dir(&mut reg, &dir);
    let backend = reg
        .instantiate_selected(&BackendSelection {
            name: "mybrain".into(),
            options: [("url".to_string(), "http://127.0.0.1:9/new".to_string())]
                .into_iter()
                .collect(),
        })
        .unwrap();
    let err = backend.list_agent("nyx").unwrap_err();
    assert!(err.cause.contains("/new"), "{}", err.cause);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Write a tiny shell responder: reads one line, prints one JSON line.
fn responder(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("responder.sh");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh
read -r line
{body}
"
        ),
    )
    .unwrap();
    p
}

fn ok_script(dir: &Path, json: &str) -> StdioBackend {
    let p = responder(dir, &format!("printf '%s\\n' '{json}'"));
    StdioBackend::new("sh".into(), vec![p.to_string_lossy().to_string()], 2000)
}

/// A stdio plugin answers a recall request; proves the whole path
/// (spawn -> JSON in -> JSON out -> typed hits).
#[test]
fn stdio_backend_recall_round_trips() {
    let dir = tmp_dir("recall");
    let backend = ok_script(&dir, r#"{"ok":true,"hits":[]}"#);
    let policy = pantheon_api::capability::Policy::coder_with_memory();
    let hits = backend
        .recall(&policy, &["agent:nyx"], &[LayerKind::Agent], "anything", 5)
        .unwrap();
    assert!(hits.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A plugin that reports failure maps to its own structured code.
#[test]
fn stdio_backend_error_response_maps_to_code() {
    let dir = tmp_dir("err");
    let backend = ok_script(
        &dir,
        r#"{"ok":false,"code":"MYBRAIN_DOWN","cause":"index missing"}"#,
    );
    let policy = pantheon_api::capability::Policy::coder_with_memory();
    let err = backend
        .recall(&policy, &["nyx"], &[LayerKind::Agent], "q", 5)
        .unwrap_err();
    assert_eq!(err.code, "MYBRAIN_DOWN");
    assert!(err.cause.contains("index missing"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hung plugin is killed, never allowed to wedge the runtime.
#[test]
fn stdio_backend_timeout_kills_the_child() {
    let backend = StdioBackend::new("sleep".into(), vec!["5".into()], 200);
    let policy = pantheon_api::capability::Policy::coder_with_memory();
    let err = backend
        .recall(&policy, &["nyx"], &[LayerKind::Agent], "q", 5)
        .unwrap_err();
    assert_eq!(err.code, "MEM_PLUGIN_TIMEOUT");
}

/// A manifest-declared stdio plugin is selectable by name and its
/// command/args come from the manifest.
#[test]
fn stdio_manifest_plugin_is_selectable() {
    let dir = tmp_dir("stdio");
    let plug = dir.join("memory-plugins");
    std::fs::create_dir_all(&plug).unwrap();
    let script = responder(&dir, r#"printf '%s\n' '{"ok":true,"rows":[["k","v"]]}'"#);
    let manifest = format!(
            "name = \"shplug\"\nlabel = \"shell plugin\"\nkind = \"stdio\"\ncommand = \"sh\"\nargs = [\"{}\"]\ntimeout_ms = 2000\n",
            script.to_string_lossy()
        );
    std::fs::write(plug.join("shplug.toml"), manifest).unwrap();
    let mut reg = BackendRegistry::with_defaults();
    let loaded = load_dir(&mut reg, &dir);
    assert_eq!(loaded, vec!["shplug".to_string()]);
    assert_eq!(
        reg.info("shplug").unwrap().kind,
        crate::backend::BackendKind::Subprocess
    );
    let backend = reg
        .instantiate_selected(&BackendSelection {
            name: "shplug".into(),
            options: Default::default(),
        })
        .unwrap();
    let rows = backend.list_agent("nyx").unwrap();
    assert_eq!(rows, vec![("k".to_string(), "v".to_string())]);
    let _ = std::fs::remove_dir_all(&dir);
}
