//! Tests for `pantheon_exec::context::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_api::message::ToolCallRef;

fn budget(limit: u32) -> WindowBudget {
    WindowBudget::new(limit, 0)
}

fn big_tool(id: &str, kb: usize) -> Message {
    Message::tool(id, "x".repeat(kb * 1024))
}

#[test]
fn estimate_is_bytes_over_four_rounded_up() {
    assert_eq!(estimate_tokens(""), 0);
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("abcde"), 2);
    // Rows carry overhead so an empty transcript is not "free".
    assert!(row_tokens(&Message::user("hi")) > estimate_tokens("hi"));
}

#[test]
fn under_budget_is_a_noop() {
    let msgs = vec![
        Message::system("sys"),
        Message::user("hello"),
        Message::assistant("hi"),
    ];
    let (out, report) = fit_to_window(msgs.clone(), &budget(10_000)).unwrap();
    assert_eq!(out, msgs);
    assert!(!report.changed());
    assert_eq!(report.dropped_rows, 0);
}

#[test]
fn oversized_tool_rows_are_compacted_before_anything_is_dropped() {
    let msgs = vec![
        Message::system("sys"),
        Message::user("go"),
        Message::assistant_tool_calls(vec![ToolCallRef {
            id: "c1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }]),
        big_tool("c1", 50),
        Message::user("next"),
        Message::assistant("ok"),
    ];
    let (out, report) = fit_to_window(msgs, &budget(4_000)).unwrap();
    // Compaction alone sufficed: nothing was dropped, pairing intact.
    assert_eq!(report.compacted_rows, 1);
    assert_eq!(report.dropped_rows, 0);
    let tool_row = out.iter().find(|m| m.role == Role::Tool).unwrap();
    // `compact_output` may append a ~20-byte byte-cap marker past max_bytes.
    assert!(tool_row.content.len() <= TOOL_FLOOR_BYTES + 32);
    assert!(out.iter().any(|m| m.role == Role::User));
    assert!(report.estimated <= report.window);
}

#[test]
fn oldest_exchange_drops_as_a_whole_group_system_and_tail_survive() {
    let msgs = vec![
        Message::system("sys"),
        Message::user("first task ".repeat(400)),
        Message::assistant("thinking ".repeat(400)),
        Message::user("second task ".repeat(400)),
        Message::assistant("more ".repeat(400)),
        Message::user("latest"),
    ];
    let (out, report) = fit_to_window(msgs, &budget(1_200)).unwrap();
    assert!(report.dropped_rows >= 1);
    // System preamble and the live tail survive.
    assert_eq!(out.first().unwrap().role, Role::System);
    assert_eq!(out.last().unwrap().content, "latest");
    // The dropped exchange is gone wholesale: no orphaned assistant
    // "thinking" row left without its user row.
    assert!(!out.iter().any(|m| m.content.contains("thinking")));
    // Grouping is preserved: every exchange still starts with a user row.
    let first_non_system = out.iter().find(|m| m.role != Role::System).unwrap();
    assert_eq!(first_non_system.role, Role::User);
}

#[test]
fn pairing_survives_dropping() {
    let msgs = vec![
        Message::system("sys"),
        Message::user("task one ".repeat(500)),
        Message::assistant_tool_calls(vec![ToolCallRef {
            id: "c1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }]),
        Message::tool("c1", "result ".repeat(500)),
        Message::user("task two ".repeat(500)),
        Message::assistant_tool_calls(vec![ToolCallRef {
            id: "c2".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }]),
        Message::tool("c2", "fresh"),
    ];
    let (out, report) = fit_to_window(msgs, &budget(1_500)).unwrap();
    assert!(
        report.dropped_rows >= 3,
        "first exchange (user + assistant + tool) should drop: {report:?}"
    );
    // If c1's assistant row is gone, its tool row must be gone too.
    let has_assistant_c1 = out
        .iter()
        .any(|m| m.role == Role::Assistant && m.tool_calls.iter().any(|c| c.id == "c1"));
    let has_tool_c1 = out
        .iter()
        .any(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("c1"));
    assert_eq!(has_assistant_c1, has_tool_c1);
    // The live pairing always survives.
    assert!(out
        .iter()
        .any(|m| { m.role == Role::Tool && m.tool_call_id.as_deref() == Some("c2") }));
}

#[test]
fn essential_rows_over_window_is_a_structured_error() {
    let msgs = vec![
        Message::system("s".repeat(4_000)),
        Message::user("only exchange ".repeat(1_000)),
    ];
    let err = fit_to_window(msgs, &budget(500)).unwrap_err();
    assert_eq!(err.code, "CONTEXT_OVERFLOW");
    assert!(!err.retryable);
    assert!(!err.remediation.is_empty());
}

#[test]
fn budget_reserves_output_and_pads() {
    let b = WindowBudget::new(100_000, 4_096);
    let usable = b.usable();
    assert!(usable < 100_000 - 4_096);
    assert!(usable > 80_000);
    // Unknown huge reserve must not underflow.
    let b2 = WindowBudget::new(1_000, 10_000);
    assert_eq!(b2.usable(), 0);
}

// --- compression (live compressor: real model, no fakes) ---

/// Working compressor: the real client against the local llm-router.
/// None = no PANTHEON_KEY_ROUTER exported, skip the live tests.
fn live_compressor() -> Option<pantheon_providers::CompressionClient> {
    let key = std::env::var("PANTHEON_KEY_ROUTER")
        .ok()
        .filter(|k| !k.trim().is_empty())?;
    let _ = key;
    Some(pantheon_providers::CompressionClient::new(
        pantheon_api::model::DefaultModel {
            provider: "router".into(),
            model: "chat".into(),
        },
        None,
    ))
}

/// Broken compressor: a real client aimed at a dead port. Refused dials
/// fail deterministically — the live stand-in for a down summarizer.
fn dead_compressor() -> pantheon_providers::CompressionClient {
    pantheon_providers::catalog::register_custom_provider(
        pantheon_providers::catalog::ProviderMeta {
            id: "livecompress-dead".into(),
            label: "livecompress-dead".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_mode: pantheon_providers::catalog::ApiMode::OpenAi,
            base_env: String::new(),
            key_env: "PANTHEON_KEY_LIVECOMPRESS_DEAD".into(),
            key_header: "Authorization".into(),
            models: vec![],
            prominent: false,
            dev: false,
            tag: "live-test".into(),
        },
    );
    pantheon_providers::CompressionClient::new(
        pantheon_api::model::DefaultModel {
            provider: "livecompress-dead".into(),
            model: "chat".into(),
        },
        None,
    )
}

fn overflowing_transcript() -> Vec<Message> {
    vec![
        Message::system("sys"),
        Message::user("task ".repeat(1_000)),
        Message::assistant("work ".repeat(1_000)),
        Message::user("task ".repeat(1_000)),
        Message::assistant("work ".repeat(1_000)),
        Message::user("live"),
    ]
}

#[test]
fn compression_skips_when_under_budget_or_no_exchange() {
    // Under budget the compressor is never invoked: a dead-port client
    // would fail loudly if called, so green proves non-invocation.
    let dead = dead_compressor();
    let msgs = vec![Message::system("sys"), Message::user("hi")];
    assert!(compress_oldest(&msgs, &dead, &budget(100_000), "r")
        .unwrap()
        .is_none());
    // Only one exchange — nothing droppable, nothing to compress.
    assert!(compress_oldest(&msgs, &dead, &budget(10), "r")
        .unwrap()
        .is_none());
}

#[test]
fn compression_absorbs_oldest_exchanges_splicing_a_memory_note() {
    let Some(live) = live_compressor() else {
        eprintln!("SKIP compression_absorbs: no PANTHEON_KEY_ROUTER");
        return;
    };
    let msgs = overflowing_transcript();
    let before = estimate_messages(&msgs);
    let (out, report) = compress_oldest(&msgs, &live, &budget(1_200), "r1")
        .unwrap()
        .expect("overflow should compress");

    // Preamble and the live exchange survive; middle absorbed into one note.
    assert_eq!(out.first().unwrap().role, Role::System);
    assert_eq!(out.first().unwrap().content, "sys");
    assert_eq!(out.last().unwrap().content, "live");
    let note = &out[1];
    assert_eq!(note.role, Role::System);
    assert!(note.content.contains("<compressed_context>"));
    assert_eq!(
        note.provenance.as_ref().map(|p| p.trust),
        Some(pantheon_api::provenance::TrustTier::Memory)
    );

    assert_eq!(report.exchanges, 2);
    assert_eq!(report.rows, 4);
    assert!(report.chars_before > report.chars_after);
    // The result must be strictly smaller than what it replaced.
    assert!(estimate_messages(&out) < before);
}

#[test]
fn compressor_error_propagates_for_deterministic_fallback() {
    // Real refused dial: the error propagates and the input is untouched
    // for the caller's deterministic fit.
    let msgs = overflowing_transcript();
    let err = compress_oldest(&msgs, &dead_compressor(), &budget(1_200), "r1").unwrap_err();
    assert!(
        err.code == "COMPRESSION_HTTP" || err.code == "PROVIDER_HTTP",
        "transport failure, got {err:?}"
    );
    // Input was borrowed, not consumed: caller still has it for fit_to_window.
    assert_eq!(
        estimate_messages(&msgs),
        estimate_messages(&overflowing_transcript())
    );
}

#[test]
fn render_caps_each_row_and_the_total() {
    let long_row = Message::tool("c1", "z".repeat(5_000));
    let msgs = vec![long_row];
    let rendered = render_exchanges(&msgs, 0..1);
    assert!(rendered.contains("[...]"));
    assert!(rendered.len() < 5_000);

    let many: Vec<Message> = (0..80).map(|_| Message::user("y".repeat(1_900))).collect();
    let rendered = render_exchanges(&many, 0..80);
    assert!(rendered.contains("[... render cap ...]"));
    assert!(rendered.len() < RENDER_MAX_CHARS + 64);
}
