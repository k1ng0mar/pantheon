//! `pantheon logs` — read the runtime's log files.
//!
//! The runtime had no logs at all until `pantheon_api::logging` landed, so
//! this verb was briefly a run trace wearing the name of a log reader. It is
//! now what its name says: it reads `<data_dir>/logs/*.log`, and the run trace
//! is back to being a run trace.
//!
//! The shape follows `hermes_cli/logs.py` — named files, `--level`, `--follow`,
//! `--since`, `logs list` — because that reader is the thing being matched. Two
//! deliberate differences:
//!
//! - The file list is a closed set (`pantheon_api::logging::KNOWN_LOGS`) and
//!   not a `*.log` glob. A data dir also holds `ledger.db.<stamp>.bak` copies
//!   from `repair` and any editor swap file, all of which a glob would present
//!   as logs.
//! - Filtering is done by re-parsing each line rather than by keeping the
//!   files pre-split by level, so `--level errors` and `--level warning` on the
//!   same file are one code path.

use pantheon_api::logging::{self, Level, KNOWN_LOGS};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn usage() -> &'static str {
    concat!(
        "usage:\n",
        "  pantheon logs                       tail agent.log\n",
        "  pantheon logs list                  the log files and their sizes\n",
        "  pantheon logs <name>                tail one file (agent|errors|gateway)\n",
        "  \n",
        "  pantheon logs [name] [-n N]         last N lines (default 50)\n",
        "                     [-f]             follow, like tail -f\n",
        "                     [--level L]      debug|info|warning|error\n",
        "                     [--since 1h]     only records newer than 1s/1m/1h/1d\n",
        "                     [--grep TEXT]    only lines containing TEXT",
    )
}

/// A log file by the name a user types, resolved against the closed set.
fn resolve(name: &str) -> Result<PathBuf, String> {
    // Resolve the name BEFORE reading the global sink. The other order turns
    // every typo into "logging was not initialised", which sends the user
    // chasing a startup problem instead of their typo.
    let want = match name {
        "agent" => logging::AGENT_LOG,
        "errors" | "error" => logging::ERRORS_LOG,
        "gateway" => logging::GATEWAY_LOG,
        other => {
            return Err(format!(
                "unknown log '{other}'. known: agent, errors, gateway (or run `pantheon logs list`)"
            ))
        }
    };
    let dirs = logging::log_dir().ok_or_else(|| {
        "logging was not initialised, so there is no log directory yet".to_string()
    })?;
    Ok(dirs.join(want))
}

/// `1h`, `30m`, `2d`, `90s` into a cutoff. `None` when unparseable, which the
/// caller turns into an error rather than "no filter" — a silently ignored
/// `--since` is how someone ends up reading a whole file believing it is
/// recent.
fn parse_since(s: &str) -> Result<SystemTime, String> {
    let t = s.trim().to_ascii_lowercase();
    let (num, unit) = t.split_at(t.len().saturating_sub(1));
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("--since: '{s}' is not a duration like 30m, 2h, 1d"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => {
            return Err(format!(
                "--since: unknown unit '{unit}' (use s, m, h, or d)"
            ))
        }
    };
    Ok(SystemTime::now() - Duration::from_secs(secs))
}

struct Options {
    name: String,
    lines: usize,
    follow: bool,
    level: Option<Level>,
    since: Option<SystemTime>,
    grep: Option<String>,
}

pub fn cmd_logs(args: &[String]) {
    // `list` is a mode, not a file, so it is handled before option parsing.
    if args.get(2).map(String::as_str) == Some("list") {
        return list_files();
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return;
    }

    let mut name: Option<String> = None;
    let mut lines = 50usize;
    let mut follow = false;
    let mut level = None;
    let mut since = None;
    let mut grep = None;

    let mut i = 2;
    while i < args.len() {
        let a = args[i].as_str();
        // Each flag that takes a value refuses a missing one by name, rather
        // than consuming the next flag and failing somewhere unrelated.
        let mut need = |what: &str| -> String {
            i += 1;
            match args.get(i) {
                Some(v) => v.clone(),
                None => {
                    eprintln!("logs: {what} needs a value\n{}", usage());
                    std::process::exit(2);
                }
            }
        };
        match a {
            "-n" | "--lines" => {
                let v = need("-n");
                lines = v.parse().unwrap_or_else(|_| {
                    eprintln!("logs: -n expects a number, got '{v}'");
                    std::process::exit(2);
                });
            }
            "-f" | "--follow" => follow = true,
            "--level" => {
                let v = need("--level");
                level = Some(Level::parse(&v).unwrap_or_else(|| {
                    eprintln!("logs: unknown level '{v}'. use debug, info, warning, or error");
                    std::process::exit(2);
                }));
            }
            "--since" => {
                let v = need("--since");
                since = Some(parse_since(&v).unwrap_or_else(|e| {
                    eprintln!("logs: {e}");
                    std::process::exit(2);
                }));
            }
            "--grep" => grep = Some(need("--grep")),
            other if other.starts_with('-') => {
                eprintln!("logs: unknown flag '{other}'\n{}", usage());
                std::process::exit(2);
            }
            // The file name is the one bare positional. Tracking it as an
            // Option rather than defaulting to "agent" up front is what makes
            // `logs agent --level error` work: with a string default, naming
            // the default explicitly looked like a second argument and was
            // rejected, so the documented spelling of the default silently
            // did nothing.
            other => {
                if name.is_none() {
                    name = Some(other.to_string());
                } else {
                    eprintln!("logs: unexpected argument '{other}'\n{}", usage());
                    std::process::exit(2);
                }
            }
        }
        i += 1;
    }

    let name = name.unwrap_or_else(|| "agent".to_string());
    let path = resolve(&name).unwrap_or_else(|e| {
        eprintln!("logs: {e}");
        std::process::exit(2);
    });
    let opts = Options {
        name,
        lines,
        follow,
        level,
        since,
        grep,
    };
    if let Err(e) = tail(&path, &opts) {
        eprintln!("logs: {e}");
        std::process::exit(1);
    }
}

fn list_files() {
    let Some(dir) = logging::log_dir() else {
        println!("no log directory yet (logging was not initialised)");
        return;
    };
    let mut any = false;
    for name in KNOWN_LOGS {
        let p = dir.join(name);
        match std::fs::metadata(&p) {
            Ok(m) => {
                any = true;
                println!("{name:<14} {}", human_bytes(m.len()));
            }
            Err(_) => println!("{name:<14} (not created yet)"),
        }
    }
    if !any {
        println!(
            "\nno log files yet. they appear once a turn, a gateway poll, or a warning happens."
        );
    }
}

fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    if n < 1024 {
        format!("{n} B")
    } else if (n as f64) < K * K {
        format!("{:.1} KB", n as f64 / K)
    } else {
        format!("{:.1} MB", n as f64 / (K * K))
    }
}

/// Print matching lines, then optionally keep following.
///
/// A missing file is not an error: on a fresh install the logs directory does
/// not exist yet, and telling the user to run a turn is more useful than an
/// `ENOENT`.
fn tail(path: &Path, opts: &Options) -> Result<(), String> {
    if !path.exists() {
        println!(
            "no {} yet — it is created on the first turn or warning",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        return Ok(());
    }
    let mut offset = 0i64;
    emit_from(path, opts, &mut offset)?;

    if !opts.follow {
        return Ok(());
    }
    println!("— following {} (ctrl-c to stop) —", opts.name);
    let mut buf = String::new();
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let len = std::fs::metadata(path)
            .map(|m| m.len() as i64)
            .unwrap_or(offset);
        if len < offset {
            // Truncated or rotated under us. Restart from the beginning rather
            // than seeking past the end and printing nothing forever.
            offset = 0;
            buf.clear();
        }
        if len == offset {
            continue;
        }
        buf.clear();
        emit_from(path, opts, &mut offset)?;
    }
}

/// Copy new bytes from `offset` to the end, filtering each complete line.
fn emit_from(path: &Path, opts: &Options, offset: &mut i64) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if *offset > data.len() as i64 {
        *offset = 0;
    }
    let mut start = *offset as usize;
    // Only whole lines: a partial trailing line is a write in progress, and
    // printing half of it then the rest later would split one record in two.
    let last_nl = match data[start..].iter().rposition(|b| *b == b'\n') {
        Some(p) => start + p + 1,
        None => return Ok(()),
    };
    let chunk = &data[start..last_nl];
    start = last_nl;
    *offset = start as i64;

    let text = String::from_utf8_lossy(chunk);
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(l) = parse_level(line) {
            if let Some(min) = opts.level {
                if l < min {
                    continue;
                }
            }
        }
        if let Some(since) = opts.since {
            if let Some(ts) = parse_ts_ms(line) {
                if ts >= 0 && timestamp_of(ts) < since {
                    continue;
                }
            }
        }
        if let Some(g) = &opts.grep {
            if !line.contains(g.as_str()) {
                continue;
            }
        }
        kept.push(line);
    }
    let out = std::io::stdout();
    let mut w = out.lock();
    if opts.follow {
        for l in &kept {
            let _ = writeln!(w, "{l}");
        }
        let _ = w.flush();
    } else {
        // Only the tail, so `-n` is a real bound rather than "everything then
        // the last N", which on a long session prints a screenful nobody reads.
        let start = kept.len().saturating_sub(opts.lines);
        for l in &kept[start..] {
            let _ = writeln!(w, "{l}");
        }
    }
    Ok(())
}

/// A log timestamp as a `SystemTime`. Negative milliseconds (a pre-1970
/// record, which no writer emits) are clamped rather than wrapped, because
/// `Duration::from_millis` takes a `u64` and a cast would turn a small negative
/// into a far-future instant that passes every `--since` filter.
fn timestamp_of(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

/// Width of the timestamp field: `YYYY-MM-DD HH:MM:SS.mmm` is 23 characters.
/// Named as a constant because getting it wrong makes every filter a no-op
/// rather than an error — `line.get(20..)` lands inside the millisecond field
/// and the level parse silently fails, so `--level error` returns the whole
/// file and looks like the filter is broken rather than the parser.
const TS_WIDTH: usize = 23;

fn parse_level(line: &str) -> Option<Level> {
    // "TIMESTAMP LEVEL [component] message"
    let after_ts = line.get(TS_WIDTH..)?;
    let rest = after_ts.trim_start();
    [Level::Debug, Level::Info, Level::Warning, Level::Error]
        .into_iter()
        .find(|l| rest.starts_with(l.as_str()))
}

fn parse_ts_ms(line: &str) -> Option<i64> {
    // "YYYY-MM-DD HH:MM:SS.mmm" at the start of the line, parsed by fixed
    // offset. Splitting on whitespace instead looks equivalent and is not: a
    // `trim_start` on the remainder eats the separator and shifts every field
    // by one, which silently yields a wrong (or absent) timestamp.
    const D: &[std::ops::Range<usize>] = &[0..4, 5..7, 8..10];
    const T: &[std::ops::Range<usize>] = &[11..13, 14..16, 17..19];
    const MS: std::ops::Range<usize> = 20..23;
    let ts = line.get(..TS_WIDTH)?;
    let num = |r: std::ops::Range<usize>| -> Option<i64> { ts.get(r)?.parse().ok() };
    let (y, mo, d) = (num(D[0].clone())?, num(D[1].clone())?, num(D[2].clone())?);
    let (h, mi, sec) = (num(T[0].clone())?, num(T[1].clone())?, num(T[2].clone())?);
    let millis = num(MS)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000) + millis)
}

/// Inverse of the civil-from-days conversion in `logging`. Duplicated rather
/// than exported because the reader is the only consumer and a private helper
/// in the writer would be dead code there.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
#[path = "logs_cli_tests.rs"]
mod tests;
