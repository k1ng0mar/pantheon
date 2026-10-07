//! Display classification for provider failures.
//!
//! The chain emits `ModelEvent::AttemptFailed` with a raw `(code, cause)`;
//! this module turns that pair into the small, stable kind the TUI card
//! and the dashboard timeline show. Rate limits get their own kind on
//! purpose: a quota failure needs a visibly different state from a dead
//! key or a down endpoint, and `PROVIDER_HTTP` alone cannot tell them
//! apart - the HTTP status lives in the cause string.

use std::fmt;

/// The kind of a provider failure, for display only. Recovery policy
/// still keys off `PantheonError::retryable`; this enum never decides
/// whether the chain falls back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// HTTP 429. Transient: another key or provider may still have quota.
    RateLimited,
    /// HTTP 401/403. The credential is dead or lacks access.
    Auth,
    /// The request never completed in time ("timed out" in the cause).
    Timeout,
    /// DNS / connection / transport failure below HTTP.
    Network,
    /// HTTP 5xx. The provider's side is broken.
    Server,
    /// Other HTTP 4xx, or a provider config problem. Our side is wrong.
    Client,
    /// Anything the shapes above do not cover. The card still shows the
    /// raw code, so unknown never means silent.
    Unknown,
}

impl ProviderErrorKind {
    /// Short label for the error card header.
    pub fn label(&self) -> &'static str {
        match self {
            ProviderErrorKind::RateLimited => "rate limited",
            ProviderErrorKind::Auth => "auth failed",
            ProviderErrorKind::Timeout => "timed out",
            ProviderErrorKind::Network => "network error",
            ProviderErrorKind::Server => "server error",
            ProviderErrorKind::Client => "request error",
            ProviderErrorKind::Unknown => "provider error",
        }
    }

    /// Rate limits render in the warning tone, not the failure tone, so
    /// quota trouble is visibly distinct from a dead provider.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, ProviderErrorKind::RateLimited)
    }
}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Classify a provider failure from its `(code, cause)`. The HTTP status
/// is embedded in the cause (`send()` writes `"{url}: HTTP {code} ..."`),
/// so the cause is matched first; the code only disambiguates the
/// non-HTTP shapes.
pub fn classify_provider_error(code: &str, cause: &str) -> ProviderErrorKind {
    // Order matters: "HTTP 429" contains "HTTP 4" as a substring, and the
    // specific statuses must win over the class checks.
    if cause.contains("HTTP 429") {
        return ProviderErrorKind::RateLimited;
    }
    if cause.contains("HTTP 401") || cause.contains("HTTP 403") {
        return ProviderErrorKind::Auth;
    }
    if cause.to_lowercase().contains("timed out") {
        return ProviderErrorKind::Timeout;
    }
    if cause.contains("HTTP 5") {
        return ProviderErrorKind::Server;
    }
    if cause.contains("HTTP 4") {
        return ProviderErrorKind::Client;
    }
    if cause.contains("Dns Failed")
        || cause.contains("Connection Failed")
        || cause.contains("Network Error")
    {
        return ProviderErrorKind::Network;
    }
    if code == "PROVIDER_CONFIG" {
        return ProviderErrorKind::Client;
    }
    ProviderErrorKind::Unknown
}

/// First line of `cause`, char-truncated to `max`. Provider causes carry
/// a URL plus up to a 300-char body snippet; cards and ledger rows want
/// the one-line shape.
pub fn short_snippet(cause: &str, max: usize) -> String {
    let line: String = cause
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(max)
        .collect();
    line.trim_end().to_string()
}

/// The readable tail of a provider cause: for HTTP failures the part
/// from the status code on (the base URL adds nothing once the card
/// already names provider/model); for transport failures the URL prefix
/// is stripped (the doubled `url: url:` from `send()` included).
pub fn display_message(cause: &str) -> String {
    let mut msg = cause;
    if let Some(i) = msg.find("HTTP ") {
        msg = &msg[i..];
    } else {
        // Strip leading "scheme://..." prefixes; transport causes repeat it.
        while let Some(colon) = msg.find(": ") {
            let head = &msg[..colon];
            if head.contains("://") && !head.contains(' ') {
                msg = &msg[colon + 2..];
            } else {
                break;
            }
        }
    }
    short_snippet(msg, 200)
}

/// Re-read the capped `Retry-After` wait a 429 asked for, from the marker
/// `send()` stamps on the error cause. `None` = no wait requested.
pub fn retry_after_secs_from_cause(cause: &str) -> Option<u64> {
    let marker = "(retry-after: ";
    let start = cause.rfind(marker)? + marker.len();
    let rest = cause.get(start..)?;
    let end = rest.find('s')?;
    rest[..end].parse::<u64>().ok()
}
