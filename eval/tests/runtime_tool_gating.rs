//! Behavioral tests: every Tools-screen toggle gates real registration.
//! Run with `cargo test -p pantheon-eval`.
//!
//! The `[tools]` section is the single source of truth: absent = every
//! group on. These tests drive the real `Session::build_tool_registry`
//! and the real builtin registration, so a toggle that stopped gating
//! would fail here instead of silently shipping a dead switch.
use pantheon_api::capability::Policy;
use pantheon_api::config::{ToolGroup, ToolsSection};
use pantheon_runtime::session::Session;
use pantheon_runtime::tool_config::ToolEnablement;
use pantheon_tools::builtins::{register_builtins_with, BuiltinOptions};
use pantheon_tools::tools::ToolRegistry;

fn gating_session(tag: &str) -> Session {
    let dir = std::env::temp_dir().join(format!("pantheon-gating-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let model_policy = pantheon_api::model::ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: pantheon_api::model::DefaultModel {
            provider: "test".into(),
            model: "test".into(),
        },
        fallbacks: pantheon_api::model::FallbackChain {
            fallbacks: Vec::new(),
        },
        auxiliaries: Vec::new(),
    };
    Session::new(
        dir,
        Policy::coder(),
        model_policy,
        pantheon_secrets::SecretsBroker::new(),
    )
    .unwrap()
}

fn names(reg: &ToolRegistry) -> Vec<String> {
    reg.names()
}

#[test]
fn builtin_toggles_gate_registration() {
    // Terminal / Files / Ask User each gate their builtins: off means
    // the names never reach the registry.
    let mut reg = ToolRegistry::new();
    register_builtins_with(
        &mut reg,
        BuiltinOptions {
            enable_terminal: false,
            enable_files: false,
            enable_ask_user: false,
            ..Default::default()
        },
    );
    let n = names(&reg);
    for tool in ["shell", "read_file", "write_file", "list_dir", "ask_user"] {
        assert!(
            !n.iter().any(|t| t == tool),
            "{tool} registered while its group is off"
        );
    }

    let mut reg = ToolRegistry::new();
    register_builtins_with(&mut reg, BuiltinOptions::default());
    let n = names(&reg);
    for tool in ["shell", "read_file", "write_file", "list_dir", "ask_user"] {
        assert!(
            n.iter().any(|t| t == tool),
            "{tool} missing with its group on"
        );
    }
}

#[test]
fn all_groups_off_registry_has_no_gated_tools_but_keeps_session_search() {
    let s = gating_session("all-off");
    s.set_tool_enablement(ToolEnablement::none());
    let (reg, _counts) = s.build_tool_registry();
    let n = names(&reg);
    // Every gated group is absent: builtins, web search, vault,
    // plugins/MCP projections.
    for tool in [
        "shell",
        "read_file",
        "write_file",
        "list_dir",
        "ask_user",
        "web_search",
        "vault_archive",
        "vault_read",
        "vault_search",
        "vault_list",
    ] {
        assert!(
            !n.iter().any(|t| t == tool),
            "{tool} registered while its group is off"
        );
    }
    assert!(
        !n.iter().any(|t| t.starts_with("mcp_")),
        "mcp projections registered while Plugins is off: {n:?}"
    );
    // Session search is default-on and hidden from the Tools screen: no
    // toggle exists, so it is always present.
    assert!(
        n.iter().any(|t| t == "session_search"),
        "session_search must always be registered, got: {n:?}"
    );
    let _ = std::fs::remove_dir_all(s.supervisor.data_dir());
}

#[test]
fn session_search_has_no_tool_group() {
    // The Tools screen cannot list what the group enum does not know:
    // session search stays hidden by construction.
    assert!(ToolGroup::parse("session_search").is_none());
    assert_eq!(ToolGroup::all().len(), 16);
}

#[test]
fn enablement_resolves_from_tools_section() {
    // Absent flags stay on; an explicit false gates exactly its group.
    let section = ToolsSection {
        browser: Some(false),
        vault: Some(false),
        plugins: Some(false),
        ..Default::default()
    };
    let e = ToolEnablement::from_section(&section);
    assert!(!e.is_enabled(ToolGroup::Browser));
    assert!(!e.is_enabled(ToolGroup::Vault));
    assert!(!e.is_enabled(ToolGroup::Plugins));
    assert!(e.is_enabled(ToolGroup::Terminal));
    assert!(e.is_enabled(ToolGroup::Memory));

    // No section at all = every group on (pre-section behavior).
    let e = ToolEnablement::from_section(&ToolsSection::default());
    for g in ToolGroup::all() {
        assert!(e.is_enabled(g), "{g:?} off with no [tools] section");
    }
}

#[test]
fn vision_and_video_aux_entries_gate_on_their_groups() {
    use pantheon_api::model::{AuxiliaryKind, DefaultModel};
    use pantheon_tui::config::auxiliaries;

    let default = DefaultModel {
        provider: "test".into(),
        model: "test".into(),
    };
    // Vision off: no vision aux entry, video untouched.
    let mut cfg = pantheon_api::config::Config {
        tools: Some(ToolsSection {
            vision: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let auxes = auxiliaries(Some(&cfg), &default);
    assert!(!auxes.iter().any(|a| a.kind == AuxiliaryKind::Vision));
    assert!(auxes.iter().any(|a| a.kind == AuxiliaryKind::Video));

    // Video off: no video aux entry, vision untouched.
    cfg.tools = Some(ToolsSection {
        video_analysis: Some(false),
        ..Default::default()
    });
    let auxes = auxiliaries(Some(&cfg), &default);
    assert!(!auxes.iter().any(|a| a.kind == AuxiliaryKind::Video));
    assert!(auxes.iter().any(|a| a.kind == AuxiliaryKind::Vision));

    // Neither off: both present.
    cfg.tools = None;
    let auxes = auxiliaries(Some(&cfg), &default);
    assert!(auxes.iter().any(|a| a.kind == AuxiliaryKind::Vision));
    assert!(auxes.iter().any(|a| a.kind == AuxiliaryKind::Video));
}

#[test]
fn computer_use_toggle_off_registers_no_driver_tools() {
    let s = gating_session("computer-off");
    let e = ToolEnablement {
        computer_use: false,
        ..Default::default()
    };
    s.set_tool_enablement(e);
    let (reg, counts) = s.build_tool_registry();
    assert_eq!(counts.computer, 0);
    assert!(
        !names(&reg).iter().any(|t| t.starts_with("mcp_cua_driver_")),
        "driver tools registered while the ComputerUse group is off"
    );
}

#[test]
fn code_intel_toggle_on_registers_lsp_and_gitundo_tools() {
    let s = gating_session("codeintel-on");
    s.set_tool_enablement(ToolEnablement::default());
    let (reg, counts) = s.build_tool_registry();
    let n = names(&reg);
    for tool in [
        "lsp.open",
        "lsp.diagnostics",
        "lsp.shutdown",
        "gitundo.snapshot",
        "gitundo.list",
        "gitundo.restore",
        "gitundo.delete",
    ] {
        assert!(
            n.iter().any(|t| t == tool),
            "'{tool}' not registered with the CodeIntel group on"
        );
    }
    assert_eq!(counts.code_intel, 7, "expected 7 code-intel tools");
    assert!(
        counts.code_intel <= counts.total(),
        "code_intel must feed total()"
    );
}

#[test]
fn code_intel_toggle_off_registers_no_codeintel_tools() {
    let s = gating_session("codeintel-off");
    let e = ToolEnablement {
        code_intel: false,
        ..Default::default()
    };
    s.set_tool_enablement(e);
    let (reg, counts) = s.build_tool_registry();
    assert_eq!(counts.code_intel, 0);
    let n = names(&reg);
    assert!(
        !n.iter()
            .any(|t| t.starts_with("lsp.") || t.starts_with("gitundo.")),
        "code-intel tools registered while the CodeIntel group is off"
    );
}

#[test]
fn code_intel_group_parses_and_resolves_in_tools_section() {
    // The new group must round-trip through the config layer: a disabled
    // group writes `code_intel = false` and resolves back to off; an
    // absent flag stays on.
    let mut on = ToolsSection::default();
    assert!(on.is_enabled(ToolGroup::CodeIntel), "absent = on");
    on.code_intel = Some(false);
    assert!(!on.is_enabled(ToolGroup::CodeIntel), "explicit false");
    // Parse back from the key.
    assert_eq!(ToolGroup::parse("code_intel"), Some(ToolGroup::CodeIntel));
}

#[test]
fn computer_use_without_driver_binary_registers_nothing() {
    let s = gating_session("computer-no-driver");
    s.set_tool_enablement(ToolEnablement::default());
    // A configured binary that does not exist fails closed: no spec, no
    // tools, no panic - the toggle promises nothing it cannot honor.
    s.set_computer_config(pantheon_runtime::tool_config::ComputerToolConfig {
        enabled: true,
        driver: "cua-driver".into(),
        binary: Some("/nonexistent/cua-driver".into()),
    });
    let (reg, counts) = s.build_tool_registry();
    assert_eq!(counts.computer, 0);
    assert!(
        !names(&reg).iter().any(|t| t.starts_with("mcp_cua_driver_")),
        "driver tools registered with no driver binary present"
    );
}

#[test]
fn computer_use_unapproved_driver_registers_nothing() {
    let s = gating_session("computer-unapproved");
    s.set_tool_enablement(ToolEnablement::default());
    // A stand-in binary (any hashable file): the driver resolves, but the
    // MCP approval gate has no record for it, so registration must report
    // pending and project zero tools.
    let dir = std::env::temp_dir().join(format!(
        "pantheon-gating-computer-unapproved-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("cua-driver");
    std::fs::write(&fake, b"#!/bin/sh\nexit 0\n").unwrap();
    s.set_computer_config(pantheon_runtime::tool_config::ComputerToolConfig {
        enabled: true,
        driver: "cua-driver".into(),
        binary: Some(fake),
    });
    let (reg, counts) = s.build_tool_registry();
    assert_eq!(counts.computer, 0);
    assert!(
        !names(&reg).iter().any(|t| t.starts_with("mcp_cua_driver_")),
        "unapproved driver projected tools into the registry"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
