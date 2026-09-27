//! Tests for `pantheon_migration::providers::tests` — sibling file so sources stay test-free.
//!
//! Kept in-file: the `toml_key` quoting test exercises a private helper not
//! reachable through the public API, so it cannot move to `eval/`.
//! Everything else from this file moved to
//! `eval/tests/migration_providers.rs`.
use super::*;

#[test]
fn an_id_needing_toml_quoting_is_quoted() {
    assert_eq!(toml_key("hp-llm-router"), "hp-llm-router");
    assert_eq!(toml_key("has space"), "\"has space\"");
    assert_eq!(toml_key(""), "\"\"");
}

