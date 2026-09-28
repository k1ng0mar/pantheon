//! Tests for `pantheon_memory::plugins::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn manifest_validation_reports_the_missing_field() {
    let err = parse_manifest("name = \"x\"\nkind = \"http\"\n").unwrap_err();
    assert_eq!(err.code, "MEM_PLUGIN_MANIFEST");
    assert!(err.cause.contains("url"), "{}", err.cause);
    let err2 = parse_manifest("name = \"x\"\nkind = \"stdio\"\n").unwrap_err();
    assert!(err2.cause.contains("command"), "{}", err2.cause);
    let err3 = parse_manifest("name = \"x\"\nkind = \"carrier-pigeon\"\n").unwrap_err();
    assert!(err3.cause.contains("unknown kind"), "{}", err3.cause);
}
