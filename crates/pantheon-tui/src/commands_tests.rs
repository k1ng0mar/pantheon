//! Tests for the command registry.

use super::*;

#[test]
fn completion_filters_by_prefix() {
    let got = complete("/mo");
    assert_eq!(got, vec!["/model".to_string(), "/models".to_string()]);
}

#[test]
fn an_empty_prefix_returns_the_whole_registry() {
    assert!(!complete("/").is_empty());
    assert_eq!(complete("/").len(), registry().len());
}

#[test]
fn completion_is_case_insensitive() {
    assert_eq!(complete("/MOD").len(), 2);
}

#[test]
fn an_unknown_prefix_returns_nothing_rather_than_everything() {
    assert!(complete("/zzz").is_empty());
}

#[test]
fn every_command_has_a_description_and_a_category() {
    for (name, c) in registry() {
        assert!(!c.desc.is_empty(), "{name} has no description");
        assert!(!c.category.is_empty(), "{name} has no category");
        assert_eq!(c.name, name, "the map key and the name disagree");
    }
}

#[test]
fn the_registry_covers_the_dispatched_commands() {
    let r = registry();
    // Every arm in handle_slash (pantheon-cli/src/tui.rs) resolves here.
    // Absent on purpose: cost, memory, tools, debug, provenance, events
    // (see the registry doc comment for why).
    for want in [
        "models",
        "model",
        "reasoning",
        "sessions",
        "resume",
        "new",
        "compress",
        "export",
        "runs",
        "remember",
        "skills",
        "agent",
        "agents",
        "collab",
        "tasks",
        "inbox",
        "history",
        "name",
        "settings",
        "gateway",
        "doctor",
        "status",
        "help",
        "clear",
        "quit",
        "exit",
    ] {
        assert!(r.contains_key(want), "/{want} is missing from the registry");
    }
    for gone in ["cost", "memory", "tools", "debug", "provenance", "events"] {
        assert!(
            !r.contains_key(gone),
            "/{gone} was removed from the surface"
        );
    }
}

#[test]
fn cost_is_not_a_command() {
    assert!(
        !registry().contains_key("cost"),
        "/cost was removed; the header carries tokens and elapsed"
    );
}
