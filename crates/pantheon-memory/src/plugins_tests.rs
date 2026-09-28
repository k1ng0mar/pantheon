//! Tests for `pantheon_memory::plugins::tests` — sibling file so sources stay test-free.
use super::*;
use std::path::PathBuf;

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pantheon-memplug-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

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
