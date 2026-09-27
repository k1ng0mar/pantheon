//! Tests for `ToolRegistry::execute_gated` — the single place the capability
//! gate lives for callers outside the agent loop.
use super::*;
use pantheon_api::capability::Policy;

fn registry_with(tag: &'static str) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(
        ToolSchema {
            name: "dangerous".into(),
            description: "test tool".into(),
            parameters: serde_json::json!({}),
        },
        Capability::ShellExecute,
        move |_args| Ok(tag.to_string()),
    );
    reg
}

/// The gate must block a tool the policy does not grant, and the error has
/// to name the tool and the capability so the denial is debuggable rather
/// than a silent no-op.
#[test]
fn gated_execute_blocks_a_capability_the_policy_lacks() {
    let reg = registry_with("ran");
    // researcher_readonly does not grant ShellExecute.
    let err = reg
        .execute_gated(&Policy::researcher_readonly(), "dangerous", "{}")
        .unwrap_err();
    assert_eq!(err.code, "TOOL_DENIED");
    assert!(err.cause.contains("dangerous"), "error omits the tool name");
    assert!(
        err.cause.contains("ShellExecute"),
        "error omits the missing capability: {}",
        err.cause
    );
}

/// A denied call must not have invoked the tool. The closure appends to a
/// shared cell, so a gate that ran the tool and then reported denial would
/// be caught here.
#[test]
fn a_denied_call_never_reaches_the_tool_body() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let ran = Arc::new(AtomicUsize::new(0));
    let counter = ran.clone();
    let mut reg = ToolRegistry::new();
    reg.register(
        ToolSchema {
            name: "counter".into(),
            description: "test tool".into(),
            parameters: serde_json::json!({}),
        },
        Capability::ShellExecute,
        move |_args| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok("ok".into())
        },
    );
    let _ = reg.execute_gated(&Policy::researcher_readonly(), "counter", "{}");
    assert_eq!(ran.load(Ordering::SeqCst), 0, "denied tool still ran");
}

/// The allowed path must still work, and must return the tool's own output.
#[test]
fn gated_execute_allows_what_the_policy_grants_and_returns_the_output() {
    let reg = registry_with("ran");
    let out = reg
        .execute_gated(&Policy::coder(), "dangerous", "{}")
        .unwrap();
    assert_eq!(out, "ran");
}

/// An unknown tool is still `TOOL_UNKNOWN`, not a denial: the gate must not
/// mask "no such tool" behind a capability error the user cannot act on.
#[test]
fn gated_execute_still_reports_an_unknown_tool_distinctly() {
    let reg = registry_with("ran");
    let err = reg
        .execute_gated(&Policy::coder(), "nope", "{}")
        .unwrap_err();
    assert_eq!(err.code, "TOOL_UNKNOWN");
}

/// Argument-derived capabilities must be gated too, not just the tool's
/// static one. This is the `git push` through `shell` case: the shell tool's
/// own capability is allowed under `coder`, but the extra GitPush capability
/// that `required_capabilities` adds for this argument is not.
#[test]
fn gated_execute_checks_argument_derived_capabilities() {
    let mut reg = ToolRegistry::new();
    reg.register_with(
        ToolSchema {
            name: "shell".into(),
            description: "test shell".into(),
            parameters: serde_json::json!({}),
        },
        Capability::ShellExecute,
        |_args| Ok("executed".into()),
        Some(Box::new(|_args: &str| vec![Capability::GitPush])),
    );
    let err = reg
        .execute_gated(&Policy::coder(), "shell", "{}")
        .unwrap_err();
    assert_eq!(err.code, "TOOL_DENIED");
    assert!(err.cause.contains("GitPush"), "got: {}", err.cause);
}
