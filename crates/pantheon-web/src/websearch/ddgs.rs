//! DuckDuckGo metasearch via the local `ddgs` binary: shells out to
//! `ddgs text -q <query> -m <n> -o <owned tmp file>` and parses the
//! `[{title,href,body}]` JSON it writes. No key, but the binary has to be
//! installed (setup wizard detects it and offers to install). Fragile —
//! selectors and rate limits shift without notice — so fallback, not default.
//!
//! Temp file is in the OS temp dir under a pid+nanos+counter name, removed
//! after parsing regardless of outcome.

use super::error::SearchError;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// binary we shell out to
pub const DDGS_BINARY: &str = "ddgs";

pub struct DdgsProvider;

impl DdgsProvider {
    /// uses `ddgs` from `PATH`
    pub fn new() -> Self {
        Self
    }
}

impl Default for DdgsProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// one owned temp path per run. Pid+nanos alone collided for concurrent
/// same-nanos calls in one process, mixing two searches' outputs in one file.
fn tmp_path() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "pantheon-ddgs-{}-{}-{}.json",
        std::process::id(),
        nanos,
        n
    ))
}

impl SearchProvider for DdgsProvider {
    fn name(&self) -> &str {
        "ddgs"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let out = tmp_path();
        // `output()` blocks forever; spawn + try_wait gives ddgs the same
        // 30s bound the HTTP providers get instead of hanging the tool call
        // on a wedged binary
        let mut child = std::process::Command::new(DDGS_BINARY)
            .args([
                "text",
                "-q",
                query,
                "-m",
                &opts.max_results.max(1).to_string(),
                "-o",
                &out.display().to_string(),
                "-nc",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                SearchError::RequestFailed(format!(
                    "ddgs binary not runnable ({e}); install it with `pip install ddgs`"
                ))
            })?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let output = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut stderr = Vec::new();
                    if let Some(mut se) = child.stderr.take() {
                        use std::io::Read;
                        let _ = se.read_to_end(&mut stderr);
                    }
                    break std::process::Output {
                        status,
                        stdout: Vec::new(),
                        stderr,
                    };
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = std::fs::remove_file(&out);
                        return Err(SearchError::Timeout);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    return Err(SearchError::RequestFailed(format!("ddgs wait failed: {e}")));
                }
            }
        };
        // the output file is ours: remove it whether we succeeded or not
        let text = std::fs::read_to_string(&out);
        let _ = std::fs::remove_file(&out);
        if !output.status.success() {
            // surface stderr (truncated): hiding it loses the rate-limit /
            // bot-block reason entirely
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.chars().take(300).collect::<String>())
            };
            return Err(SearchError::RequestFailed(format!(
                "ddgs exited {}{}",
                output.status.code().unwrap_or(-1),
                detail
            )));
        }
        let text = text.map_err(|e| {
            SearchError::BadResponse(format!("ddgs wrote no parseable output: {e}"))
        })?;
        parse_ddgs_response(&text)
    }
}

/// Parse the `[{title,href,body}]` JSON. Skip hits without a usable `href`;
/// non-array envelope -> `BadResponse`.
pub fn parse_ddgs_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let results = v
        .as_array()
        .ok_or_else(|| SearchError::BadResponse("response is not a JSON array".to_string()))?;
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let Some(url) = opt_str(r, "href") else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        out.push(SearchResult {
            title: opt_str(r, "title").unwrap_or_default(),
            url,
            snippet: opt_str(r, "body").unwrap_or_default(),
            published: None,
            score: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ddgs_json_file_shape() {
        let body = r#"[
            {"title":"Rust - Wikipedia","href":"https://en.wikipedia.org/wiki/Rust","body":"a language"},
            {"title":"no href here","body":"skip"},
            {"title":"empty","href":"  ","body":"skip"}
        ]"#;
        let hits = parse_ddgs_response(body).expect("parse");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://en.wikipedia.org/wiki/Rust");
        assert_eq!(hits[0].snippet, "a language");
    }

    #[test]
    fn non_array_is_bad_response() {
        assert!(parse_ddgs_response(r#"{"results":[]}"#).is_err());
    }
}

#[cfg(test)]
mod tmp_path_tests {
    use super::*;

    #[test]
    fn tmp_paths_are_unique_across_rapid_calls() {
        let a = tmp_path();
        let b = tmp_path();
        assert_ne!(a, b, "same-nanos calls must not share a tmp file");
    }
}
