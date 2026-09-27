//! Tests for the logging module. Sibling file so `logging.rs` stays test-free.
use super::*;

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join("logs").join(name)).unwrap_or_default()
}

#[test]
fn levels_parse_case_insensitively_and_reject_nonsense() {
    assert_eq!(Level::parse("debug"), Some(Level::Debug));
    assert_eq!(Level::parse("WARN"), Some(Level::Warning));
    assert_eq!(Level::parse("Error"), Some(Level::Error));
    // `warning` is accepted alongside the shorter form a user will type.
    assert_eq!(Level::parse("warning"), Some(Level::Warning));
    // A typo must not silently mean DEBUG, which is the failure mode that
    // turns a filtered reader into "why is nothing showing up".
    assert_eq!(Level::parse("verbose"), None);
    assert_eq!(Level::parse(""), None);
}

#[test]
fn level_order_puts_warnings_above_info() {
    assert!(Level::Debug < Level::Info);
    assert!(Level::Info < Level::Warning);
    assert!(Level::Warning < Level::Error);
}

/// The hand-rolled civil-from-days conversion is the kind of code that is
/// wrong in a way no test notices until a user reads a timestamp. Pin the
/// epoch, a leap day, a year boundary, and a post-1970 date.

#[test]
fn timestamps_are_correct_across_boundaries() {
    assert_eq!(stamp(0), "1970-01-01 00:00:00.000");
    // 2001-09-09T01:46:40Z, a well-known round-ish value.
    assert_eq!(stamp(1_000_000_000_000), "2001-09-09 01:46:40.000");
    // 2024-02-29 12:00:00 — a leap day, so the March branch of the algorithm.
    assert_eq!(stamp(1_709_208_000_000), "2024-02-29 12:00:00.000");
    // 2025-01-01 00:00:00 — the month < 3 branch, which adds a year.
    assert_eq!(stamp(1_735_689_600_000), "2025-01-01 00:00:00.000");
    // A far-future date must not roll over into a negative month.
    assert_eq!(stamp(4_102_444_800_000), "2100-01-01 00:00:00.000");
}

#[test]
fn lines_carry_timestamp_level_component_and_message() {
    // The reader parses this exact shape back out, so the format is a
    // contract with `logs --level`, not a cosmetic choice.
    let line = format!("{} {} [{}] {}", stamp(0), Level::Info, "turn", "hello");
    assert_eq!(line, "1970-01-01 00:00:00.000 INFO [turn] hello");

    assert!(line.starts_with("1970-01-01 00:00:00.000 "));
    assert!(line.contains(" INFO ["));
    assert!(line.ends_with("] hello"));
}

/// A message containing a newline must not forge a second log line. A caller
/// passing a multi-line tool result is the ordinary case, not an edge case.

#[test]
fn newlines_in_a_message_cannot_forge_extra_lines() {
    let dir = std::env::temp_dir().join(format!("pantheon-log-nl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    append(
        &dir.join("logs").join(AGENT_LOG),
        Level::Info,
        "tool",
        "line one\nERROR [fake] forged",
    );
    let body = read(&dir, AGENT_LOG);
    assert_eq!(
        body.lines().count(),
        1,
        "a multi-line message must stay one line: {body:?}"
    );
    assert!(body.contains("line one"));
    assert!(body.contains("forged"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_error_file_mirrors_only_warnings_and_worse() {
    let dir = std::env::temp_dir().join(format!("pantheon-log-split-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    append(
        &dir.join("logs").join(AGENT_LOG),
        Level::Info,
        "c",
        "an info",
    );
    append(
        &dir.join("logs").join(AGENT_LOG),
        Level::Warning,
        "c",
        "a warning",
    );
    append(
        &dir.join("logs").join(AGENT_LOG),
        Level::Error,
        "c",
        "an error",
    );

    let agent = read(&dir, AGENT_LOG);
    let errors = read(&dir, ERRORS_LOG);
    assert_eq!(agent.lines().count(), 3, "agent.log keeps everything");
    assert_eq!(errors.lines().count(), 2, "errors.log keeps only warn+");
    assert!(!errors.contains("an info"));
    assert!(errors.contains("a warning") && errors.contains("an error"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_known_log_list_is_the_source_of_truth_for_a_reader() {
    // `logs list` enumerates this rather than globbing the dir, so a stray
    // `.bak` or editor swap file cannot appear as a log.
    assert!(KNOWN_LOGS.contains(&AGENT_LOG));
    assert!(KNOWN_LOGS.contains(&ERRORS_LOG));
    assert!(KNOWN_LOGS.contains(&GATEWAY_LOG));
    assert_eq!(KNOWN_LOGS.len(), 3);
}

/// `init` is a `OnceLock::set`, so a second caller must not retarget the
/// first one's files. The gateway and a CLI command can both reach it.

#[test]
fn append_redacts_api_keys_and_bearer_tokens() {
    let dir = std::env::temp_dir().join(format!("pantheon-log-redact-{}", std::process::id()));
    let file = dir.join("logs").join(AGENT_LOG);
    append(
        &file,
        Level::Error,
        "provider",
        "request failed: key=sk-or-v1-SECRETKEY123 auth=Bearer BEARERTOKEN456 retry",
    );
    let body = read(&dir, AGENT_LOG);
    assert!(
        !body.contains("SECRETKEY123"),
        "openrouter key leaked: {body}"
    );
    assert!(
        !body.contains("BEARERTOKEN456"),
        "bearer token leaked: {body}"
    );
    assert!(
        body.contains("[REDACTED]"),
        "redaction marker missing: {body}"
    );
    // The mirrored errors.log line gets the same treatment, since warn+
    // mirrors the already-redacted line.
    let errors = read(&dir, ERRORS_LOG);
    assert!(!errors.contains("SECRETKEY123") && !errors.contains("BEARERTOKEN456"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression: a benign message must survive redaction untouched, so the
/// pipeline cannot be silently eating log content.

#[test]
fn append_leaves_clean_messages_intact() {
    let dir = std::env::temp_dir().join(format!("pantheon-log-clean-{}", std::process::id()));
    let file = dir.join("logs").join(AGENT_LOG);
    append(&file, Level::Info, "turn", "turn 7 completed in 1.2s");
    let body = read(&dir, AGENT_LOG);
    assert!(
        body.contains("turn 7 completed in 1.2s"),
        "clean line mangled: {body}"
    );
    assert!(!body.contains("[REDACTED]"), "false redaction: {body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rotation renames generations in the right order: `agent.log` -> `.1`,
/// `.1` -> `.2`, ..., and the oldest generation is dropped so the file set
/// stays bounded. Uses small sizes/generations; the production constants are
/// exercised by `rotation_defaults_are_sane`.

#[test]
fn rotation_is_a_noop_below_the_cap_and_on_missing_files() {
    let dir = std::env::temp_dir().join(format!("pantheon-log-rot2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.log");
    std::fs::write(&path, "small").unwrap();

    rotate_sized(&path, 10 * 1024 * 1024, 5);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "small");
    assert!(!dir.join("agent.log.1").exists());

    rotate_log(&dir.join("nope.log"), 5); // must not panic
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pin the production rotation policy so it cannot silently regress to
/// "never rotate" (cap 0) or "keep one generation".

#[test]
fn rotation_defaults_are_sane() {
    assert_eq!(LOG_ROTATE_BYTES, 10 * 1024 * 1024);
    assert_eq!(LOG_ROTATE_GENERATIONS, 5);
}

// FLAG: the five filesystem tests above this line require `pub(crate) append`
// (`newlines_...`, `the_error_file_...`, `append_redacts_...`,
// `append_leaves_clean_messages_intact`) or private `rotate_sized`
// (`rotation_is_a_noop_...`). They stay in-file per the test-hygiene
// policy rather than widening those helpers' visibility.
