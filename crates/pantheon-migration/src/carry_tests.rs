//! Tests for `pantheon_migration::carry::tests` — sibling file so sources stay test-free.
//!
//! Kept in-file: `indirection_names_are_extracted_only_when_pure` tests the
//! private `indirection_name` helper, which is not reachable through the
//! public API, so it cannot move to `eval/`. Everything else from this file
//! moved to `eval/tests/migration_carry.rs`.
use super::*;

#[test]
fn indirection_names_are_extracted_only_when_pure() {
    assert_eq!(indirection_name("${FOO}"), Some("FOO".into()));
    assert_eq!(indirection_name(" ${FOO} "), Some("FOO".into()));
    assert_eq!(indirection_name("Bearer ${FOO}"), None);
    assert_eq!(indirection_name("plain"), None);
    assert_eq!(indirection_name("${}"), None);
}

#[test]
fn a_source_env_is_merged_into_the_pantheon_key_store() {
    let d = tmp("env-merge");
    let src = d.join(".env");
    fs::write(
        &src,
        "OPENAI_API_KEY=sk-sourcevalue12345\nTELEGRAM_BOT_TOKEN=999:aaa\nHOME=/root\n",
    )
    .unwrap();

    let data = d.join("data");
    let r = merge_env_into(&data, "hermes", &src).unwrap();

    // The destination is pantheon's own .env, not a foreign copy.
    assert_eq!(r.path, data.join(".env").to_string_lossy());
    assert!(data.join(".env").is_file());

    // Only credentials carried; HOME stayed behind.
    assert_eq!(r.added, vec!["OPENAI_API_KEY", "TELEGRAM_BOT_TOKEN"]);
    assert_eq!(r.unclassified, vec!["HOME"]);

    // The value is in the store, and only there.
    let body = fs::read_to_string(data.join(".env")).unwrap();
    assert!(body.contains("OPENAI_API_KEY=sk-sourcevalue12345"));
    assert!(!body.contains("HOME"));
}
#[test]
fn an_existing_pantheon_key_is_never_clobbered() {
    let d = tmp("env-noclobber");
    let data = d.join("data");
    fs::create_dir_all(&data).unwrap();
    // The operator already set this one in Pantheon.
    fs::write(data.join(".env"), "OPENAI_API_KEY=sk-alreadyset99999\n").unwrap();

    let src = d.join(".env");
    fs::write(
        &src,
        "OPENAI_API_KEY=sk-sourcevalue12345\nANTHROPIC_API_KEY=sk-newvalue77777\n",
    )
    .unwrap();

    let r = merge_env_into(&data, "hermes", &src).unwrap();

    assert_eq!(r.already_present, vec!["OPENAI_API_KEY"]);
    assert_eq!(r.added, vec!["ANTHROPIC_API_KEY"]);

    let body = fs::read_to_string(data.join(".env")).unwrap();
    assert!(
        body.contains("OPENAI_API_KEY=sk-alreadyset99999"),
        "the operator's value must survive: {body}"
    );
    assert!(!body.contains("sk-sourcevalue12345"));
    assert!(body.contains("ANTHROPIC_API_KEY=sk-newvalue77777"));
}
#[test]
fn merging_preserves_comments_and_order() {
    let d = tmp("env-preserve");
    let data = d.join("data");
    fs::create_dir_all(&data).unwrap();
    fs::write(
        data.join(".env"),
        "# pantheon key store\nEXISTING_ONE=keepme\n# a comment in the middle\n",
    )
    .unwrap();
    let src = d.join(".env");
    fs::write(&src, "NEW_KEY=newvalue12345\n").unwrap();

    merge_env_into(&data, "hermes", &src).unwrap();
    let body = fs::read_to_string(data.join(".env")).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines[0], "# pantheon key store");
    assert_eq!(lines[1], "EXISTING_ONE=keepme");
    assert_eq!(lines[2], "# a comment in the middle");
    assert!(body.contains("NEW_KEY=newvalue12345"));
}
#[test]
fn empty_and_placeholder_values_are_not_carried() {
    let d = tmp("env-empty");
    let src = d.join(".env");
    fs::write(
        &src,
        "OPENAI_API_KEY=\nANTHROPIC_API_KEY=your_key_here\nGEMINI_API_KEY=<paste>\nQWEN_API_KEY=sk-realvalue42\n",
    )
    .unwrap();
    let data = d.join("data");
    let r = merge_env_into(&data, "hermes", &src).unwrap();
    assert_eq!(r.added, vec!["QWEN_API_KEY"]);
    assert_eq!(r.no_value.len(), 3, "{:?}", r.no_value);
    let body = fs::read_to_string(data.join(".env")).unwrap();
    assert!(!body.contains("your_key_here"));
    assert!(!body.contains("<paste>"));
}
#[test]
fn the_merge_report_never_carries_a_value() {
    let d = tmp("env-report");
    let src = d.join(".env");
    fs::write(&src, "OPENAI_API_KEY=sk-verysecretvalue1234\n").unwrap();
    let data = d.join("data");
    let r = merge_env_into(&data, "hermes", &src).unwrap();
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains("sk-verysecretvalue1234"), "{json}");
    // And the sidecar manifest is names-only too.
    let m = fs::read_to_string(data.join("credentials/hermes.json")).unwrap();
    assert!(m.contains("OPENAI_API_KEY"));
    assert!(!m.contains("sk-verysecretvalue1234"), "{m}");
}

#[cfg(unix)]
#[test]
fn the_key_store_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp("env-perm");
    let src = d.join(".env");
    fs::write(&src, "OPENAI_API_KEY=sk-value12345678\n").unwrap();
    let data = d.join("data");
    merge_env_into(&data, "hermes", &src).unwrap();
    let mode = fs::metadata(data.join(".env"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
}
