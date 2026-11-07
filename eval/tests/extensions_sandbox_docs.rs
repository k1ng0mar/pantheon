//! D5 residual regression: hook versus tool plugin sandbox claims.
//!
//! The roadmap's blanket line that plugins are unsandboxed went stale in
//! halves. Hook plugins (`pantheon-extensions` python runner) go through
//! `build_sandboxed_checked` at the VeryHigh level whenever a capability
//! policy is configured, and spawn directly only when no policy exists.
//! Tool plugins (`pantheon-exec` supervisor) still spawn a sealed runner
//! with no namespace sandbox. The user docs must keep those two facts
//! distinct, and the code markers below pin the behavior the docs claim.

/// User guide for hook and tool plugins.
const EXTENSIONS_MD: &str = include_str!("../../docs/user-guide/extensions.md");

/// Architecture overview that repeats the plugin trust claim.
const ARCHITECTURE_MD: &str = include_str!("../../docs/developer/architecture.md");

/// Hook plugin runner that builds children through the sandbox gate.
///
/// `python_runner` is now a re-export inside `plugins.rs`; the former
/// `python_runner.rs` file no longer exists.
const HOOK_RUNNER_RS: &str = include_str!("../../crates/pantheon-extensions/src/plugins.rs");

/// Tool plugin supervisor that seals and spawns runners.
///
/// The supervisor moved into the `sandbox_group` module when the exec sandbox
/// was consolidated; `crates/pantheon-exec/src/supervisor.rs` no longer
/// exists. Point at the file that actually defines `PluginSupervisor`.
const TOOL_SUPERVISOR_RS: &str = include_str!("../../crates/pantheon-exec/src/sandbox_group.rs");

/// PluginEnable must map to the strongest sandbox level.
const SANDBOX_MOD_RS: &str = include_str!("../../crates/pantheon-exec/src/sandbox/mod.rs");

#[test]
fn docs_distinguish_tool_and_hook_sandboxing() {
    for doc in [EXTENSIONS_MD, ARCHITECTURE_MD] {
        assert!(
            doc.contains("Tool plugins"),
            "docs must name tool plugins separately from hook plugins"
        );
        assert!(
            doc.contains("Hook plugins"),
            "docs must name hook plugins separately from tool plugins"
        );
        assert!(
            doc.contains("VeryHigh"),
            "docs must state the hook sandbox level"
        );
        assert!(
            !doc.contains("Pantheon does **not** sandbox plugins"),
            "docs must not restore the blanket unsandboxed claim"
        );
        assert!(
            !doc.contains("Plugins are not sandboxed:"),
            "docs must not restore the blanket unsandboxed claim"
        );
    }
    assert!(
        EXTENSIONS_MD.contains("no namespace sandbox"),
        "docs must state that tool plugins have no namespace sandbox today"
    );
    assert!(
        EXTENSIONS_MD.contains("spawned directly only when no policy is configured"),
        "docs must state the hook direct-spawn condition"
    );
}

#[test]
fn hook_runner_sandboxes_when_policy_is_configured() {
    assert!(
        HOOK_RUNNER_RS.contains("build_sandboxed_checked"),
        "hook runner must build children through build_sandboxed_checked"
    );
    assert!(
        HOOK_RUNNER_RS.contains("Enforcement::Run(profile)"),
        "hook runner must take the sandbox profile from the capability gate"
    );
    assert!(
        SANDBOX_MOD_RS.contains("| Capability::PluginEnable"),
        "PluginEnable must stay in the VeryHigh capability group"
    );
}

#[test]
fn tool_supervisor_sandboxes_through_staged_runner() {
    assert!(
        TOOL_SUPERVISOR_RS.contains("spawn_verified"),
        "tool supervisor must keep its verified spawn entry point"
    );
    assert!(
        TOOL_SUPERVISOR_RS.contains("stage_runner_bytes"),
        "tool supervisor must stage runner bytes content addressed"
    );
    assert!(
        TOOL_SUPERVISOR_RS.contains("build_sandboxed_checked"),
        "tool supervisor must build children through the sandbox gate"
    );
    assert!(
        TOOL_SUPERVISOR_RS.contains("PLUGIN_SANDBOX_UNAVAILABLE"),
        "tool supervisor must fail closed when the wrapper is unavailable"
    );
}
