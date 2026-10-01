//! Lenient HTTP fetching for servers and networks with sloppy TLS.
//!
//! Two layers of tolerance:
//!
//! 1. **Unclean TLS shutdowns.** Some servers close the TCP connection
//!    without sending a TLS `close_notify` alert. rustls treats that as
//!    an `UnexpectedEof` I/O error once the buffered plaintext is
//!    consumed; curl and OpenSSL accept the bytes. Since every byte we
//!    keep arrived in an authenticated TLS record — only the *shutdown*
//!    was sloppy — we accept the body when the framing says it is
//!    already whole, and keep failing hard on genuine truncation.
//!
//! 2. **curl fallback.** Some egress proxies drop rustls handshakes
//!    while OpenSSL succeeds (observed: every HTTPS host through a
//!    filtering proxy fails pre-status-line with ureq, while curl
//!    returns 200). rustls exposes no knob for this — there is no
//!    response to salvage — so on a transport-level ureq failure we
//!    retry once via the `curl` CLI (argv-only, no shell) with the same
//!    timeout/cap posture. HTTP error statuses never trigger it.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Outcome of an HTTP GET: status plus the body (already cap-checked).
#[derive(Debug)]
pub struct Fetched {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Crate-neutral GET failure.
#[derive(Debug)]
pub enum GetError {
    /// The server answered with a non-2xx status.
    Status(u16),
    /// Transport failure on every attempt (message already cleaned).
    Transport(String),
    TooLarge,
}

/// GET `url` with `timeout` and a `max_bytes` body cap.
///
/// Primary path is ureq with the tolerant body reader; on a
/// transport-level ureq failure it falls back once to `curl`
/// ([`curl_get`]). HTTP error statuses and oversize bodies never
/// trigger the fallback.
pub fn get(
    url: &str,
    timeout: Duration,
    max_bytes: u64,
    user_agent: &str,
) -> Result<Fetched, GetError> {
    match get_ureq(url, timeout, max_bytes, user_agent) {
        ok @ Ok(_) => ok,
        err @ Err(GetError::Status(_)) => err,
        err @ Err(GetError::TooLarge) => err,
        Err(GetError::Transport(raw)) => match curl_get(url, timeout, max_bytes, user_agent) {
            Some(r) => r,
            // curl unavailable: report the original ureq failure, cleaned.
            None => Err(GetError::Transport(fetch_error_message(url, &raw))),
        },
    }
}

/// GET via ureq only — no curl fallback. Used by callers that must not
/// inherit the fallback (e.g. paths that need the raw ureq response
/// handling). Most callers want [`get`].
fn get_ureq(
    url: &str,
    timeout: Duration,
    max_bytes: u64,
    user_agent: &str,
) -> Result<Fetched, GetError> {
    let resp = ureq::get(url)
        .timeout(timeout)
        .set("User-Agent", user_agent)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => GetError::Status(code),
            ureq::Error::Transport(t) => GetError::Transport(t.to_string()),
        })?;
    let status = resp.status();
    let body = read_body_capped(resp, max_bytes).map_err(|e| GetError::Transport(e.to_string()))?;
    if body.len() as u64 > max_bytes {
        return Err(GetError::TooLarge);
    }
    Ok(Fetched { status, body })
}

/// Fallback GET via the `curl` CLI: argv-only (no shell), the URL as a
/// single argv after `--`. Returns `None` when curl is not installed so
/// the caller can report the original failure instead.
fn curl_get(
    url: &str,
    timeout: Duration,
    max_bytes: u64,
    user_agent: &str,
) -> Option<Result<Fetched, GetError>> {
    let curl = find_curl()?;
    let seq = CURL_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tag = format!("pantheon-curl-{}-{seq}", std::process::id());
    let dir = std::env::temp_dir();
    let hdr_path = dir.join(format!("{tag}.hdr"));
    let body_path = dir.join(format!("{tag}.body"));
    let out = std::process::Command::new(&curl)
        .args([
            "-sS",
            "-L",
            "--max-redirs",
            "5",
            "--max-time",
            &timeout.as_secs().max(1).to_string(),
            "--max-filesize",
            &(max_bytes + 1).to_string(),
            "-A",
            user_agent,
            "--dump-header",
            hdr_path.to_str()?,
            "--output",
            body_path.to_str()?,
            "--",
            url,
        ])
        .output()
        .ok();
    // Always clean up the temp files, success or not.
    let hdr = std::fs::read(&hdr_path).unwrap_or_default();
    let _ = std::fs::remove_file(&hdr_path);
    let body = std::fs::read(&body_path).unwrap_or_default();
    let _ = std::fs::remove_file(&body_path);
    let out = out?;
    if out.status.code() == Some(63) {
        // CURLE_FILESIZE_EXCEEDED: the cap tripped inside curl.
        return Some(Err(GetError::TooLarge));
    }
    let status = parse_status(&hdr);
    match (out.status.success(), status) {
        (true, Some(code @ 200..=299)) => {
            if body.len() as u64 > max_bytes {
                Some(Err(GetError::TooLarge))
            } else {
                Some(Ok(Fetched { status: code, body }))
            }
        }
        (_, Some(code)) => Some(Err(GetError::Status(code))),
        _ => {
            let detail = String::from_utf8_lossy(&out.stderr);
            let detail: String = detail
                .trim()
                .lines()
                .last()
                .unwrap_or("curl failed")
                .chars()
                .take(200)
                .collect();
            Some(Err(GetError::Transport(fetch_error_message(url, &detail))))
        }
    }
}

static CURL_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Locate the `curl` binary on PATH.
fn find_curl() -> Option<PathBuf> {
    let name = if cfg!(windows) { "curl.exe" } else { "curl" };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let p = dir.join(name);
            if p.is_file() {
                Some(p)
            } else {
                None
            }
        })
    })
}

/// Final status code from a `--dump-header` file: the last `HTTP/x`
/// status line wins (redirect hops come first).
fn parse_status(hdr: &[u8]) -> Option<u16> {
    String::from_utf8_lossy(hdr)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(proto), Some(code)) if proto.starts_with("HTTP/") => code.parse().ok(),
                _ => None,
            }
        })
        .last()
}

/// True when `msg` is rustls's "peer closed connection without sending
/// TLS close_notify" text (surfaced through ureq as a transport error).
/// rustls exposes no structured variant for this; the `close_notify`
/// marker is the stable part of its `UNEXPECTED_EOF_MESSAGE`.
pub fn is_unclean_tls_close_text(msg: &str) -> bool {
    msg.contains("close_notify")
}

fn is_unclean_tls_close(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::UnexpectedEof && is_unclean_tls_close_text(&e.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// Declared byte count; EOF before it arrives is truncation.
    Length(u64),
    /// Terminal chunk ends the body; an unclean close means the
    /// terminal chunk never arrived.
    Chunked,
    /// EOF is the only terminator.
    CloseDelimited,
}

fn framing(content_length: Option<&str>, transfer_encoding: Option<&str>) -> Framing {
    if let Some(len) = content_length.and_then(|v| v.parse::<u64>().ok()) {
        return Framing::Length(len);
    }
    let chunked = transfer_encoding
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    if chunked {
        Framing::Chunked
    } else {
        Framing::CloseDelimited
    }
}

/// Decide whether an unclean TLS shutdown (TCP EOF without
/// `close_notify`) is benign: only when the framing says the body is
/// already whole. Pure decision logic, unit-tested.
fn teardown_is_benign(framing: Framing, received: u64) -> bool {
    match framing {
        Framing::Chunked => false,
        Framing::Length(len) => received == len,
        Framing::CloseDelimited => received > 0,
    }
}

/// Read a response body, capped at `max_bytes + 1` bytes (the caller
/// decides what "too large" means).
///
/// Tolerates an unclean TLS shutdown when the framing says the body is
/// already whole (close-delimited bodies with bytes received, or a
/// fully-received declared length). Anything else — zero bytes, a short
/// length-framed body, a chunked body — still errors: that is genuine
/// truncation, not server sloppiness.
pub fn read_body_capped(resp: ureq::Response, max_bytes: u64) -> io::Result<Vec<u8>> {
    let framing = framing(
        resp.header("content-length"),
        resp.header("transfer-encoding"),
    );
    read_capped_tolerant(resp.into_reader(), max_bytes, framing)
}

fn read_capped_tolerant<R: Read>(
    reader: R,
    max_bytes: u64,
    framing: Framing,
) -> io::Result<Vec<u8>> {
    let mut reader = reader.take(max_bytes + 1);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if is_unclean_tls_close(&e) && teardown_is_benign(framing, buf.len() as u64) => {
                break
            }
            Err(e) => return Err(e),
        }
    }
    Ok(buf)
}

/// User-facing message for a failed fetch. An unclean TLS shutdown gets a
/// plain-English explanation instead of leaking rustls's internal doc
/// link into API error bodies; anything else passes through unchanged.
pub fn fetch_error_message(what: &str, err: &str) -> String {
    if is_unclean_tls_close_text(err) {
        format!(
            "{what}: the server closed the connection without a clean TLS shutdown \
             (it skipped the TLS closing handshake — a server-side quirk); \
             no complete response was received"
        )
    } else {
        format!("{what}: {err}")
    }
}
