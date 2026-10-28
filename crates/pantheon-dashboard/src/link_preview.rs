//! `GET /api/link-preview?url=<url>`: rich link cards for chat.
//!
//! The mobile app renders link cards (title, image, domain) under
//! messages; this endpoint fetches the target page server-side and
//! extracts Open Graph metadata so the client never has to.
//!
//! Security model: the dashboard fetches an arbitrary URL the client
//! names, so every hop is SSRF-guarded - only `http`/`https`, the host
//! must resolve to public IPs (no loopback, private, link-local,
//! carrier-grade NAT, or unique-local ranges), and redirects are
//! followed manually (max 3) with the guard re-applied per hop.
//! Fetch is bounded: 5 s total timeout, ~1 MiB body cap.
//!
//! v1 notes: no caching (every card view re-fetches), and the DNS
//! check is a pre-flight - a resolver that answers differently on the
//! real connection (TOCTOU) is not defended against. Good enough for a
//! localhost dashboard; revisit before exposing this remotely.

use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use pantheon_gateway::http::{Request, Response};
use url::Url;

use crate::{err_json, json_ok};

const MAX_REDIRECTS: u32 = 3;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BODY_BYTES: u64 = 1024 * 1024;
const USER_AGENT: &str =
    "Mozilla/5.0 (compatible; Pantheon/1.0; +https://github.com/k1ng0mar/pantheon)";

/// Error carrying the HTTP status the endpoint should return.
pub struct PreviewError {
    pub status: u16,
    pub code: &'static str,
    pub msg: String,
}

fn fail(status: u16, code: &'static str, msg: impl Into<String>) -> PreviewError {
    PreviewError {
        status,
        code,
        msg: msg.into(),
    }
}

/// True when the IP is publicly routable. Rejects loopback, private,
/// link-local, unspecified, multicast, CGNAT (100.64.0.0/10), IPv6
/// unique-local (fc00::/7), and the reserved IPv4 ranges
/// 192.0.0.0/24, 198.18.0.0/15, and 240.0.0.0/4 - the ranges std's
/// helpers don't cover are checked by hand.
///
/// IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible (`::a.b.c.d`)
/// IPv6 addresses route to the IPv4 host, so they are judged by the
/// IPv4 rules: without the mapping, `::ffff:127.0.0.1` and
/// `::ffff:192.168.1.1` sailed past the V6 branch untouched.
///
/// Public so the behavioral suite in `eval/tests/link_preview_guard.rs` can
/// pin the rejection set; this is the SSRF boundary, not an internal detail.
pub fn is_public_ip(ip: &IpAddr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => is_public_v4(*v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4() {
                return is_public_v4(v4);
            }
            if v6.is_unicast_link_local() {
                return false;
            }
            // Unique-local fc00::/7.
            let seg0 = v6.segments()[0];
            if seg0 & 0xfe00 == 0xfc00 {
                return false;
            }
            true
        }
    }
}

/// IPv4 half of [`is_public_ip`], shared with the mapped-V6 path so
/// both spellings of an address get the same verdict.
fn is_public_v4(v4: std::net::Ipv4Addr) -> bool {
    // Loopback/unspecified/broadcast must be denied HERE, not just
    // by the outer `is_public_ip` check: an IPv4-mapped IPv6 address
    // (`::ffff:127.0.0.1`) is not loopback as V6, so it reaches this
    // function, and without these checks it would pass as public.
    if v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
    {
        return false;
    }
    let o = v4.octets();
    // Carrier-grade NAT: 100.64.0.0/10.
    if o[0] == 100 && (64..128).contains(&o[1]) {
        return false;
    }
    // IETF protocol assignments: 192.0.0.0/24 (includes NAT64
    // well-known prefixes - not a public host).
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return false;
    }
    // Benchmarking: 198.18.0.0/15.
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return false;
    }
    // Reserved: 240.0.0.0/4 (includes 255.255.255.255 broadcast).
    if o[0] >= 240 {
        return false;
    }
    // Everything else IPv4 is public (documentation ranges 192.0.2/24,
    // 198.51.100/24, 203.0.113/24 are harmless to allow: they route
    // nowhere real).
    true
}

/// SSRF guard: the URL's host must be http(s) with no credentials, and
/// every resolved IP must be public. Returns the parsed URL.
///
/// Public so the behavioral suite in `eval/tests/link_preview_guard.rs` can
/// exercise the exact rejection set this boundary enforces.
pub fn guard_url(raw: &str) -> Result<Url, PreviewError> {
    let url = Url::parse(raw)
        .map_err(|_| fail(422, "LINK_BAD_URL", "url is not a valid absolute URL"))?;
    match url.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(fail(
                422,
                "LINK_BAD_URL",
                "only http and https URLs are allowed",
            ));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(fail(
            422,
            "LINK_BAD_URL",
            "URLs with credentials are not allowed",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| fail(422, "LINK_BAD_URL", "URL has no host"))?;
    // Literal IP in the host: check it directly. `host_str()` brackets IPv6
    // literals (`"[::1]"`), which `IpAddr::parse` rejects, so unwrap the
    // brackets first - otherwise every IPv6 literal silently skips this
    // branch and falls through to the DNS path, where it is rejected only
    // because a bracketed host does not resolve. That made the guard's
    // literal check a no-op for the entire IPv6 space.
    let ip_candidate = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = ip_candidate.parse::<IpAddr>() {
        if !is_public_ip(&ip) {
            return Err(fail(422, "LINK_SSRF", "URL resolves to a non-public IP"));
        }
        return Ok(url);
    }
    // DNS name: resolve and check every answer.
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|_| fail(422, "LINK_BAD_URL", "could not resolve URL host"))?;
    let mut any = false;
    for addr in addrs {
        any = true;
        if !is_public_ip(&addr.ip()) {
            return Err(fail(422, "LINK_SSRF", "URL resolves to a non-public IP"));
        }
    }
    if !any {
        return Err(fail(
            422,
            "LINK_BAD_URL",
            "URL host resolved to no addresses",
        ));
    }
    Ok(url)
}

/// Extracted card fields; `None` serializes as null.
pub struct Card {
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub site_name: Option<String>,
}

/// ASCII case-insensitive byte search. Returns the byte offset in the
/// ORIGINAL string, so slicing stays valid for non-ASCII documents
/// (`str::to_lowercase` can change byte length and would misalign
/// indices).
fn find_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    h.windows(n.len()).position(|w| {
        w.iter()
            .zip(n.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

fn find_insensitive_from(haystack: &str, from: usize, needle: &str) -> Option<usize> {
    find_insensitive(&haystack[from..], needle).map(|p| from + p)
}

/// Pull one attribute value out of a tag string. Handles
/// `name="v"`, `name='v'`, and `name=v` forms, case-insensitively.
/// The match must sit on an attribute boundary (start of tag or
/// whitespace), so `data-content=` never matches a search for
/// `content=`.
fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let mut from = 0;
    let pos = loop {
        let rel = find_insensitive_from(tag, from, &needle)?;
        let boundary =
            rel == 0 || matches!(tag.as_bytes()[rel - 1], b' ' | b'\t' | b'\n' | b'\r' | b'<');
        if boundary {
            break rel;
        }
        from = rel + 1;
    };
    let rest = tag[pos + name.len() + 1..].trim_start();
    let mut chars = rest.chars();
    let first = chars.next()?;
    if first == '"' || first == '\'' {
        return rest[1..].split(first).next().map(|s| s.to_string());
    }
    Some(
        rest.split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string(),
    )
}

/// Scan `<meta ...>` tags for an og/property (or name) match and return
/// its `content` value.
fn meta_content(html: &str, key: &str) -> Option<String> {
    let mut pos = 0;
    while let Some(abs) = find_insensitive_from(html, pos, "<meta") {
        let end = find_insensitive_from(html, abs, ">")
            .map(|e| e + 1)
            .unwrap_or(html.len());
        let tag = &html[abs..end.min(html.len())];
        let prop = attr(tag, "property").or_else(|| attr(tag, "name"));
        if prop
            .as_deref()
            .map(|p| p.eq_ignore_ascii_case(key))
            .unwrap_or(false)
        {
            if let Some(c) = attr(tag, "content") {
                let c = c.trim();
                if !c.is_empty() {
                    return Some(c.to_string());
                }
            }
        }
        pos = end;
    }
    None
}

fn clean(s: &str) -> Option<String> {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Parse card fields out of an HTML document. `base` resolves relative
/// image URLs. Pure function - unit-tested with fixture HTML.
pub fn parse_preview_html(html: &str, base: &Url) -> Card {
    let title = meta_content(html, "og:title").or_else(|| {
        find_insensitive(html, "<title>").and_then(|s| {
            find_insensitive_from(html, s, "</title>").and_then(|e| clean(&html[s + 7..e]))
        })
    });
    let description =
        meta_content(html, "og:description").or_else(|| meta_content(html, "description"));
    let image = meta_content(html, "og:image")
        .and_then(|src| base.join(src.trim()).ok().map(|u| u.to_string()))
        .or_else(|| {
            // Fallback: first non-data, non-empty <img src>.
            let mut pos = 0;
            while let Some(abs) = find_insensitive_from(html, pos, "<img") {
                let end = find_insensitive_from(html, abs, ">")
                    .map(|e| e + 1)
                    .unwrap_or(html.len());
                let tag = &html[abs..end.min(html.len())];
                if let Some(src) = attr(tag, "src") {
                    let src = src.trim();
                    if !src.is_empty() && !src.starts_with("data:") {
                        return base.join(src).ok().map(|u| u.to_string());
                    }
                }
                pos = end;
            }
            None
        });
    let site_name = meta_content(html, "og:site_name");
    Card {
        title: title.and_then(|t| clean(&t)),
        description: description.and_then(|d| clean(&d)),
        image,
        site_name: site_name.and_then(|s| clean(&s)),
    }
}

fn fetch_html(url: &Url) -> Result<(Url, String), PreviewError> {
    // Manual redirect loop so the SSRF guard runs on every hop.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .max_redirects(0)
        .max_redirects_will_error(false)
        .user_agent(USER_AGENT)
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    let mut current = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        guard_url(current.as_str())?;
        let resp = agent
            .get(current.as_str())
            .call()
            .map_err(|e| fail(502, "LINK_FETCH", format!("fetch failed: {e}")))?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            let loc = resp
                .headers()
                .get("Location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| fail(502, "LINK_FETCH", "redirect without Location"))?;
            current = current
                .join(loc)
                .map_err(|_| fail(502, "LINK_FETCH", "bad redirect Location"))?;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(fail(
                502,
                "LINK_FETCH",
                format!("target returned HTTP {status}"),
            ));
        }
        let mut body = String::new();
        resp.into_body()
            .into_reader()
            .take(MAX_BODY_BYTES + 1)
            .read_to_string(&mut body)
            .map_err(|e| fail(502, "LINK_FETCH", format!("read failed: {e}")))?;
        return Ok((current, body));
    }
    Err(fail(502, "LINK_FETCH", "too many redirects"))
}

/// Fetch and summarize a URL into a link card.
pub fn fetch_preview(raw_url: &str) -> Result<serde_json::Value, PreviewError> {
    let url = guard_url(raw_url)?;
    let (final_url, html) = fetch_html(&url)?;
    let card = parse_preview_html(&html, &final_url);
    let domain = final_url.host_str().unwrap_or("").to_string();
    Ok(serde_json::json!({
        "url": final_url.to_string(),
        "title": card.title,
        "description": card.description,
        "image": card.image,
        "site_name": card.site_name,
        "domain": domain,
    }))
}

/// `GET /api/link-preview?url=<url>`.
pub fn link_preview(req: &Request) -> Response {
    let raw = req.query.get("url").map(String::as_str).unwrap_or("");
    if raw.trim().is_empty() {
        return err_json(422, "LINK_BAD_URL", "missing ?url= parameter");
    }
    match fetch_preview(raw) {
        Ok(v) => json_ok(v),
        Err(e) => err_json(e.status, e.code, &e.msg),
    }
}
