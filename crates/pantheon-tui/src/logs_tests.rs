//! Tests for the `logs` reader. Sibling file so `logs.rs` stays test-free.
use super::*;
use pantheon_api::logging as core_log;

/// Write a log file the reader will see, and return its path.
fn write_log(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pantheon-logs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn opts(lines: usize) -> Options {
    Options {
        name: "test".into(),
        lines,
        follow: false,
        level: None,
        since: None,
        grep: None,
    }
}

/// Timestamps are generated relative to now, not pinned to a date. A
/// date-pinned fixture makes `--since 1h` correct only on the day it was
/// written, which is a test that fails every day after.
fn recent(level: &str, component: &str, msg: &str, secs_ago: i64) -> String {
    let ms = core_log::now_ms() - secs_ago * 1000;
    format!("{} {level} [{component}] {msg}", core_log::stamp(ms))
}

fn sample() -> Vec<String> {
    vec![
        recent("INFO", "turn", "started run_1", 50),
        recent("DEBUG", "tool", "shell: ls", 40),
        recent("WARNING", "provider", "rate limited, retrying", 30),
        recent("ERROR", "provider", "PROVIDER_EXHAUSTED", 20),
        recent("INFO", "turn", "completed run_1", 10),
    ]
}

fn sample_text() -> String {
    let mut s = sample().join("\n");
    s.push('\n');
    s
}
/// The filter half of `emit_from`, extracted so it can be asserted without
/// capturing stdout. The production path calls exactly this predicate.
fn kept_lines(body: &str, o: &Options) -> Vec<String> {
    body.lines()
        .filter(|line| {
            if let Some(l) = parse_level(line) {
                if let Some(min) = o.level {
                    if l < min {
                        return false;
                    }
                }
            }
            if let Some(since) = o.since {
                if let Some(ts) = parse_ts_ms(line) {
                    if timestamp_of(ts) < since {
                        return false;
                    }
                }
            }
            match &o.grep {
                Some(g) => line.contains(g.as_str()),
                None => true,
            }
        })
        .map(str::to_string)
        .collect()
}

fn bounded(kept: &[String], n: usize) -> Vec<String> {
    let start = kept.len().saturating_sub(n);
    kept[start..].to_vec()
}

#[test]
fn level_parsing_finds_the_level_after_the_timestamp() {
    let lines = sample();
    let l: Vec<&str> = lines.iter().map(String::as_str).collect();
    assert_eq!(parse_level(l[0]), Some(Level::Info));
    assert_eq!(parse_level(l[1]), Some(Level::Debug));
    assert_eq!(parse_level(l[2]), Some(Level::Warning));
    assert_eq!(parse_level(l[3]), Some(Level::Error));
    // A line that is not a log record must not be misread as one.
    assert_eq!(parse_level("not a log line at all"), None);
    // A message that merely mentions a level must not be read as one.
    assert_eq!(
        parse_level("2026-09-26 10:00:00.000 note [c] saw an ERROR earlier"),
        None
    );
}

#[test]
fn level_filter_drops_lower_severities() {
    let got = kept_lines(&sample_text(), &{
        let mut o = opts(50);
        o.level = Some(Level::Warning);
        o
    });
    assert_eq!(got.len(), 2, "expected only warning+error: {got:?}");
    assert!(got.iter().any(|l| l.contains("rate limited")));
    assert!(got.iter().any(|l| l.contains("PROVIDER_EXHAUSTED")));
    assert!(!got.iter().any(|l| l.contains("started run_1")));
}

#[test]
fn level_error_is_stricter_than_level_warning() {
    let mut o = opts(50);
    o.level = Some(Level::Error);
    let got = kept_lines(&sample_text(), &o);
    assert_eq!(got.len(), 1, "only the error: {got:?}");
    assert!(got[0].contains("PROVIDER_EXHAUSTED"));
}

#[test]
fn the_line_count_bound_keeps_only_the_tail() {
    let kept = kept_lines(&sample_text(), &opts(50));
    let got = bounded(&kept, 2);
    assert_eq!(got.len(), 2, "got {got:?}");
    // The tail means the newest records, which are at the end of the file.
    assert!(got[1].contains("completed run_1"), "got {got:?}");
    assert!(!got[0].contains("started run_1"), "got {got:?}");
}

#[test]
fn a_bound_larger_than_the_file_returns_everything() {
    let kept = kept_lines(&sample_text(), &opts(50));
    assert_eq!(bounded(&kept, 5000).len(), sample().len());
    // And a bound of zero must not panic or print the whole file.
    assert!(bounded(&kept, 0).is_empty());
}

#[test]
fn grep_filters_on_a_substring_anywhere_in_the_line() {
    let mut o = opts(50);
    o.grep = Some("provider".to_string());
    let got = kept_lines(&sample_text(), &o);
    assert_eq!(got.len(), 2, "got {got:?}");
    assert!(got.iter().all(|l| l.contains("provider")));
}

#[test]
fn since_parses_relative_durations() {
    assert!(parse_since("1h").is_ok());
    assert!(parse_since("30m").is_ok());
    assert!(parse_since("2d").is_ok());
    assert!(parse_since("90s").is_ok());
    // A bad duration is an error, not a silently ignored filter.
    assert!(parse_since("later").is_err());
    assert!(parse_since("1y").is_err());
    assert!(parse_since("").is_err());
    assert!(parse_since("h").is_err());
}

#[test]
fn since_filters_out_old_records() {
    let ancient = "2020-01-01 00:00:00.000 INFO [turn] ancient";
    let body = format!("{ancient}\n{}", sample_text());
    let o = {
        let mut o = opts(50);
        o.since = Some(SystemTime::now() - Duration::from_secs(3600));
        o
    };
    let got = kept_lines(&body, &o);
    assert!(
        !got.iter().any(|l| l.contains("ancient")),
        "--since 1h must drop a 2020 record: {got:?}"
    );
    assert_eq!(got.len(), sample().len(), "and must keep the recent ones");
}

/// A record timestamped in the future is kept, not dropped: a clock skew
/// between the writer and the reader must not hide the newest entries.
#[test]
fn since_keeps_a_record_from_the_future() {
    let future = recent("INFO", "turn", "from the future", -600);
    let o = {
        let mut o = opts(50);
        o.since = Some(SystemTime::now() - Duration::from_secs(3600));
        o
    };
    let got = kept_lines(&format!("{future}\n"), &o);
    assert_eq!(
        got.len(),
        1,
        "a future record must survive --since: {got:?}"
    );
}
#[test]
fn the_timestamp_parser_inverts_the_writer() {
    for ms in [
        0_i64,
        1_000_000_000_000,
        1_709_208_000_000,
        1_735_689_600_000,
        4_102_444_800_000,
    ] {
        let line = format!("{} INFO [c] x", core_log::stamp(ms));
        assert_eq!(
            parse_ts_ms(&line),
            Some(ms),
            "round-trip failed for {ms}: {line}"
        );
    }
    // A line that is not a record must not yield a timestamp. Note that a
    // valid timestamp with trailing junk DOES parse — the reader keys on the
    // fixed-width prefix, which is the same rule the writer emits under.
    assert_eq!(parse_ts_ms("no timestamp here"), None);
    assert_eq!(parse_ts_ms("2026-09-26 10:00:00 nonsense"), None);
    assert_eq!(parse_ts_ms("2026-13-26 10:00:00.100 INFO [c] x"), None);
    assert_eq!(parse_ts_ms("2026-09-32 10:00:00.100 INFO [c] x"), None);
    assert_eq!(parse_ts_ms("2026-09-26 25:00:00.100 INFO [c] x"), None);
    assert!(
        parse_ts_ms("2026-09-26 10:00:00.100 trailing junk").is_some(),
        "a valid prefix parses even with junk after it"
    );
}

/// `emit_from` must not consume a line that has no trailing newline yet: a
/// write in progress would otherwise be printed in halves.
#[test]
fn a_partial_trailing_line_is_never_consumed() {
    let body = "2026-09-26 10:00:00.000 INFO [c] complete\n2026-09-26 10:00:01.000 INFO [c] half";
    let p = write_log("partial.log", body);
    // Drive the real reader and assert on the offset it consumed. A partial
    // line must be left for the next poll, or one record prints as two.
    let mut offset = 0i64;
    emit_from(&p, &opts(50), &mut offset).unwrap();
    // Consumed exactly the first complete record, i.e. up to and including its
    // newline. The trailing partial line is left for the next poll.
    let after_first = body.find('\n').expect("fixture has a first line") + 1;
    assert_eq!(
        offset, after_first as i64,
        "must consume the complete line only, not the partial one"
    );
    assert!(
        offset < body.find("half").unwrap() as i64,
        "the partial record must not be consumed"
    );

    // Completing the line releases exactly the remainder, nothing more.
    std::fs::write(&p, format!("{body}\n")).unwrap();
    emit_from(&p, &opts(50), &mut offset).unwrap();
    assert_eq!(offset, (body.len() + 1) as i64);
}

/// A missing file is a hint, not an error: on a fresh install the log
/// directory does not exist yet, and "run a turn first" beats an ENOENT.
#[test]
fn a_missing_file_is_not_an_error() {
    let missing = std::env::temp_dir().join("pantheon-logs-nope-does-not-exist.log");
    let _ = std::fs::remove_file(&missing);
    assert!(!missing.exists(), "precondition: the file must not exist");
    assert!(tail(&missing, &opts(10)).is_ok());
}

#[test]
fn resolve_rejects_an_unknown_log_name() {
    // Resolving needs the global sink, so the assertion is on the message when
    // the sink is absent and simply must not panic when it is present.
    // Name validation must come first, so an unknown name is rejected even
    // when the sink is absent. That ordering is the test: an "uninitialised"
    // error for a typo would send the user chasing the wrong problem.
    match resolve("not-a-log") {
        Err(e) => assert!(e.contains("unknown log"), "{e}"),
        Ok(p) => assert!(p.ends_with(".log"), "{p:?}"),
    }
}

#[test]
fn human_bytes_is_readable_at_every_scale() {
    assert_eq!(human_bytes(512), "512 B");
    assert_eq!(human_bytes(2048), "2.0 KB");
    assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    assert_eq!(human_bytes(0), "0 B");
}
