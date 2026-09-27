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
fn the_registry_covers_the_spec_command_list() {
    let r = registry();
    for want in [
        "models", "sessions", "resume", "memory", "tools", "skills", "settings", "doctor",
        "status", "help",
    ] {
        assert!(r.contains_key(want), "/{want} is missing from the registry");
    }
}

#[test]
fn cost_is_not_a_command() {
    assert!(
        !registry().contains_key("cost"),
        "/cost was removed; the header carries tokens and elapsed"
    );
}
