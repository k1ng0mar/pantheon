//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral - SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem - lives here and
//! runs via `cargo test -p pantheon-eval`.

//! Tests for `pantheon_dashboard::link_preview`'s SSRF guard - sibling file
//! so sources stay test-free.
//!
//! `guard_url` and `is_public_ip` are the boundary that stops the dashboard
//! fetching an arbitrary internal address (localhost services, cloud
//! metadata, RFC1918 hosts). They shipped with zero coverage; these tests
//! pin the rejection set so a later edit cannot silently widen it.
//!
//! Only pure guards are exercised here - no test performs a real outbound
//! request. `guard_url` on a DNS name does resolve, so those cases use
//! literal IPs or reserved names instead.

use pantheon_dashboard::link_preview::{guard_url, is_public_ip};
use std::net::IpAddr;

fn code(raw: &str) -> String {
    guard_url(raw)
        .expect_err("must be rejected")
        .code
        .to_string()
}

#[test]
fn loopback_literals_are_rejected() {
    for raw in [
        "http://127.0.0.1/",
        "http://127.0.0.1:8015/",
        "https://127.1.2.3/",
    ] {
        assert_eq!(code(raw), "LINK_SSRF", "{raw}");
        let err = guard_url(raw).unwrap_err();
        assert_eq!(err.status, 422, "{raw}");
    }
}

#[test]
fn rfc1918_and_link_local_literals_are_rejected() {
    let bad = [
        "http://10.0.0.1/",
        "http://172.16.0.1/",
        "http://192.168.1.1/",
        // Cloud metadata endpoint: the highest-value SSRF target.
        "http://169.254.169.254/latest/meta-data/",
        "http://0.0.0.0/",
        "http://224.0.0.1/",
    ];
    for raw in bad {
        assert_eq!(code(raw), "LINK_SSRF", "{raw}");
    }
}

#[test]
fn hand_checked_ranges_are_rejected() {
    // CGNAT 100.64.0.0/10 and IPv6 unique-local fc00::/7 are not covered by
    // std's helpers, so they are the likeliest to regress.
    for raw in ["http://100.64.0.1/", "http://100.127.255.254/"] {
        assert_eq!(code(raw), "LINK_SSRF", "{raw}");
    }
    // IPv6 literals arrive bracketed from `host_str()`; the guard must
    // unwrap the brackets or these skip the literal check entirely.
    for raw in [
        "http://[fc00::1]/",
        "http://[fd12:3456::1]/",
        "http://[fe80::1]/",
    ] {
        assert_eq!(code(raw), "LINK_SSRF", "{raw}");
    }
}

#[test]
fn ipv6_loopback_literal_is_rejected_as_ssrf() {
    // Regression: `::1` used to fall through the literal branch (the
    // bracketed host failed `IpAddr::parse`) and was rejected only because a
    // bracketed host does not resolve. It must be rejected by the guard.
    for raw in ["http://[::1]/", "http://[::1]:8080/", "https://[::1]/"] {
        assert_eq!(code(raw), "LINK_SSRF", "{raw}");
    }
}

#[test]
fn non_http_schemes_are_rejected() {
    // file:// would be a direct local read.
    for raw in ["file:///etc/passwd", "ftp://example.com/x"] {
        assert_eq!(code(raw), "LINK_BAD_URL", "{raw}");
    }
}

#[test]
fn embedded_credentials_are_rejected() {
    // A credentialed URL lets the guard's literal check and the HTTP
    // client's request disagree about the host; reject outright.
    for raw in ["http://user@example.com/", "http://user:pass@example.com/"] {
        assert_eq!(code(raw), "LINK_BAD_URL", "{raw}");
    }
}

#[test]
fn malformed_and_hostless_urls_are_rejected() {
    for raw in ["not a url", "/relative/path", ""] {
        assert!(guard_url(raw).is_err(), "{raw:?} must not pass the guard");
    }
}

#[test]
fn localhost_name_is_rejected() {
    // Resolves to loopback; must not slip through because the resolver was
    // asked rather than the literal being parsed.
    assert_eq!(code("http://localhost:8080/"), "LINK_SSRF");
}

#[test]
fn is_public_ip_matches_the_guard_contract() {
    for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
        assert!(
            is_public_ip(&ip.parse::<IpAddr>().unwrap()),
            "{ip} is public"
        );
    }
    for ip in [
        "127.0.0.1",
        "10.1.2.3",
        "172.20.0.5",
        "192.168.0.1",
        "169.254.1.1",
        "100.64.0.1",
        "0.0.0.0",
    ] {
        assert!(
            !is_public_ip(&ip.parse::<IpAddr>().unwrap()),
            "{ip} must not be treated as public"
        );
    }
    // The top of the CGNAT block is the classic off-by-one.
    assert!(!is_public_ip(&"100.127.255.255".parse::<IpAddr>().unwrap()));
}
