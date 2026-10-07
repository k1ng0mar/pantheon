//! Miscellaneous runtime behavior: computer-use driver spec, the nightly
//! tool-disable list, and tool config defaults. (Temporal hint behavior
//! lives in `eval/tests/temporal.rs`; skill_exec registration here too.)
//!
//! All tests use public APIs only and are fully deterministic.

use pantheon_exec::skills::{parse_skill, Skill, SKILL_EXEC_TOOL_NAME};
use pantheon_runtime::computer::{cua_driver_spec, CUA_DRIVER_ID, CUA_DRIVER_SERVER};
use pantheon_runtime::nightly_tools::{
    disabled_tools_path, load_disabled_tools, record_disabled_tool,
};
use pantheon_runtime::skill_exec_tool::register_skill_exec_tool;
use pantheon_runtime::tool_config::{BrowserToolConfig, WebsearchToolConfig};

use pantheon_api::capability::Capability;
use pantheon_mcp::manager::McpTransport;
use pantheon_tools::tools::ToolRegistry;
use pantheon_web::browser::BackendKind;

// ---------------------------------------------------------------------------
// computer.rs: CUA driver spec
// ---------------------------------------------------------------------------

/// An unknown driver id must fail closed: the runtime never launches a
/// binary it does not recognize as the CUA driver.
#[test]
fn computer_unknown_driver_is_rejected() {
    assert!(cua_driver_spec(Some("not-a-driver"), None).is_none());
    // Case-insensitivity: the one known driver id is accepted either way.
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("cua-driver");
    std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
    let bin = bin.to_string_lossy().into_owned();
    assert!(cua_driver_spec(Some("CUA-DRIVER"), Some(&bin)).is_some());
    assert_eq!(CUA_DRIVER_ID, "cua-driver");
}

/// A configured binary that does not exist fails closed rather than
/// resolving to a dangling command.
#[test]
fn computer_missing_explicit_binary_fails_closed() {
    assert!(cua_driver_spec(Some("cua-driver"), Some("/nonexistent/cua-driver")).is_none());
}

/// An explicit binary builds a stdio MCP spec for the CUA driver server,
/// using the configured binary verbatim. Both an explicit driver id and
/// the default (None) driver resolve.
#[test]
fn computer_explicit_binary_builds_stdio_mcp_spec() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("cua-driver");
    std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
    let bin = bin.to_string_lossy().into_owned();

    for driver in [Some("cua-driver"), None] {
        let spec = cua_driver_spec(driver, Some(&bin)).expect("explicit binary resolves");
        assert_eq!(spec.name, CUA_DRIVER_SERVER);
        assert_eq!(spec.transport, McpTransport::Stdio);
        assert_eq!(spec.args, vec!["mcp".to_string()]);
        assert!(spec.enabled);
        assert_eq!(
            spec.command.as_deref(),
            Some(bin.as_str()),
            "the configured binary is used verbatim"
        );
    }
}

// ---------------------------------------------------------------------------
// nightly_tools.rs: the durable tool-disable list
// ---------------------------------------------------------------------------

/// Record then load: the disable list round-trips the tool name, and the
/// file on disk is valid JSON containing the record.
#[test]
fn nightly_disabled_tools_round_trip() {
    let dd = tempfile::tempdir().unwrap();
    assert!(load_disabled_tools(dd.path()).is_empty());
    record_disabled_tool(dd.path(), "badtool", "every call errored").unwrap();
    assert_eq!(load_disabled_tools(dd.path()), vec!["badtool".to_string()]);
    let text = std::fs::read_to_string(disabled_tools_path(dd.path())).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(v.as_array().unwrap().iter().any(|r| {
        r.get("name").and_then(|n| n.as_str()) == Some("badtool")
            && r.get("reason").and_then(|n| n.as_str()) == Some("every call errored")
    }));
}

/// Re-disabling the same tool updates the record in place: no duplicate
/// rows, and insertion order is preserved.
#[test]
fn nightly_re_disabling_updates_in_place() {
    let dd = tempfile::tempdir().unwrap();
    record_disabled_tool(dd.path(), "t", "first").unwrap();
    record_disabled_tool(dd.path(), "t", "second").unwrap();
    record_disabled_tool(dd.path(), "u", "other").unwrap();
    assert_eq!(
        load_disabled_tools(dd.path()),
        vec!["t".to_string(), "u".to_string()],
        "one row per name, in first-seen order"
    );
}

/// Fail-open: a missing or corrupt disable file reads as empty, so a
/// broken list can never brick the registry build - and a later record
/// replaces the corrupt file.
#[test]
fn nightly_corrupt_file_reads_as_empty() {
    let dd = tempfile::tempdir().unwrap();
    let path = disabled_tools_path(dd.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not json {{{").unwrap();
    assert!(load_disabled_tools(dd.path()).is_empty());
    record_disabled_tool(dd.path(), "t", "r").unwrap();
    assert_eq!(load_disabled_tools(dd.path()), vec!["t".to_string()]);
}

// ---------------------------------------------------------------------------
// tool_config.rs: documented defaults
// ---------------------------------------------------------------------------

/// The browser tool defaults are a public contract: callers depend on
/// the backend, approval behavior, timeouts, and secret names.
#[test]
fn browser_defaults_match_documented_behavior() {
    let cfg = BrowserToolConfig::default();
    assert!(cfg.enabled);
    assert_eq!(cfg.backend, BackendKind::Gsd);
    assert!(cfg.binary.is_none());
    assert!(cfg.act_require_approval);
    assert_eq!(cfg.idle_timeout_secs, 900);
    assert_eq!(
        cfg.vault_key_secret.as_deref(),
        Some("GSD_BROWSER_VAULT_KEY")
    );
    assert_eq!(cfg.steel_api_key_secret.as_deref(), Some("STEEL_API_KEY"));
    assert!(cfg.steel_base_url.is_none());
    assert_eq!(
        cfg.browserbase_api_key_secret.as_deref(),
        Some("BROWSERBASE_API_KEY")
    );
    assert!(cfg.browserbase_project_id.is_none());
    assert!(cfg.lightpanda_cdp_url.is_none());
    assert!(cfg.playwright_binary.is_none());
    assert!(cfg.chrome_binary.is_none());
    assert!(cfg.headless);
    assert_eq!(cfg.timeout_secs, 120);
    // Camofox defaults mirror the backend's (CamofoxConfig).
    let c = &cfg.camofox;
    assert!(c.python.is_none());
    assert!(c.headless);
    assert!(!c.headless_virtual);
    assert!(c.os.is_none());
    assert!(c.humanize_secs.is_none());
    assert!(!c.geoip);
    assert!(c.locale.is_none());
    assert!(c.timezone.is_none());
    assert!(c.proxy_server.is_none());
    assert!(c.proxy_username.is_none());
    assert!(c.proxy_password_secret.is_none());
    assert!(!c.block_images);
    assert!(!c.block_webrtc);
    assert!(c.fingerprint_preset);
    assert_eq!(c.timeout_secs, 120);
    assert_eq!(c.startup_timeout_secs, 300);
}

/// The websearch defaults are a public contract: enabled, the default
/// provider, and no pinned secret name (the session resolves the
/// provider's own default key env var).
#[test]
fn websearch_defaults_match_documented_behavior() {
    let cfg = WebsearchToolConfig::default();
    assert!(cfg.enabled);
    assert_eq!(cfg.provider, "tinyfish");
    assert_eq!(cfg.api_key_secret.as_deref(), None);
    assert_eq!(cfg.max_results, 8);
}

// ---------------------------------------------------------------------------
// skill_exec_tool.rs: skill_exec registration
// ---------------------------------------------------------------------------

fn test_skill(name: &str, dir: &str, exec_yaml: &str) -> Skill {
    let p = std::path::PathBuf::from(format!("/tmp/skills-rt/{name}/SKILL.md"));
    let front = format!("---\nname: {name}\ndescription: d\n{exec_yaml}---\n\n# {name}\n");
    let mut s = parse_skill(&front, &p).unwrap();
    s.dir = std::path::PathBuf::from(dir);
    s
}

const EXEC_YAML: &str = "exec:\n  - name: fetch\n    command: scripts/fetch.sh\n    side_effects: read\n    description: \"Fetch things\"\n  - name: write-it\n    command: scripts/write.sh\n    side_effects: write\n";

/// skill_exec registers when at least one skill exists, carrying the
/// static read-only capability; per-call extras come from
/// `required_capabilities`.
#[test]
fn skill_exec_registers_when_skills_exist() {
    let mut reg = ToolRegistry::new();
    register_skill_exec_tool(
        &mut reg,
        vec![test_skill("plain", "/tmp/skills-rt/plain", "")],
    );
    assert!(reg.get(SKILL_EXEC_TOOL_NAME).is_some());
    assert_eq!(
        reg.capability_of(SKILL_EXEC_TOOL_NAME),
        Some(Capability::FilesystemRead)
    );
}

/// No skills, no tool: the registry stays clean.
#[test]
fn skill_exec_skips_registration_with_no_skills() {
    let mut reg = ToolRegistry::new();
    register_skill_exec_tool(&mut reg, vec![]);
    assert!(reg.get(SKILL_EXEC_TOOL_NAME).is_none());
}

/// The tool description advertises each skill's declared executables
/// with side-effect tags plus the skill-directory mapping, so the model
/// knows exactly what skill_exec can run.
#[test]
fn skill_exec_description_lists_executables_and_dirs() {
    let mut reg = ToolRegistry::new();
    register_skill_exec_tool(
        &mut reg,
        vec![
            test_skill("with-exec", "/tmp/skills-rt/with-exec", EXEC_YAML),
            test_skill("plain", "/tmp/skills-rt/plain", ""),
        ],
    );
    let desc = &reg.get(SKILL_EXEC_TOOL_NAME).unwrap().schema.description;
    // Declared executables with side-effect tags.
    assert!(desc.contains("\"with-exec\""), "{desc}");
    assert!(desc.contains("\"fetch\""), "{desc}");
    assert!(desc.contains("[read]"), "{desc}");
    assert!(desc.contains("\"write-it\""), "{desc}");
    assert!(desc.contains("[write]"), "{desc}");
    // Skill-directory mapping for third-party prose invocation.
    assert!(desc.contains("/tmp/skills-rt/with-exec"), "{desc}");
    assert!(desc.contains("/tmp/skills-rt/plain"), "{desc}");
    assert!(desc.contains("CLAUDE_SKILL_DIR"), "{desc}");
    // skill_exec's scope boundary, stated in the description itself.
    assert!(desc.contains("only runs exec:-declared entries"), "{desc}");
}

/// When no skill declares an executable the description says so
/// explicitly instead of implying executables exist.
#[test]
fn skill_exec_description_says_none_when_no_executables_declared() {
    let mut reg = ToolRegistry::new();
    register_skill_exec_tool(
        &mut reg,
        vec![test_skill("plain", "/tmp/skills-rt/plain", "")],
    );
    let desc = &reg.get(SKILL_EXEC_TOOL_NAME).unwrap().schema.description;
    assert!(
        desc.contains("No skill currently declares an executable"),
        "{desc}"
    );
    // The dir mapping is still there: that is why the tool registers.
    assert!(desc.contains("/tmp/skills-rt/plain"), "{desc}");
}

/// Extra capabilities follow the declared side effects: a read
/// executable needs only the static read capability, a write
/// executable additionally requires ShellExecute (the normal approval
/// path); unresolvable calls fail closed to the static capability and
/// the executor rejects the call itself.
#[test]
fn skill_exec_extra_capabilities_follow_side_effects() {
    let mut reg = ToolRegistry::new();
    register_skill_exec_tool(
        &mut reg,
        vec![test_skill(
            "with-exec",
            "/tmp/skills-rt/with-exec",
            EXEC_YAML,
        )],
    );
    let caps = |skill: &str, name: &str| {
        reg.required_capabilities(
            SKILL_EXEC_TOOL_NAME,
            &serde_json::json!({"skill": skill, "name": name}).to_string(),
        )
    };
    // Read-side-effect: only the static FilesystemRead.
    assert_eq!(caps("with-exec", "fetch"), vec![Capability::FilesystemRead]);
    // Write-side-effect: additionally ShellExecute (normal approval path).
    let write_caps = caps("with-exec", "write-it");
    assert!(
        write_caps.contains(&Capability::ShellExecute),
        "{write_caps:?}"
    );
    assert!(
        write_caps.contains(&Capability::FilesystemRead),
        "{write_caps:?}"
    );
    // Unresolvable call: the gate treats it as an execution attempt and
    // requires ShellExecute. It previously fell back to the static
    // FilesystemRead alone, which meant the gate reported a read for a
    // call whose nature is an execution; the executor rejecting the call
    // was the only thing keeping it from running. The capability that
    // matches the tool's nature is the one the gate should ask for.
    let unknown_caps = caps("with-exec", "nope");
    assert!(
        unknown_caps.contains(&Capability::ShellExecute),
        "an unresolvable skill_exec must still require ShellExecute: {unknown_caps:?}"
    );
    assert!(
        unknown_caps.contains(&Capability::FilesystemRead),
        "{unknown_caps:?}"
    );
}

/// Unknown skill/exec names and malformed args fail with distinct,
/// machine-readable error codes.
#[test]
fn skill_exec_execute_rejects_unknown_skill_and_exec() {
    let mut reg = ToolRegistry::new();
    register_skill_exec_tool(
        &mut reg,
        vec![test_skill(
            "with-exec",
            "/tmp/skills-rt/with-exec",
            EXEC_YAML,
        )],
    );
    let err = reg
        .execute(
            SKILL_EXEC_TOOL_NAME,
            &serde_json::json!({"skill": "ghost", "name": "fetch"}).to_string(),
        )
        .unwrap_err();
    assert_eq!(err.code, "SKILL_UNKNOWN");
    let err = reg
        .execute(
            SKILL_EXEC_TOOL_NAME,
            &serde_json::json!({"skill": "with-exec", "name": "ghost"}).to_string(),
        )
        .unwrap_err();
    assert_eq!(err.code, "SKILL_UNKNOWN_EXEC");
    let err = reg.execute(SKILL_EXEC_TOOL_NAME, "not json").unwrap_err();
    assert_eq!(err.code, "TOOL_BAD_ARGS");
}

// ---------------------------------------------------------------------------
// permission_mode.rs: the three-mode gate
// ---------------------------------------------------------------------------

/// End-to-end through a real `Session`: a config file naming a permission
/// mode must actually change what the session reports, and an unparseable
/// value must fall back to `Ask` rather than widening anything.
///
/// This is the wiring proof. The unit tests in `permission_mode` cover the
/// ladder; this covers "the config is read and reaches the session".
#[test]
fn configured_permission_mode_reaches_the_session() {
    use pantheon_api::capability::Policy;
    use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy};
    use pantheon_api::permission_mode::PermissionMode;
    use pantheon_runtime::session::Session;

    // A minimal, deterministic model policy: this test is about the
    // permission-mode plumbing, not model resolution.
    fn model_policy() -> ModelPolicy {
        ModelPolicy {
            reasoning_budget: Default::default(),
            reasoning: Default::default(),
            default: DefaultModel {
                provider: "test".into(),
                model: "test".into(),
            },
            fallbacks: FallbackChain {
                fallbacks: Vec::new(),
            },
            auxiliaries: Vec::new(),
        }
    }

    fn session_with(permission_mode: Option<&str>) -> Session {
        let dir = tempfile::tempdir().unwrap();
        // Keep the tempdir alive for the session's lifetime by leaking it;
        // the test process exits shortly after.
        let path = dir.keep();
        if let Some(mode) = permission_mode {
            std::fs::write(
                path.join("config.toml"),
                format!("permission_mode = \"{mode}\"\n"),
            )
            .unwrap();
        }
        Session::new(
            path,
            Policy::coder(),
            model_policy(),
            pantheon_secrets::SecretsBroker::new(),
        )
        .unwrap()
    }

    assert_eq!(
        session_with(None).permission_mode(),
        PermissionMode::Ask,
        "an absent setting must default to the conservative mode"
    );
    assert_eq!(
        session_with(Some("smart")).permission_mode(),
        PermissionMode::Smart
    );
    assert_eq!(
        session_with(Some("allow_all")).permission_mode(),
        PermissionMode::AllowAll
    );
    assert_eq!(
        session_with(Some("nonsense")).permission_mode(),
        PermissionMode::Ask,
        "a typo must never silently widen what runs unattended"
    );
}

// ---------------------------------------------------------------------------
// Budget: the consecutive-failure cap
// ---------------------------------------------------------------------------

/// The cap bounds *fruitless* work, which `max_tool_calls` does not: a
/// model retrying one broken command can otherwise spend the whole tool
/// budget on it, paying a provider round trip per retry. A success must
/// reset the streak so a long run that is making progress is never cut.
#[test]
fn consecutive_failure_cap_bounds_fruitless_retries() {
    use pantheon_agent::Budget;

    let b = Budget::default();
    assert_eq!(
        b.max_consecutive_tool_failures, 5,
        "the cap must be on by default: an unbounded retry loop is the bug"
    );

    // The semantics the drive loop implements: a success resets to zero,
    // a failure extends. Simulate the counter directly so the contract is
    // pinned here even if the loop is refactored.
    let mut streak = 0u32;
    let mut tripped_at = None;
    for (n, failed) in [true, true, false, true, true, true, true, true]
        .into_iter()
        .enumerate()
    {
        if failed {
            streak += 1;
        } else {
            streak = 0;
        }
        if streak >= b.max_consecutive_tool_failures {
            tripped_at = Some(n);
            break;
        }
    }
    assert_eq!(
        tripped_at,
        Some(7),
        "the streak must reset on the success at index 2, so it trips on \
         the fifth consecutive failure after it, not earlier"
    );
}

/// `0` disables the cap, and the config resolver must preserve that
/// rather than treating it as unset (which is how every other budget key
/// behaves).
#[test]
fn zero_disables_the_failure_cap() {
    use pantheon_api::config::BudgetSection;

    let disabled = BudgetSection {
        max_consecutive_tool_failures: Some(0),
        ..Default::default()
    };
    let b = pantheon_tui::config::resolve_budget_section(&disabled);
    assert_eq!(
        b.max_consecutive_tool_failures, 0,
        "an explicit 0 must mean 'no cap', not 'fall back to the default'"
    );

    let absent = BudgetSection::default();
    let b = pantheon_tui::config::resolve_budget_section(&absent);
    assert_eq!(
        b.max_consecutive_tool_failures, 5,
        "an absent key takes the default"
    );

    let tuned = BudgetSection {
        max_consecutive_tool_failures: Some(12),
        ..Default::default()
    };
    let b = pantheon_tui::config::resolve_budget_section(&tuned);
    assert_eq!(b.max_consecutive_tool_failures, 12);
}
