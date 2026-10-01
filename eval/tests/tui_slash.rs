//! Behavioral tests for the slash-command batch: `/title` (renamed from
//! `/name`, kept as a silent alias), `/reset`, `/mcp reload`,
//! `/tools reload`, dynamic `/<skill>` dispatch, and `/learn`.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_api::capability::{Capability, Policy};
use pantheon_exec::skills::{Skill, SkillMeta};
use pantheon_memory::{LayerKind, MemoryStore};
use pantheon_tui::commands;
use pantheon_tui::mcp;
use pantheon_tui::session::{
    find_skill_by_name, lesson_proposal, slugify_lesson_key, BlockKind, TranscriptBlock, TuiState,
};
use std::path::PathBuf;

// ---------------------------------------------------------------- /title ---

#[test]
fn title_replaces_name_in_the_command_registry() {
    let reg = commands::registry();
    assert!(reg.contains_key("title"), "title must be registered");
    assert!(
        !reg.contains_key("name"),
        "/name is a silent alias, not a listed command"
    );
    assert_eq!(reg["title"].desc, "show or rename this conversation");
    let completions = commands::complete("/tit");
    assert!(
        completions.contains(&"/title".to_string()),
        "palette suggests /title: {completions:?}"
    );
    // The alias still dispatches (handled in session.rs), it just isn't
    // advertised.
    assert!(commands::is_builtin("title"));
}

// ---------------------------------------------------------------- /reset ---

/// A minimal runtime session for APIs that now take one (the MCP reload
/// pushes resolved specs into the session's manager).
fn test_runtime_session(dir: &std::path::Path) -> pantheon_runtime::session::Session {
    let model_policy = pantheon_api::model::ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: pantheon_api::model::DefaultModel {
            provider: "test".into(),
            model: "test".into(),
        },
        fallbacks: pantheon_api::model::FallbackChain { fallbacks: vec![] },
        auxiliaries: Vec::new(),
    };
    let secrets = pantheon_secrets::SecretsBroker::new();
    pantheon_runtime::session::Session::new(
        dir.to_path_buf(),
        Policy::coder(),
        model_policy,
        secrets,
    )
    .expect("test session")
}

#[test]
fn reset_ephemeral_clears_turn_state_but_keeps_identity() {
    let mut state = TuiState::default();
    state.session_id = "sess-1".to_string();
    state.title = Some("my chat".to_string());
    state.queued_message = Some("queued".to_string());
    state.ready = false;
    state.active_run = Some("run-9".to_string());
    state.status_line = "working".to_string();
    state.turn_started_at = Some(std::time::Instant::now());
    state.turn_in = Some(10);
    state.turn_out = Some(20);
    state.blocks.push(TranscriptBlock {
        kind: BlockKind::Status("x".into()),
    });

    assert!(state.reset_ephemeral(), "a turn was in flight");

    assert!(state.blocks.is_empty(), "transcript cleared");
    assert!(state.queued_message.is_none(), "queue dropped");
    assert!(state.ready, "ready for the next turn");
    assert_eq!(state.status_line, "ready");
    assert!(state.active_run.is_none());
    assert!(state.turn_started_at.is_none());
    assert!(state.turn_in.is_none());
    assert!(state.turn_out.is_none());
    // Identity survives the reset.
    assert_eq!(state.session_id, "sess-1");
    assert_eq!(state.title.as_deref(), Some("my chat"));
}

#[test]
fn reset_while_idle_reports_no_turn_running() {
    let mut state = TuiState::default();
    state.ready = true;
    // booted and waiting for input: genuinely idle
    assert!(
        !state.reset_ephemeral(),
        "nothing to cancel when idle, but the clear still applies"
    );
    assert!(state.ready);
}

// ------------------------------------------------------------ /mcp reload ---

fn mcp_server(
    name: &str,
    transport: &str,
    command: Option<&str>,
    url: Option<&str>,
    needs_credentials: bool,
) -> pantheon_migration::McpServer {
    pantheon_migration::McpServer {
        name: name.into(),
        transport: transport.into(),
        command: command.map(str::to_string),
        args: Vec::new(),
        url: url.map(str::to_string),
        requires_env: Vec::new(),
        needs_credentials,
        enabled: true,
    }
}

#[test]
fn server_readiness_matches_the_old_listing_rules() {
    assert!(mcp::server_readiness(&mcp_server(
        "a",
        "stdio",
        Some("/usr/bin/tool"),
        None,
        false
    ))
    .is_none());
    assert_eq!(
        mcp::server_readiness(&mcp_server("b", "stdio", None, None, false)),
        Some("no command declared".to_string())
    );
    assert_eq!(
        mcp::server_readiness(&mcp_server("c", "http", None, None, false)),
        Some("no url declared".to_string())
    );
    assert!(
        mcp::server_readiness(&mcp_server("d", "sse", None, Some("https://x/hook"), false))
            .is_none()
    );
    assert!(mcp::server_readiness(&mcp_server("e", "stdio", Some("/bin/t"), None, true)).is_some());
    assert_eq!(
        mcp::server_readiness(&mcp_server("f", "websocket", None, None, false)),
        Some("unsupported transport \"websocket\"".to_string())
    );
}

#[test]
fn mcp_reload_re_scans_declarations_and_reports_per_server() {
    let dir = tempfile::tempdir().unwrap();
    let mcp_dir = dir.path().join("mcp");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    std::fs::write(
        mcp_dir.join("hermes.json"),
        serde_json::json!({
            "source": "hermes",
            "servers": [
                {"name": "good", "transport": "stdio", "command": "/usr/bin/tool"},
                {"name": "broken", "transport": "stdio"},
                {"name": "web", "transport": "http", "url": "https://x/hook",
                 "needs_credentials": true, "requires_env": ["API_KEY"]},
            ]
        })
        .to_string(),
    )
    .unwrap();

    let session = test_runtime_session(dir.path());
    let lines = mcp::reload_report(&session, dir.path());
    let text = lines.join("\n");
    // The reload pushes declaration servers into the live manager; the
    // broken one never becomes a spec, and nothing is approved yet so
    // both live servers report as needing approval.
    assert!(
        text.contains("mcp: re-scanned config + declarations"),
        "{text}"
    );
    assert!(text.contains("good [needs approval]"), "{text}");
    assert!(text.contains("web [needs approval]"), "{text}");
    assert!(
        text.contains("2 server(s) need `pantheon mcp approve <name>` before they launch"),
        "{text}"
    );
    assert!(!text.contains("broken"), "{text}");
}

#[test]
fn mcp_reload_with_no_declarations_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let session = test_runtime_session(dir.path());
    let lines = mcp::reload_report(&session, dir.path());
    let text = lines.join("\n");
    assert!(
        text.contains("mcp: re-scanned config + declarations"),
        "{text}"
    );
    assert!(text.contains("no MCP servers configured"), "{text}");
}

// ---------------------------------------------------------- /tools reload ---

fn tools_test_session(dir: &std::path::Path) -> pantheon_runtime::session::Session {
    let policy = Policy::coder();
    let model_policy = pantheon_api::model::ModelPolicy {
        default: pantheon_api::model::DefaultModel {
            provider: "test".into(),
            model: "test".into(),
        },
        fallbacks: pantheon_api::model::FallbackChain {
            fallbacks: Vec::new(),
        },
        auxiliaries: Vec::new(),
        reasoning: pantheon_api::model::ReasoningLevel::default(),
        reasoning_budget: None,
    };
    let secrets = pantheon_secrets::SecretsBroker::new();
    pantheon_runtime::session::Session::new(dir.to_path_buf(), policy, model_policy, secrets)
        .unwrap()
}

#[test]
fn tools_reload_counts_agree_with_the_registry_itself() {
    let dir = tempfile::tempdir().unwrap();
    let session = tools_test_session(dir.path());
    // The shared constructor: the same registry the next turn will use.
    let (reg, counts) = session.build_tool_registry();
    assert_eq!(
        counts.total(),
        reg.names().len(),
        "counts are registry-size deltas, they cannot disagree"
    );
    assert!(counts.builtin > 0, "built-ins always register");
    assert!(counts.total() > 0);
    // MCP contributes nothing: declarations exist, but there is no
    // launcher attaching them yet.
    assert!(!reg.names().iter().any(|n| n.starts_with("mcp_")));
}

// -------------------------------------------------------------- /<skill> ---

fn skill(name: &str) -> Skill {
    Skill {
        meta: SkillMeta {
            name: name.into(),
            description: "test skill".into(),
            origin: "user".into(),
            exec: Vec::new(),
        },
        path: PathBuf::from("/tmp/test-skill.md"),
        dir: PathBuf::from("/tmp"),
    }
}

#[test]
fn skill_dispatch_prefers_builtin_commands() {
    assert!(commands::is_builtin("title"));
    assert!(commands::is_builtin("remember"));
    assert!(commands::is_builtin("tools"));
    assert!(commands::is_builtin("mcp"));
    assert!(!commands::is_builtin("pdf-summarizer"));
    assert!(!commands::is_builtin(""));
}

#[test]
fn skill_lookup_is_case_insensitive_and_misses_cleanly() {
    let skills = vec![skill("pdf-summarizer"), skill("Title")];
    assert_eq!(
        find_skill_by_name(&skills, "PDF-SUMMARIZER")
            .unwrap()
            .meta
            .name,
        "pdf-summarizer"
    );
    assert!(find_skill_by_name(&skills, "nope").is_none());
    // A skill named like a builtin still matches by name here: the
    // builtin-wins rule lives in the dispatch order (is_builtin is
    // consulted first), not in the lookup.
    assert_eq!(
        find_skill_by_name(&skills, "title").unwrap().meta.name,
        "Title"
    );
}

// ---------------------------------------------------------------- /learn ---

#[test]
fn slugify_lesson_key_is_stable_bounded_and_prefixed() {
    assert_eq!(
        slugify_lesson_key("when editing Rust, run cargo fmt before finishing"),
        "lesson:when-editing-rust-run-cargo-fmt"
    );
    assert_eq!(slugify_lesson_key("!!!"), "lesson:untitled");
    let long = slugify_lesson_key(&"word ".repeat(100));
    assert!(long.starts_with("lesson:"));
    assert!(long.len() <= "lesson:".len() + 48);
}

#[test]
fn learn_proposal_marks_lessons_and_stays_in_agent_memory() {
    let p = lesson_proposal("agent:nyx", "prefer ripgrep over grep", 123);
    assert!(p.key.starts_with("lesson:"), "the lesson marker: {}", p.key);
    assert_eq!(p.namespace, "agent:nyx");
    assert_eq!(p.layer, LayerKind::Agent);
    assert_eq!(p.value, "prefer ripgrep over grep");
    assert_eq!(p.provenance.origin, "user");
}

#[test]
fn learn_persists_and_is_recallable() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = Policy::coder().allow(Capability::MemoryWrite);
    let proposal = lesson_proposal(
        "agent:nyx",
        "when editing Rust, run cargo fmt before finishing",
        1,
    );
    let key = proposal.key.clone();
    pantheon_memory::write_via(&store, &policy, proposal, 4096).unwrap();

    let hits = pantheon_memory::recall(
        &store,
        &policy,
        &["agent:nyx"],
        &[LayerKind::Agent],
        "cargo fmt",
        5,
    )
    .unwrap();
    assert!(
        hits.iter()
            .any(|h| h.record.key == key && h.record.value.contains("cargo fmt")),
        "the lesson must come back on recall"
    );
}
