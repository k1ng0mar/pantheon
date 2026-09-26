//! Tests for the section 8 compat adapter: manifest reading, hook mapping,
//! registration scanning, and plugin.yaml generation.
use super::*;
use crate::manifest::PluginManifest;
use std::fs;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-compat-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write(d: &Path, rel: &str, body: &str) {
    let p = d.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

// ---------------------------------------------------------------------------
// hook mapping
// ---------------------------------------------------------------------------

#[test]
fn direct_semantic_matches_map() {
    assert_eq!(
        map_hook("before_prompt_build"),
        HookMap::Mapped(Hook::PreLlmCall)
    );
    assert_eq!(
        map_hook("before_api_request"),
        HookMap::Mapped(Hook::PreApiRequest)
    );
    assert_eq!(
        map_hook("after_api_request"),
        HookMap::Mapped(Hook::PostApiRequest)
    );
    // The load-bearing tool hooks map for real now.
    assert_eq!(
        map_hook("before_tool_call"),
        HookMap::Mapped(Hook::PreToolCall)
    );
    assert_eq!(
        map_hook("after_tool_call"),
        HookMap::Mapped(Hook::PostToolCall)
    );
    assert_eq!(
        map_hook("transform_tool_result"),
        HookMap::Mapped(Hook::TransformToolResult)
    );
}

#[test]
fn unwired_hooks_are_never_reported_as_mapped() {
    // `pre_gateway_dispatch` is declared but has no fire site: inbound
    // messages are handled in a crate that cannot reach the manager. Mapping
    // it would promise the operator their handler runs, and it would not.
    assert!(!Hook::PreGatewayDispatch.is_wired());
    for ev in [
        "message_received",
        "pre_gateway_dispatch",
        "gateway_dispatch",
        "before_dispatch",
    ] {
        assert_eq!(
            map_hook(ev),
            HookMap::Unsupported { nearest: None },
            "{ev} must not claim a mapped hook"
        );
    }
}

#[test]
fn mapped_implies_wired() {
    // The invariant that makes `Mapped` trustworthy: it is a promise.
    let events = [
        "before_prompt_build",
        "before_api_request",
        "after_api_request",
        "before_tool_call",
        "after_tool_call",
        "transform_tool_result",
        "on_session_start",
        "on_session_end",
        "subagent_start",
        "subagent_stop",
        "on_stream_start",
        "on_stream_delta",
        "on_stream_end",
        "message_received",
        "pre_gateway_dispatch",
    ];
    for ev in events {
        if let HookMap::Mapped(h) = map_hook(ev) {
            assert!(h.is_wired(), "{ev} mapped to unwired {}", h.name());
        }
    }
}

#[test]
fn tool_and_lifecycle_hooks_are_wired() {
    for h in [
        Hook::PreToolCall,
        Hook::PostToolCall,
        Hook::TransformToolResult,
        Hook::OnSessionStart,
        Hook::OnSessionEnd,
        Hook::SubagentStart,
        Hook::SubagentStop,
        Hook::OnStreamStart,
        Hook::OnStreamEnd,
    ] {
        assert!(h.is_wired(), "{} should be wired", h.name());
    }
}

#[test]
fn mapping_is_case_and_separator_insensitive() {
    for spelling in [
        "before_prompt_build",
        "BEFORE_PROMPT_BUILD",
        "before-prompt-build",
        " before_prompt_build ",
    ] {
        assert_eq!(
            map_hook(spelling),
            HookMap::Mapped(Hook::PreLlmCall),
            "{spelling}"
        );
    }
}

#[test]
fn wired_tool_and_session_events_map_to_their_own_hooks() {
    assert_eq!(
        map_hook("before_tool_call"),
        HookMap::Mapped(Hook::PreToolCall)
    );
    assert_eq!(
        map_hook("after_tool_call"),
        HookMap::Mapped(Hook::PostToolCall)
    );
    assert_eq!(
        map_hook("transform_tool_result"),
        HookMap::Mapped(Hook::TransformToolResult)
    );
    assert_eq!(
        map_hook("on_session_start"),
        HookMap::Mapped(Hook::OnSessionStart)
    );
    assert_eq!(
        map_hook("on_session_end"),
        HookMap::Mapped(Hook::OnSessionEnd)
    );
    assert_eq!(
        map_hook("subagent_start"),
        HookMap::Mapped(Hook::SubagentStart)
    );
    assert_eq!(
        map_hook("subagent_stop"),
        HookMap::Mapped(Hook::SubagentStop)
    );
}

#[test]
fn every_mapping_lands_on_a_wired_hook() {
    // Whatever the table says, the promise must hold for every entry.
    for e in KNOWN_FOREIGN_EVENTS {
        if let HookMap::Mapped(h) = map_hook(e) {
            assert!(h.is_wired(), "{e} mapped to unwired {h:?}");
        }
    }
}

#[test]
fn unsupported_events_are_refused_not_approximated() {
    // No Panthey equivalent exists for outbound message handling.
    for e in ["message_sending", "before_send", "outbound_message"] {
        assert_eq!(map_hook(e), HookMap::Unsupported { nearest: None }, "{e}");
    }
}

#[test]
fn unknown_events_are_unsupported_not_guessed() {
    assert_eq!(
        map_hook("some_future_event"),
        HookMap::Unsupported { nearest: None }
    );
    assert_eq!(map_hook(""), HookMap::Unsupported { nearest: None });
}

#[test]
fn every_known_foreign_event_classifies() {
    for e in KNOWN_FOREIGN_EVENTS {
        let _ = map_hook(e);
    }
    let mapped: Vec<&str> = KNOWN_FOREIGN_EVENTS
        .iter()
        .copied()
        .filter(|e| matches!(map_hook(e), HookMap::Mapped(_)))
        .collect();
    assert!(mapped.contains(&"before_prompt_build"));
    assert!(mapped.contains(&"before_tool_call"));
    // And the unwired one is provably not in the mapped set.
    assert!(!mapped.contains(&"message_received"));
}

// ---------------------------------------------------------------------------
// registration scanning
// ---------------------------------------------------------------------------

#[test]
fn scans_api_on_events_from_real_shapes() {
    // Shaped like the actual openclaw-lark extension.
    let js = r#"
        api.on('before_tool_call', (event, ctx) => {});
        api.on('after_tool_call', (event, ctx) => {});
    "#;
    let (events, _) = scan_registrations(js);
    assert_eq!(events, vec!["before_tool_call", "after_tool_call"]);
}

#[test]
fn scans_double_quoted_events() {
    let (events, _) = scan_registrations(r#"api.on("before_prompt_build", async (e,c) => {})"#);
    assert_eq!(events, vec!["before_prompt_build"]);
}

#[test]
fn scans_refusable_api_methods() {
    let js = "api.registerProvider({}); api.registerTool({}); api.on('before_prompt_build', f);";
    let (events, methods) = scan_registrations(js);
    assert_eq!(events, vec!["before_prompt_build"]);
    assert!(methods.contains(&"registerProvider".to_string()));
    assert!(methods.contains(&"registerTool".to_string()));
    assert!(!methods.contains(&"registerCli".to_string()));
}

#[test]
fn scan_ignores_non_registration_api_uses() {
    let (events, methods) = scan_registrations("const x = api.runtime; api.logger.info('hi');");
    assert!(events.is_empty());
    assert!(methods.is_empty());
}

#[test]
fn scan_deduplicates_repeated_events() {
    let js = "api.on('message_received', f); api.on('message_received', g);";
    let (events, _) = scan_registrations(js);
    assert_eq!(events.len(), 1);
}

// ---------------------------------------------------------------------------
// manifest reading
// ---------------------------------------------------------------------------

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
fn generated_yaml_lists_only_mapped_hooks() {
    let y = render_plugin_yaml("lark", "1.0.0", "Lark bridge", &[Hook::PreLlmCall]);
    assert!(y.contains("name: lark"));
    assert!(y.contains("  - pre_llm_call"));
    // A generated manifest must never claim a hook the adapter cannot run.
    assert!(!y.contains("before_tool_call"));
    assert!(!y.contains("provides_hooks:\n  - pre_tool_call"));
}

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

#[test]
fn omp_compaction_pair_collapses_onto_on_compaction() {
    // OMP emits start/end; Pantheon fires once, after the fact. Both spellings
    // must map, and the target must be wired, or the promise is false.
    for e in ["auto_compaction_start", "auto_compaction_end"] {
        assert_eq!(map_hook(e), HookMap::Mapped(Hook::OnCompaction), "{e}");
    }
    assert!(Hook::OnCompaction.is_wired());
    assert_eq!(Hook::parse("on_compaction"), Some(Hook::OnCompaction));
}

#[test]
fn omp_retry_lifecycle_is_explicitly_unsupported() {
    // The fallback walk lives in the provider plane and never reaches the
    // core fan-out, so there is no fire site. These must be reasoned-about
    // Unsupported (in KNOWN_FOREIGN_EVENTS), never mapped, never guessed.
    for e in [
        "auto_retry_start",
        "auto_retry_end",
        "retry_fallback_applied",
        "retry_fallback_succeeded",
        "ttsr_triggered",
    ] {
        assert_eq!(map_hook(e), HookMap::Unsupported { nearest: None }, "{e}");
        assert!(KNOWN_FOREIGN_EVENTS.contains(&e), "{e} must be known");
    }
}
