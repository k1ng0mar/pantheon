//! Plugin-import security invariants, exercised over real HTTP.
//!
//! `POST /api/plugins/import` takes `{url, ref?}` where `url` is a
//! `https://github.com/<owner>/<repo>` URL or a `clawhub:<slug>`. The
//! SSRF whitelist is enforced in `parse_source_spec` *before* any fetch
//! happens, so hostile URLs are rejected with 400 without touching the
//! network - that is what these tests assert.
//!
//! ZIP-level parser invariants (path traversal inside archives, zip
//! bombs) cannot reach the public surface: the endpoint only accepts
//! URLs, never archive bytes, so those unit tests were cut rather than
//! preserved behind a new test-only seam.

#[path = "support.rs"]
mod support;

use support::{boot, Dash, Resp};

fn import_url(d: &Dash, body: &str) -> Resp {
    d.post("/api/plugins/import", body)
}

fn import(d: &Dash, url: &str) -> Resp {
    import_url(d, &serde_json::json!({"url": url}).to_string())
}

#[test]
fn import_rejects_missing_or_empty_url() {
    let d = boot();
    let r = import_url(&d, "{}");
    assert_eq!(r.status, 400, "{}", r.body);
    let r = import(&d, "   ");
    assert_eq!(r.status, 400, "{}", r.body);
    // Not JSON at all.
    let r = import_url(&d, "{oops");
    assert_eq!(r.status, 400, "{}", r.body);
}

#[test]
fn import_rejects_non_github_hosts() {
    let d = boot();
    for url in [
        "https://evil.example.com/owner/repo",
        "https://raw.githubusercontent.com/owner/repo",
        "https://github.com.evil.example.com/owner/repo",
        "https://gist.github.com/owner/repo",
    ] {
        let r = import(&d, url);
        assert_eq!(r.status, 400, "{url}: {}", r.body);
    }
}

#[test]
fn import_rejects_non_https_scheme() {
    let d = boot();
    for url in [
        "http://github.com/owner/repo",
        "ftp://github.com/owner/repo",
    ] {
        let r = import(&d, url);
        assert_eq!(r.status, 400, "{url}: {}", r.body);
    }
}

#[test]
fn import_rejects_embedded_credentials() {
    let d = boot();
    let r = import(&d, "https://user:pass@github.com/owner/repo");
    assert_eq!(r.status, 400, "{}", r.body);
    let r = import(&d, "https://user@github.com/owner/repo");
    assert_eq!(r.status, 400, "{}", r.body);
}

#[test]
fn import_rejects_explicit_port() {
    let d = boot();
    let r = import(&d, "https://github.com:8443/owner/repo");
    assert_eq!(r.status, 400, "{}", r.body);
}

#[test]
fn import_rejects_loopback_and_private_ips() {
    let d = boot();
    for url in [
        "https://localhost/owner/repo",
        "https://127.0.0.1/owner/repo",
        "https://10.0.0.5/owner/repo",
        "https://192.168.1.1/owner/repo",
        "https://[::1]/owner/repo",
    ] {
        let r = import(&d, url);
        assert_eq!(r.status, 400, "{url}: {}", r.body);
    }
}

#[test]
fn import_rejects_deep_or_shallow_paths() {
    let d = boot();
    for url in [
        "https://github.com/owner/repo/extra",
        "https://github.com/owner",
        "https://github.com/",
    ] {
        let r = import(&d, url);
        assert_eq!(r.status, 400, "{url}: {}", r.body);
    }
}

#[test]
fn import_rejects_bad_refs() {
    let d = boot();
    // Traversal / spaces in the ref field.
    for gitref in ["../../etc", "..\\windows", "a b", ".hidden", "/abs"] {
        let r = import_url(
            &d,
            &serde_json::json!({"url": "https://github.com/owner/repo", "ref": gitref}).to_string(),
        );
        assert_eq!(r.status, 400, "ref {gitref:?}: {}", r.body);
    }
    // Traversal smuggled in the URL fragment.
    let r = import(&d, "https://github.com/owner/repo#../x");
    assert_eq!(r.status, 400, "{}", r.body);
}

#[test]
fn import_rejects_bad_clawhub_slugs() {
    let d = boot();
    for url in [
        "clawhub:",
        "clawhub:../evil",
        "clawhub:has space",
        "clawhub:.hidden",
        "clawhub:-dash",
        "clawhub:a/b/c",
    ] {
        let r = import(&d, url);
        assert_eq!(r.status, 400, "{url}: {}", r.body);
    }
}

#[test]
fn import_rejections_write_nothing_to_quarantine() {
    let d = boot();
    for url in [
        "https://evil.example.com/owner/repo",
        "https://127.0.0.1/owner/repo",
        "clawhub:../evil",
    ] {
        assert_eq!(import(&d, url).status, 400);
    }
    // Rejected imports never touch the plugin store.
    assert!(
        !d.dir.path().join("plugins").exists(),
        "rejected imports must not create the plugin store"
    );
}

#[test]
fn import_unreachable_github_url_fails_gracefully() {
    let d = boot();
    // Passes the whitelist, then the fetch fails: with network this is a
    // 404 from GitHub, without network a transport failure. Either way it
    // must be a graceful 4xx/5xx with a structured error code - never a
    // 500 and never a hang.
    let r = import(&d, "https://github.com/pantheon-nonexistent-owner-zz9/repo");
    assert!(
        r.status == 404 || r.status == 502,
        "expected graceful 404/502, got {}: {}",
        r.status,
        r.body
    );
    let code = r.json()["error"]["code"].as_str().unwrap_or("").to_string();
    assert!(
        code == "PLUGIN_IMPORT_NOT_FOUND" || code == "PLUGIN_IMPORT_FETCH",
        "unexpected error code {code}: {}",
        r.body
    );
}
