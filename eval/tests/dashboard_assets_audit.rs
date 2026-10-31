//! S5 regression: dashboard insertion-sink audit.
//!
//! The dashboard renders externally-sourced data (model output, tool
//! results, logs, memory records, approval args, third-party link
//! previews) with `innerHTML` throughout `app.js`. The audit that
//! cleared this file is pinned here as source contracts: every finding
//! has a corresponding assertion, so a refactor that drops an escape
//! or re-opens a URL scheme fails CI instead of shipping a sink.
//!
//! This is deliberately a source-text test (the asset has no JS test
//! runner in this repo): what must never regress is *the code shown
//! here*, not runtime behavior a headless browser would exercise.
//! `esc()` escaping and `safeUrl()` scheme gating are the two
//! primitives every assertion traces back to.

/// The full dashboard front-end asset under audit.
const APP_JS: &str = include_str!("../../crates/pantheon-dashboard/assets/app.js");

/// `esc()` must cover every character that can break out of text or an
/// attribute context.
#[test]
fn esc_covers_all_context_breaking_characters() {
    for pat in [
        r#""&": "&amp;""#,
        r#""<": "&lt;""#,
        r#"">": "&gt;""#,
        r#"'"': "&quot;""#,
        r#""'": "&#39;""#,
    ] {
        assert!(
            APP_JS.contains(pat),
            "esc() lost the {pat:?} mapping - text-context escaping is incomplete"
        );
    }
}

/// The S5 finding: a URL `href` needs a scheme allowlist, not just
/// esc(). `javascript:alert(1)` contains no metacharacter and executes
/// on click; esc() alone lets it through. Both variable-URL sinks (UI
/// document refs, link-preview cards) must route through `safeUrl`.
#[test]
fn url_hrefs_route_through_the_scheme_allowlist() {
    assert!(
        APP_JS.contains("function safeUrl(u)"),
        "safeUrl() scheme allowlist is missing"
    );
    assert!(
        APP_JS.contains("/^https?:\\/\\//i.test(s)"),
        "safeUrl() must accept http/https only"
    );
    // Positive: both sinks use it.
    assert!(
        APP_JS.contains("href=\"' + safeUrl(r.url)"),
        "UI refs href must go through safeUrl"
    );
    assert!(
        APP_JS.contains("href=\"' + safeUrl(p.url)"),
        "link-card href must go through safeUrl"
    );
    // Negative: the esc-only pattern must not come back on any variable
    // URL - esc() is attribute safety, not scheme safety.
    assert!(
        !APP_JS.contains("href=\"' + esc(r.url"),
        "UI refs regressed to esc-only href (javascript: scheme would pass)"
    );
    assert!(
        !APP_JS.contains("href=\"' + esc(p.url"),
        "link-card regressed to esc-only href"
    );
}

/// Markdown links (transcript, UI detail) must stay https?-anchored and
/// escape first - the D-11 contract. The `javascript:` scheme can never
/// reach an href through renderMd.
#[test]
fn render_md_escapes_first_and_only_links_http() {
    assert!(
        APP_JS.contains("let src = esc(text)"),
        "renderMd must escape the whole source before restoring constructs"
    );
    assert!(
        APP_JS.contains("\\]\\((https?:"),
        "renderMd markdown links must be scheme-anchored to http(s)"
    );
}

/// The highest-traffic external-data sinks: each must interpolate
/// through esc() (or renderMd, which escapes first). These are the
/// findings the audit walked one by one - they are pinned so the walk
/// never has to be repeated from scratch.
#[test]
fn external_data_sinks_interpolate_through_esc() {
    let pins: &[(&str, &str)] = &[
        ("log lines", "esc(l)"),
        ("SSE stream notes", "esc(d.note)"),
        ("transcript content", "renderMd(m.content)"),
        ("reasoning content", "esc(m.content)"),
        ("approval arguments (table)", "esc(a.args)"),
        ("approval tool names", "esc(a.tool)"),
        ("run titles", "esc(r.title"),
        ("model names", "esc(r.model"),
        ("memory record values", "esc(r.value"),
        ("memory provenance origin", "esc(r.provenance"),
        ("skills names/descriptions", "esc(s.description"),
        ("MCP server commands/urls", "esc(s.command || s.url"),
        ("scheduled job tasks", "esc(j.task.slice(0, 120))"),
        ("error messages", "esc(msg)"),
        ("UI document text", "renderMd(node.text || \"\")"),
        ("UI diff lines", "esc(sign + (l.text || \"\"))"),
    ];
    for (what, pat) in pins {
        assert!(
            APP_JS.contains(pat),
            "the {what} sink no longer escapes through {pat:?} - re-audit before shipping"
        );
    }
}

/// The one HTML-context helper whose color input is validated, not just
/// escaped: `expertAvatarHtml` builds `style="background:..."`. The
/// regex gate must stay.
#[test]
fn inline_style_colors_are_validated() {
    assert!(
        APP_JS.contains("/^#[0-9a-fA-F]{6}$/.test(String(color))"),
        "dynamic style= colors must keep the hex validation gate"
    );
}
