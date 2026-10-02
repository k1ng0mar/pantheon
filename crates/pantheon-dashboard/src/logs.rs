//! Log tailing over `<data_dir>/logs/*.log`.
//!
//! The files are the closed set from `pantheon_api::logging`
//! (`KNOWN_LOGS`), and the line format is its `"TIMESTAMP LEVEL
//! [component] message"` contract - parsed the same way the `pantheon
//! logs` reader parses it. `GET /api/logs` tails; `GET /api/logs/stream`
//! is Server-Sent Events over chunked encoding for live follow.

use crate::{bad_json, err_json, json_ok, query_usize, App};
use pantheon_api::logging::{self, Level};
use pantheon_gateway::http::{write_chunk, Request, Response};
use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Width of `YYYY-MM-DD HH:MM:SS.mmm`. Same fixed-offset parse as the
/// CLI reader: splitting on whitespace looks equivalent and is not.
const TS_WIDTH: usize = 23;
/// How far back the SSE stream starts: the last 64 KiB of the log, like
/// `tail` - enough context without dumping a huge file to the client.
const STREAM_TAIL_BYTES: u64 = 64 * 1024;
/// Poll interval for new log lines on the SSE stream.
const STREAM_POLL: Duration = Duration::from_millis(500);
/// Hard cap on one SSE stream's lifetime; clients reconnect for longer
/// follows so a forgotten tab cannot hold a connection forever.
const STREAM_MAX_AGE: Duration = Duration::from_secs(600);

struct Filter {
    level: Option<Level>,
    grep: Option<String>,
    since_ms: Option<i64>,
}

fn parse_level(line: &str) -> Option<Level> {
    let after_ts = line.get(TS_WIDTH..)?;
    let rest = after_ts.trim_start();
    [Level::Debug, Level::Info, Level::Warning, Level::Error]
        .into_iter()
        .find(|l| rest.starts_with(l.as_str()))
}

/// Millis since epoch from the fixed-width timestamp prefix.
fn parse_ts_ms(line: &str) -> Option<i64> {
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
    Some(((days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec) * 1000) + millis)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn keep(line: &str, f: &Filter) -> bool {
    if let Some(min) = f.level {
        // Lines without a parseable level are kept: dropping them would
        // hide multi-line continuations and foreign writes.
        if let Some(l) = parse_level(line) {
            if (l as u8) < (min as u8) {
                return false;
            }
        }
    }
    if let Some(since) = f.since_ms {
        if let Some(ts) = parse_ts_ms(line) {
            if ts < since {
                return false;
            }
        }
    }
    if let Some(g) = &f.grep {
        if !line.contains(g.as_str()) {
            return false;
        }
    }
    true
}

fn resolve_source(source: &str) -> Result<&'static str, String> {
    match source {
        "agent" => Ok(logging::AGENT_LOG),
        "errors" | "error" => Ok(logging::ERRORS_LOG),
        "gateway" => Ok(logging::GATEWAY_LOG),
        other => Err(format!(
            "unknown log '{other}'; known: agent, errors, gateway"
        )),
    }
}

fn log_path(app: &App, source: &str) -> Result<Option<PathBuf>, String> {
    let file = resolve_source(source)?;
    let Some(dir) = logging::log_dir() else {
        return Ok(None);
    };
    // Belt and suspenders: the file name comes from the closed KNOWN_LOGS
    // set, but never let a path escape the log dir.
    let p = dir.join(file);
    if !p.starts_with(&dir) {
        return Err("invalid log path".to_string());
    }
    let _ = app;
    Ok(Some(p))
}

fn parse_filter(req: &Request) -> Result<Filter, Response> {
    let level = match req.query.get("level") {
        Some(s) if !s.is_empty() => match Level::parse(s) {
            Some(l) => Some(l),
            None => return Err(bad_json("level must be debug|info|warning|error")),
        },
        _ => None,
    };
    let since_ms = match req.query.get("since") {
        Some(s) if !s.is_empty() => match parse_since(s) {
            Some(ms) => Some(ms),
            None => return Err(bad_json("since must be like 30m, 2h, 1d")),
        },
        _ => None,
    };
    Ok(Filter {
        level,
        grep: req.query.get("grep").cloned().filter(|s| !s.is_empty()),
        since_ms,
    })
}

fn parse_since(s: &str) -> Option<i64> {
    let t = s.trim().to_lowercase();
    let (num, unit) = t.split_at(t.len().saturating_sub(1));
    let n: i64 = num.trim().parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return None,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some(now - secs * 1000)
}

/// Read the whole file, keep matching lines, return the last `tail`.
fn tail_lines(path: &PathBuf, f: &Filter, tail: usize) -> Result<Vec<String>, String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?
        .read_to_string(&mut text)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let kept: Vec<String> = text
        .lines()
        .filter(|l| keep(l, f))
        .map(str::to_string)
        .collect();
    let start = kept.len().saturating_sub(tail);
    Ok(kept[start..].to_vec())
}

/// `GET /api/logs?source=agent|errors|gateway&level=&tail=200&grep=&since=`
pub fn tail(app: &App, req: &Request) -> Response {
    let source = req
        .query
        .get("source")
        .map(String::as_str)
        .unwrap_or("agent");
    let filter = match parse_filter(req) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let tail_n = query_usize(&req.query, "tail", 200).min(5000);
    let path = match log_path(app, source) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return json_ok(serde_json::json!({
                "source": source, "lines": [], "note": "no log directory yet",
            }))
        }
        Err(e) => return bad_json(&e),
    };
    if !path.exists() {
        return json_ok(serde_json::json!({
            "source": source, "lines": [],
            "note": "log file not created yet. It appears on the first turn or warning",
        }));
    }
    match tail_lines(&path, &filter, tail_n) {
        Ok(lines) => json_ok(serde_json::json!({"source": source, "lines": lines})),
        Err(e) => err_json(500, "LOGS", &e),
    }
}

/// `GET /api/logs/stream?source=&level=`: SSE follow. Sends the current
/// tail first, then new matching lines every 500ms, for up to 10 minutes
/// or until the client disconnects (a write error ends the stream).
pub fn stream(app: &App, req: &Request) -> Response {
    let source = req
        .query
        .get("source")
        .map(String::as_str)
        .unwrap_or("agent")
        .to_string();
    let filter = match parse_filter(req) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let path = match log_path(app, &source) {
        Ok(Some(p)) => p,
        Ok(None) => return bad_json("no log directory yet"),
        Err(e) => return bad_json(&e),
    };
    let token_note = format!("following {source}");
    let run = move |sock: &TcpStream| {
        // Best-effort: every write error means the client went away.
        let send = |sock: &TcpStream, payload: &str| -> bool {
            let frame = format!("data: {}\n\n", payload);
            write_chunk(sock, frame.as_bytes()).is_ok()
        };
        let _ = send(sock, &serde_json::json!({"note": token_note}).to_string());
        let mut offset: u64 = 0;
        // Start at the tail: like the CLI's follow, we show what's there
        // and then stream.
        if let Ok(meta) = std::fs::metadata(&path) {
            offset = meta.len().saturating_sub(STREAM_TAIL_BYTES);
        }
        let mut pending = String::new();
        let deadline = std::time::Instant::now() + STREAM_MAX_AGE;
        loop {
            if std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(STREAM_POLL);
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(offset);
            if len < offset {
                offset = 0; // truncated/rotated under us
                pending.clear();
            }
            if len == offset {
                // Heartbeat so proxies don't kill an idle stream.
                let _ = write_chunk(sock, b": ping\n\n");
                continue;
            }
            let mut f = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            use std::io::Seek;
            if f.seek(std::io::SeekFrom::Start(offset)).is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_err() {
                continue;
            }
            offset = len;
            pending.push_str(&String::from_utf8_lossy(&buf));
            // Only whole lines: a partial trailing line is a write in
            // progress.
            let upto = match pending.rfind('\n') {
                Some(p) => p + 1,
                None => continue,
            };
            let chunk: String = pending.drain(..upto).collect();
            for line in chunk.lines() {
                if !keep(line, &filter) {
                    continue;
                }
                let payload = serde_json::json!({"line": line}).to_string();
                if !send(sock, &payload) {
                    return;
                }
            }
        }
    };
    Response::ChunkedStream {
        content_type: "text/event-stream",
        stream: Box::new(run),
    }
}
