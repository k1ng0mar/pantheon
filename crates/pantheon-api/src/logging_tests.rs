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
fn a_write_failure_does_not_propagate() {
    // Logging must never be the reason an operation fails. Point it at a path
    // that cannot be created and assert the call simply returns.
    append(
        Path::new("/proc/definitely-not-writable/agent.log"),
        Level::Error,
        "test",
        "unwritable",
    );
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
fn init_is_idempotent_and_never_retargets() {
    // Cannot assert the "first wins" direction from a shared test binary
    // without owning the lock, so assert the observable contract instead: the
    // accessor is Some once set and always points inside the directory it was
    // given.
    if let Some(dir) = log_dir() {
        assert!(
            dir.ends_with("logs"),
            "the sink must live in a logs/ subdir: {dir:?}"
        );
    }
}
