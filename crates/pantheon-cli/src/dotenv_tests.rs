//! Tests for `crate::dotenv` (lives here so source files stay test-free).

use crate::dotenv::*;

#[test]
fn parses_shapes_and_ignores_noise() {
    let pairs = parse_dotenv(
        "# comment\n\nexport A=1\nB='two words'\nC=\"three\"\nD=k1,k2\nbad line\nE=trailing # kept? \n",
    );
    let get = |k: &str| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(get("A").as_deref(), Some("1"));
    assert_eq!(get("B").as_deref(), Some("two words"));
    assert_eq!(get("C").as_deref(), Some("three"));
    assert_eq!(get("D").as_deref(), Some("k1,k2"));
    assert_eq!(get("E").as_deref(), Some("trailing"));
    assert!(get("bad").is_none());
}

#[test]
fn upsert_preserves_comments_and_replaces_in_place() {
    let dir = std::env::temp_dir().join(format!("pantheon-dotenv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    upsert_dotenv(&dir, "K1", "v1").unwrap();
    upsert_dotenv(&dir, "K2", "a,b").unwrap();
    std::fs::write(dir.join(".env"), "# keys\nK1=v1\nK2=a,b\nK1=stale\n").unwrap();
    upsert_dotenv(&dir, "K1", "v2").unwrap();
    let text = std::fs::read_to_string(dir.join(".env")).unwrap();
    assert_eq!(text, "# keys\nK1=v2\nK2=a,b\n");
    assert_eq!(read_dotenv_value(&dir, "K2").as_deref(), Some("a,b"));
    assert!(delete_dotenv_key(&dir, "K1").unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.join(".env")).unwrap(),
        "# keys\nK2=a,b\n"
    );
    assert!(!delete_dotenv_key(&dir, "K1").unwrap());
    assert!(!delete_dotenv_key(&dir.join("missing"), "K1").unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_does_not_override_exported_vars() {
    let dir = std::env::temp_dir().join(format!("pantheon-dotenv-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".env"),
        "PANTHEON_DOTENV_T1=stale\nPANTHEON_DOTENV_T1=from-file\n",
    )
    .unwrap();
    std::env::set_var("PANTHEON_DOTENV_T1", "from-env");
    load_dotenv(&dir);
    assert_eq!(std::env::var("PANTHEON_DOTENV_T1").unwrap(), "from-env");
    std::env::remove_var("PANTHEON_DOTENV_T1");
    load_dotenv(&dir);
    assert_eq!(std::env::var("PANTHEON_DOTENV_T1").unwrap(), "from-file");
    std::env::remove_var("PANTHEON_DOTENV_T1");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The runtime's dotenv reader and the migration's must agree on values.
///
/// They are two implementations of the same format in two crates, and they
/// already drifted once: the CLI stripped an inline ` # comment` from an
/// unquoted value and the migration did not, so a key carried by
/// `pantheon migrate` could land in `<data_dir>/.env` with the comment still
/// attached. This test is the tripwire for that class.
#[test]
fn migrate_and_runtime_dotenv_parsers_agree_on_values() {
    let cases = [
        "K=plain",
        "K=value # trailing comment",
        "K=\"quoted # hash\"",
        "K='single # hash'",
        "K=  spaced  ",
        "export K=exported",
        "K=",
        "K=\"\"",
        "# whole line comment",
        "",
        "K=a=b=c",
        "K=has#nospace",
    ];
    for case in cases {
        let runtime = crate::dotenv::parse_dotenv(case);
        let migrate = {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-dotenv-parity-{}-{}",
                std::process::id(),
                case.len()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(".env"), format!("{case}\n")).unwrap();
            let v = pantheon_migrate::read_dotenv(&dir.join(".env"));
            let _ = std::fs::remove_dir_all(&dir);
            v
        };
        assert_eq!(
            runtime, migrate,
            "parsers disagree on {case:?}\n  runtime: {runtime:?}\n  migrate: {migrate:?}"
        );
    }
}

/// No test file may declare its own env lock.
///
/// The CLI crate mutates `PANTHEON_DATA_DIR` and other process-global vars
/// from tests running in parallel, and serializing that correctly needs
/// exactly one lock. A file-local `OnceLock<Mutex<()>>` reads as "this file
/// is careful" while providing no exclusion against any other file, and the
/// failure mode is a whole test binary exiting with status 1 and no panic
/// message. That is what happened in CI on the commit that added
/// `fallback_cli_tests`.
///
/// This is a source check, not a runtime one: the bug is in the shape of
/// the code, and it can only be seen by reading the declarations.
#[test]
fn no_test_file_declares_a_private_env_lock() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("src dir readable") {
        let p = entry.expect("dir entry").path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.ends_with("_tests.rs") {
            continue;
        }
        // This file names every pattern it searches for, so scanning it would
        // always fail. It declares no lock of its own, which is the invariant.
        if name == "dotenv_tests.rs" {
            continue;
        }
        let body = std::fs::read_to_string(&p).expect("test file readable");
        for (n, line) in body.lines().enumerate() {
            let t = line.trim();
            // Skip prose: doc comments name the patterns on purpose.
            if t.starts_with("//") {
                continue;
            }
            if t.contains("OnceLock<Mutex")
                || t.contains("lazy_static")
                || t.contains("static TEST_ENV_LOCK")
            {
                offenders.push(format!("{}:{}: {}", name, n + 1, t));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "use crate::dotenv::test_support::TEST_ENV_LOCK, not a private lock:\n{}",
        offenders.join("\n")
    );
}
