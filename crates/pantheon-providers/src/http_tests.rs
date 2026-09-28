//! Tests for `pantheon_providers::http::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_api::error::Layer;

fn rate_err(cause: &str) -> PantheonError {
    PantheonError::new("PROVIDER_HTTP", Layer::Provider, true, cause, "", "")
}

#[test]
fn retry_after_delta_seconds_parses_and_caps() {
    assert_eq!(parse_retry_after("5"), Some(5));
    assert_eq!(
        parse_retry_after("  7  "),
        Some(7),
        "surrounding whitespace"
    );
    assert_eq!(parse_retry_after("0"), Some(0));
    // The cap: a provider asking for two minutes gets sixty seconds.
    assert_eq!(parse_retry_after("120"), Some(MAX_RETRY_AFTER_SECS));
    assert_eq!(parse_retry_after("99999"), Some(MAX_RETRY_AFTER_SECS));
}

#[test]
fn retry_after_rejects_garbage() {
    assert_eq!(parse_retry_after(""), None);
    assert_eq!(parse_retry_after("soon"), None);
    assert_eq!(parse_retry_after("-3"), None);
    assert_eq!(parse_retry_after("1.5"), None);
    assert_eq!(parse_retry_after("Wed, 32 Foo 2099 00:00:00 GMT"), None);
    assert_eq!(parse_retry_after("Wed, 01 Jan 2099 00:00:00 UTC"), None);
}

#[test]
fn retry_after_http_date_parses() {
    // Long past: no wait.
    assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"), Some(0));
    // Far future: capped, not a multi-year sleep.
    assert_eq!(
        parse_retry_after("Wed, 01 Jan 2099 00:00:00 GMT"),
        Some(MAX_RETRY_AFTER_SECS)
    );
}

#[test]
fn retry_after_marker_round_trips_through_the_error() {
    // The exact shape `send()` stamps on a 429's cause.
    let e = rate_err("http://x/v1: HTTP 429 slow down (retry-after: 12s)");
    assert_eq!(retry_after_secs(&e), Some(12));
    // No stamp → no wait (a 429 without the header sleeps nothing).
    let e = rate_err("http://x/v1: HTTP 429 slow down");
    assert_eq!(retry_after_secs(&e), None);
    // A 500's cause never carries the marker.
    let e = rate_err("http://x/v1: HTTP 500 boom");
    assert_eq!(retry_after_secs(&e), None);
}

#[test]
fn auth_header_pair_matches_adapter_convention() {
    // Authorization → Bearer; anything else → raw key, same as the
    // adapters have always done.
    assert_eq!(
        auth_header_pair("Authorization", "k"),
        ("Authorization".to_string(), "Bearer k".to_string())
    );
    assert_eq!(
        auth_header_pair("authorization", "k").1,
        "Bearer k".to_string(),
        "case-insensitive"
    );
    assert_eq!(
        auth_header_pair("api-key", "k"),
        ("api-key".to_string(), "k".to_string())
    );
    assert_eq!(
        auth_header_pair("", "k").0,
        "Authorization".to_string(),
        "empty header name defaults"
    );
}

#[test]
fn turn_options_default_to_previous_wire_behavior() {
    let o = TurnOptions::default();
    assert!(o.response_schema.is_none());
    assert_eq!(o.tool_choice, ToolChoice::Auto);
}
