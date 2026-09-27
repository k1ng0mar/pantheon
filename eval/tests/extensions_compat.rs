//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_extensions::compat::{
    detect_kind, entry_file, inspect, read_manifest, render_plugin_yaml, scan_registrations,
    CompatKind,
};
use pantheon_extensions::hooks::Hook;
use pantheon_extensions::manifest::PluginManifest;
use std::fs;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-compat-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(d: &std::path::Path, rel: &str, body: &str) {
    let p = d.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

#[test]
fn detects_openclaw_by_its_manifest() {
    let d = tmp("oc-detect");
    write(&d, "openclaw.plugin.json", r#"{"id":"x"}"#);
    assert_eq!(detect_kind(&d), Some(CompatKind::OpenClaw));
}

#[test]
fn detects_omp_by_its_package_field() {
    let d = tmp("omp-detect");
    write(&d, "package.json", r#"{"name":"x","omp":{}}"#);
    assert_eq!(detect_kind(&d), Some(CompatKind::Omp));
    let d2 = tmp("pi-detect");
    write(&d2, "package.json", r#"{"name":"x","pi":{}}"#);
    assert_eq!(detect_kind(&d2), Some(CompatKind::Omp));
}

#[test]
fn a_plain_package_json_is_not_an_extension() {
    let d = tmp("not-ext");
    write(&d, "package.json", r#"{"name":"x","version":"1.0.0"}"#);
    assert_eq!(detect_kind(&d), None);
}

#[test]
fn reads_openclaw_credentials_as_names_only() {
    let d = tmp("oc-creds");
    write(
        &d,
        "openclaw.plugin.json",
        r#"{
          "id": "alibaba",
          "name": "Alibaba",
          "setup": { "providers": [ { "id": "alibaba",
            "envVars": ["MODELSTUDIO_API_KEY","DASHSCOPE_API_KEY"] } ] }
        }"#,
    );
    let (name, _desc, creds) = read_manifest(&d, CompatKind::OpenClaw).unwrap();
    assert_eq!(name, "Alibaba");
    assert_eq!(creds.len(), 2);
    assert_eq!(creds[0].env_var, "MODELSTUDIO_API_KEY");
    assert_eq!(creds[0].provider, "alibaba");
}

#[test]
fn openclaw_manifest_tolerates_missing_and_extra_fields() {
    let d = tmp("oc-loose");
    write(
        &d,
        "openclaw.plugin.json",
        r#"{"id":"only-id","totallyNewField":{"a":1}}"#,
    );
    let (name, _, creds) = read_manifest(&d, CompatKind::OpenClaw).unwrap();
    assert_eq!(name, "only-id");
    assert!(creds.is_empty());
}

#[test]
fn finds_the_omp_hooks_entry_file() {
    let d = tmp("omp-entry");
    write(
        &d,
        "package.json",
        r#"{"name":"x","omp":{"hooks":"lib/hooks.js"}}"#,
    );
    write(&d, "lib/hooks.js", "export function register(){}");
    let e = entry_file(&d, CompatKind::Omp).unwrap();
    assert!(e.ends_with("lib/hooks.js"));
}

#[test]
fn falls_back_to_index_when_omp_names_no_entry() {
    let d = tmp("omp-fallback");
    write(&d, "package.json", r#"{"name":"x","omp":{}}"#);
    write(&d, "index.js", "export function register(){}");
    let e = entry_file(&d, CompatKind::Omp).unwrap();
    assert!(e.ends_with("index.js"));
}

// ---------------------------------------------------------------------------
// inspect + report
// ---------------------------------------------------------------------------

#[test]
fn inspect_separates_mapped_from_unsupported() {
    let d = tmp("inspect");
    write(
        &d,
        "openclaw.plugin.json",
        r#"{"id":"lark","name":"Lark","setup":{"providers":[]}}"#,
    );
    write(
        &d,
        "index.js",
        r#"
        export function register(api) {
          api.on('before_tool_call', () => {});
          api.on('after_tool_call', () => {});
          api.registerProvider({});
        }
        "#,
    );
    let r = inspect(&d).unwrap();
    assert_eq!(r.name, "Lark");
    assert_eq!(r.origin, "openclaw");
    // The tool hooks now have real fire sites, so they MAP rather than being
    // reported lost. `message_sending` is the honest "no equivalent" case.
    assert!(r.mapped.contains(&"before_tool_call".to_string()));
    assert!(r.mapped.contains(&"after_tool_call".to_string()));
    assert!(r.refused.contains(&"registerProvider".to_string()));
    assert!(!r.clean(), "a partial load must not claim to be clean");
}

#[test]
fn a_fully_mappable_plugin_reports_clean() {
    let d = tmp("clean");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('before_prompt_build', ()=>{}); }",
    );
    let r = inspect(&d).unwrap();
    assert!(r.clean(), "{}", r.summary());
    assert_eq!(r.mapped, vec!["before_prompt_build"]);
}

#[test]
fn inspect_returns_none_for_a_non_extension() {
    let d = tmp("nope");
    write(&d, "readme.md", "hello");
    assert!(inspect(&d).is_none());
}

#[test]
fn report_summary_mentions_the_loss() {
    let d = tmp("summary");
    write(&d, "openclaw.plugin.json", r#"{"id":"p","name":"P"}"#);
    // `message_sending` has no Pantheon equivalent in any state, so it is the
    // stable "this plugin loses something" case.
    write(
        &d,
        "index.js",
        "export function register(api){ api.on('message_sending',()=>{}); }",
    );
    let s = inspect(&d).unwrap().summary();
    assert!(s.contains("1 unsupported"), "{s}");
}

// ---------------------------------------------------------------------------
// manifest generation
// ---------------------------------------------------------------------------

#[test]
fn generated_yaml_is_parseable_by_the_real_manifest_loader() {
    let d = tmp("gen-parse");
    let y = render_plugin_yaml("x", "1", "d", &[Hook::PreLlmCall, Hook::PreGatewayDispatch]);
    let p = d.join("plugin.yaml");
    fs::write(&p, &y).unwrap();
    let m = PluginManifest::load(&p).expect("generated manifest must load");
    assert_eq!(m.name, "x");
    let (known, unknown) = m.hook_list();
    assert_eq!(known.len(), 2);
    assert!(unknown.is_empty(), "{unknown:?}");
}

#[test]
fn generated_yaml_with_no_mapped_hooks_is_still_valid() {
    let d = tmp("gen-empty");
    let y = render_plugin_yaml("x", "1", "d", &[]);
    fs::write(d.join("plugin.yaml"), &y).unwrap();
    let m = PluginManifest::load(&d.join("plugin.yaml")).unwrap();
    assert!(m.hook_list().0.is_empty());
}
