//! Markdown rendering for the transcript's left panel.
//!
//! Pure functions: markdown text in, styled [`Line`]s out. No I/O, no
//! terminal access - fully unit-testable.
//!
//! Supported:
//! - ATX headings (`# ...`) → theme heading (muted blue), bold
//! - Bare section headers (`Root cause`, `Changes`, ...): a short plain
//!   line (≤ 48 chars, no trailing colon, no list marker, no backticks)
//!   preceded by a blank line (or start of text) and followed by a
//!   non-blank line → theme heading, bold
//! - Tag lines (`+ Thought · 8.9s`): leading `+` in amber bold, rest dim
//! - Unordered (`-`, `*`, `+`) and ordered (`1.`) lists: dim marker,
//!   styled body
//! - Pipe tables: header row in heading color bold, body rows aligned
//!   with two-space column gaps; the `|---|` rule line and outer pipes
//!   are dropped (no box-drawing anywhere)
//! - Fenced code blocks: fences hidden, body indented two spaces in
//!   theme code green, blank line before/after
//! - Blockquotes (`> ...`): dim marker, styled body
//! - Inline: `**bold**` (amber bold), `` `code` `` (theme code green),
//!   `*em*` / `_em_` (theme emphasis amber italic), `[text](url)`
//!   (theme primary)
//!
//! All styling uses semantic [`Theme`] fields only; no literal colors
//! appear here.

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::session::theme::Theme;

/// Maximum length (chars) of a bare section header or a tag line.
const SHORT_LINE: usize = 48;

/// Render markdown `text` to styled lines (no trailing blank line).
pub fn render_markdown(text: &str, th: &Theme) -> Vec<Line<'static>> {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    let mut in_fence = false;

    while i < n {
        let line = lines[i];

        // --- fenced code -------------------------------------------------
        if line.trim_start().starts_with("```") {
            if in_fence {
                in_fence = false;
                // Blank line after the block, only when the next line is non-blank.
                if i + 1 < n && !is_blank(lines[i + 1]) {
                    out.push(Line::default());
                }
            } else {
                in_fence = true;
                // Blank line before the block, only when the previous line is non-blank.
                if out.last().is_some_and(|l| l.width() > 0) {
                    out.push(Line::default());
                }
            }
            i += 1;
            continue;
        }
        if in_fence {
            if is_blank(line) {
                out.push(Line::default());
            } else {
                out.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(th.code),
                )));
            }
            i += 1;
            continue;
        }

        // --- blank lines --------------------------------------------------
        if is_blank(line) {
            out.push(Line::default());
            i += 1;
            continue;
        }

        // --- pipe tables --------------------------------------------------
        if is_table_start(&lines, i) {
            let (table, next) = render_table(&lines, i, th);
            out.extend(table);
            i = next;
            continue;
        }

        let trimmed = line.trim();

        // --- ATX headings (`# ...`, space required after the hashes) --------
        if let Some(heading) = atx_heading(trimmed) {
            out.push(Line::from(Span::styled(
                heading.to_string(),
                Style::default().fg(th.heading).bold(),
            )));
            i += 1;
            continue;
        }

        // --- tag lines (`+ Thought · 8.9s`) --------------------------------
        if trimmed.chars().count() <= SHORT_LINE {
            if let Some(rest) = trimmed.strip_prefix("+ ") {
                out.push(Line::from(vec![
                    Span::styled("+", Style::default().fg(th.emphasis).bold()),
                    Span::styled(format!(" {rest}"), Style::default().fg(th.dim)),
                ]));
                i += 1;
                continue;
            }
        }

        // --- blockquotes ---------------------------------------------------
        if let Some(body) = trimmed.strip_prefix('>') {
            let mut spans = vec![Span::styled("> ", Style::default().fg(th.dim))];
            spans.extend(inline(body.trim_start(), th));
            out.push(Line::from(spans));
            i += 1;
            continue;
        }

        // --- lists ----------------------------------------------------------
        if let Some((marker, body)) = list_item(trimmed) {
            let mut spans = vec![Span::styled(
                marker.to_string(),
                Style::default().fg(th.dim),
            )];
            spans.extend(inline(body, th));
            out.push(Line::from(spans));
            i += 1;
            continue;
        }

        // --- bare section headers -------------------------------------------
        if is_section_header(&lines, i) {
            out.push(Line::from(Span::styled(
                trimmed.to_string(),
                Style::default().fg(th.heading).bold(),
            )));
            i += 1;
            continue;
        }

        // --- plain body ------------------------------------------------------
        out.push(Line::from(inline(trimmed, th)));
        i += 1;
    }

    // No trailing blank line.
    while out.last().is_some_and(|l| l.width() == 0) {
        out.pop();
    }
    out
}

fn is_blank(s: &str) -> bool {
    s.trim().is_empty()
}

/// `# Title` → `Some("Title")`; requires 1-6 hashes followed by a space.
fn atx_heading(trimmed: &str) -> Option<&str> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    Some(rest.trim())
}

/// `- item` / `* item` / `+ item` (long only; short `+ ` lines are tags) /
/// `1. item` → `Some((marker, body))`. Ordered markers keep their number.
fn list_item(trimmed: &str) -> Option<(&str, &str)> {
    for marker in ["- ", "* "] {
        if let Some(body) = trimmed.strip_prefix(marker) {
            return Some((marker, body));
        }
    }
    // `+ ` lines longer than SHORT_LINE stay list items; shorter ones are tags.
    if trimmed.chars().count() > SHORT_LINE {
        if let Some(body) = trimmed.strip_prefix("+ ") {
            return Some(("+ ", body));
        }
    }
    // Ordered: `1. `, `12. `, ...
    let mut digits = 0;
    for c in trimmed.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else {
            break;
        }
    }
    if digits > 0 && trimmed[digits..].starts_with(". ") {
        let marker_len = digits + 2; // digits + ". "
        return Some((&trimmed[..marker_len], &trimmed[marker_len..]));
    }
    None
}

/// Bare section header: short plain line (≤ 48 chars, no trailing colon),
/// not starting with a list marker / `>` / `#` / backtick / `|` / `+ `,
/// preceded by a blank line (or start of text) and followed by a non-blank line.
fn is_section_header(lines: &[&str], i: usize) -> bool {
    let trimmed = lines[i].trim();
    if trimmed.is_empty() || trimmed.chars().count() > SHORT_LINE || trimmed.ends_with(':') {
        return false;
    }
    for prefix in [">", "#", "`", "|", "+ "] {
        if trimmed.starts_with(prefix) {
            return false;
        }
    }
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return false;
    }
    let mut digits = 0;
    for c in trimmed.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else {
            break;
        }
    }
    if digits > 0 && trimmed[digits..].starts_with(". ") {
        return false;
    }
    if i > 0 && !is_blank(lines[i - 1]) {
        return false;
    }
    if i + 1 >= lines.len() || is_blank(lines[i + 1]) {
        return false;
    }
    true
}

/// Table start: this line and the next both start with `|`, and the next
/// matches the rule pattern `^\|[\s:\-|]+\|$`.
fn is_table_start(lines: &[&str], i: usize) -> bool {
    if i + 1 >= lines.len() {
        return false;
    }
    let first = lines[i].trim_start();
    let second = lines[i + 1].trim();
    if !first.starts_with('|') || !second.starts_with('|') {
        return false;
    }
    is_rule_line(second)
}

fn is_rule_line(trimmed: &str) -> bool {
    if !(trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() >= 2) {
        return false;
    }
    trimmed[1..trimmed.len() - 1]
        .chars()
        .all(|c| c == ' ' || c == '\t' || c == ':' || c == '-' || c == '|')
}

/// Split a `| a | b |` row into trimmed cells, dropping the outer pipes.
fn table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// Render a pipe table starting at `i`; returns the lines and the index of
/// the first line after the table.
fn render_table(lines: &[&str], i: usize, th: &Theme) -> (Vec<Line<'static>>, usize) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut j = i;
    while j < lines.len() && lines[j].trim_start().starts_with('|') {
        rows.push(table_cells(lines[j]));
        j += 1;
    }
    // rows[0] = header, rows[1] = rule (dropped), rows[2..] = body.
    let body_rows: Vec<&Vec<String>> = rows
        .iter()
        .enumerate()
        .filter(|(k, _)| *k != 1)
        .map(|(_, r)| r)
        .collect();
    let cols = body_rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in &body_rows {
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(cell.chars().count());
        }
    }
    let mut out = Vec::new();
    for (ri, row) in body_rows.iter().enumerate() {
        let padded: Vec<String> = (0..cols)
            .map(|c| {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                format!("{cell:<width$}", width = widths[c])
            })
            .collect();
        let text = padded.join("  ");
        let style = if ri == 0 {
            Style::default().fg(th.heading).bold()
        } else {
            Style::default().fg(th.body)
        };
        out.push(Line::from(Span::styled(text, style)));
    }
    (out, j)
}

/// Inline formatting applied to body text after block detection:
/// `**bold**` → bold body, `` `code` `` → theme code, `*em*`/`_em_` →
/// theme emphasis italic, `[text](url)` → theme primary (URL dropped).
/// A lone backtick is literal.
fn inline(text: &str, th: &Theme) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut rest = text;

    let flush = |buf: &mut String, out: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            out.push(Span::styled(
                std::mem::take(buf),
                Style::default().fg(th.body),
            ));
        }
    };

    while !rest.is_empty() {
        // `[text](url)` → text in primary, URL dropped.
        if rest.starts_with('[') {
            if let Some((spans, remaining)) = try_link(rest, th) {
                flush(&mut buf, &mut out);
                out.extend(spans);
                rest = remaining;
                continue;
            }
        }
        // `**bold**` → amber bold (matches reference: bold keywords render amber).
        if rest.starts_with("**") {
            if let Some(end) = rest[2..].find("**") {
                flush(&mut buf, &mut out);
                out.push(Span::styled(
                    rest[2..2 + end].to_string(),
                    Style::default().fg(th.emphasis).bold(),
                ));
                rest = &rest[2 + end + 2..];
                continue;
            }
            buf.push_str("**");
            rest = &rest[2..];
            continue;
        }
        // `` `code` `` → theme code; a lone backtick is literal.
        if rest.starts_with('`') {
            if let Some(end) = rest[1..].find('`') {
                flush(&mut buf, &mut out);
                out.push(Span::styled(
                    rest[1..1 + end].to_string(),
                    Style::default().fg(th.code),
                ));
                rest = &rest[1 + end + 1..];
                continue;
            }
            buf.push('`');
            rest = &rest[1..];
            continue;
        }
        // `*em*` / `_em_` → theme emphasis italic.
        if rest.starts_with('*') || rest.starts_with('_') {
            let delim = rest.chars().next().unwrap();
            if let Some(end) = rest[delim.len_utf8()..].find(delim) {
                flush(&mut buf, &mut out);
                out.push(Span::styled(
                    rest[delim.len_utf8()..delim.len_utf8() + end].to_string(),
                    Style::default().fg(th.emphasis).italic(),
                ));
                rest = &rest[delim.len_utf8() + end + delim.len_utf8()..];
                continue;
            }
            buf.push(delim);
            rest = &rest[delim.len_utf8()..];
            continue;
        }
        let ch = rest.chars().next().unwrap();
        buf.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush(&mut buf, &mut out);
    out
}

/// Try to parse `[text](url)` at the start of `rest`. Returns the styled
/// spans (nested inline formatting kept, recolored to primary) and the
/// remaining text after the closing paren.
fn try_link<'a>(rest: &'a str, th: &Theme) -> Option<(Vec<Span<'static>>, &'a str)> {
    let close_bracket = rest.find(']')?;
    if !rest[close_bracket + 1..].starts_with('(') {
        return None;
    }
    let after_paren = close_bracket + 2;
    let close_paren = rest[after_paren..].find(')')?;
    let link_text = &rest[1..close_bracket];
    let remaining = &rest[after_paren + close_paren + 1..];
    let mut spans = inline(link_text, th);
    for span in spans.iter_mut() {
        span.style = span.style.patch(Style::default().fg(th.primary));
    }
    Some((spans, remaining))
}
