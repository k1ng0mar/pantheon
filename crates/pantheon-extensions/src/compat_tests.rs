//! Tests for the section 8 compat adapter: manifest reading, hook mapping,
//! registration scanning, and plugin.yaml generation.
use super::*;

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
fn generated_yaml_lists_only_mapped_hooks() {
    let y = render_plugin_yaml("lark", "1.0.0", "Lark bridge", &[Hook::PreLlmCall]);
    assert!(y.contains("name: lark"));
    assert!(y.contains("  - pre_llm_call"));
    // A generated manifest must never claim a hook the adapter cannot run.
    assert!(!y.contains("before_tool_call"));
    assert!(!y.contains("provides_hooks:\n  - pre_tool_call"));
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
