//! Curated secret-pattern scan for the memory write path.
//!
//! No regex dependency: the patterns are simple substring/assignment
//! shapes matched by hand over ASCII-lowercased bytes. Fail-closed: any
//! hit refuses the write with `WriteRefusal::SecretDetected`, naming the
//! *class* of pattern ("labeled password", "github token prefix"), never
//! the matched text.
//!
//! The patterns are deliberately narrow so prose *about* secrets does not
//! trip the scan:
//! - labeled assignments: `password`, `api_key`, … followed by `=`/`:` and
//!   a non-empty value (`"the password policy requires rotation"` has no
//!   assignment and passes; `"password = hunter2"` fails);
//! - known token prefixes (`sk-`, `ghp_`, `AKIA`, …) followed by at least
//!   8 opaque characters (`"task-list"` does not count as `sk-`);
//! - PEM private-key blocks.
//!
//! This is a tripwire, not a vault: it catches the common accident (a
//! pasted key, "remember this password: …"), not a determined exfil.

/// (lowercase label, pattern class)
const LABELED: &[(&str, &str)] = &[
    ("password", "labeled password"),
    ("passwd", "labeled password"),
    ("api_key", "labeled api key"),
    ("apikey", "labeled api key"),
    ("api-key", "labeled api key"),
    ("client_secret", "labeled client secret"),
    ("secret", "labeled secret"),
    ("access_token", "labeled access token"),
    ("auth_token", "labeled auth token"),
    ("token", "labeled token"),
    ("bearer", "bearer token"),
    ("private_key", "private key material"),
];

/// (lowercase prefix, pattern class)
const PREFIXED: &[(&str, &str)] = &[
    ("sk-", "api key prefix"),
    ("ghp_", "github token prefix"),
    ("gho_", "github token prefix"),
    ("ghu_", "github token prefix"),
    ("gsk_", "api key prefix"),
    ("xoxb-", "slack token prefix"),
    ("xoxp-", "slack token prefix"),
    ("xoxa-", "slack token prefix"),
    ("akia", "aws key id prefix"),
    ("aiza", "google api key prefix"),
    ("-----begin private key-----", "private key block"),
    ("-----begin rsa private key-----", "private key block"),
    ("-----begin openssh private key-----", "private key block"),
    ("-----begin ec private key-----", "private key block"),
];

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `label` at `j` (end of label) is followed by `=`/`:` and a non-empty
/// value, skipping whitespace/quotes/brackets. `password = ""` and
/// `{"password": ""}` do not count; `password=hunter2` does.
fn assignment_follows(lower: &[u8], mut j: usize) -> bool {
    while j < lower.len() && matches!(lower[j], b' ' | b'\t' | b'"' | b'\'' | b'[' | b'(') {
        j += 1;
    }
    if j >= lower.len() || !matches!(lower[j], b'=' | b':') {
        return false;
    }
    j += 1;
    while j < lower.len() && matches!(lower[j], b' ' | b'\t' | b'"' | b'\'') {
        j += 1;
    }
    if j >= lower.len() {
        return false;
    }
    !matches!(
        lower[j],
        b'"' | b'\'' | b'}' | b']' | b')' | b',' | b'\n' | b'\r'
    )
}

fn labeled_hit(lower: &[u8]) -> Option<&'static str> {
    for (label, class) in LABELED {
        let lb = label.as_bytes();
        let mut start = 0;
        while let Some(pos) = find_sub(&lower[start..], lb) {
            let i = start + pos;
            let left_ok = i == 0 || !is_word(lower[i - 1]);
            let j = i + lb.len();
            let right_ok = j >= lower.len() || !is_word(lower[j]);
            if left_ok && right_ok && assignment_follows(lower, j) {
                return Some(class);
            }
            start = i + 1;
        }
    }
    None
}

/// Token prefixes need a left word boundary (so `task-list` is not `sk-`)
/// and at least 8 opaque characters after the prefix. PEM blocks are
/// distinctive enough to match bare.
fn prefixed_hit(lower: &[u8]) -> Option<&'static str> {
    for (prefix, class) in PREFIXED {
        let pb = prefix.as_bytes();
        let bare = pb.starts_with(b"-----");
        let mut start = 0;
        while let Some(pos) = find_sub(&lower[start..], pb) {
            let i = start + pos;
            if bare {
                return Some(class);
            }
            let left_ok = i == 0 || !lower[i - 1].is_ascii_alphanumeric();
            if left_ok {
                let tail = &lower[i + pb.len()..];
                let n = tail
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_' || **b == b'-')
                    .count();
                if n >= 8 {
                    return Some(class);
                }
            }
            start = i + 1;
        }
    }
    None
}

/// Scan `text` for secret patterns. Returns the matched pattern *class*
/// on a hit, `None` when clean.
pub(crate) fn detect_secret(text: &str) -> Option<&'static str> {
    // ASCII-lowercased bytes: 1:1 mapping keeps byte indices valid, and
    // every pattern is ASCII.
    let lower: Vec<u8> = text.bytes().map(|b| b.to_ascii_lowercase()).collect();
    labeled_hit(&lower).or_else(|| prefixed_hit(&lower))
}
