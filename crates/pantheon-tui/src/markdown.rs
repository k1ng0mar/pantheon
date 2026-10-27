//! Markdown rendering for the transcript's left panel.
//!
//! Pure functions: markdown text in, styled [`Line`]s out. No I/O, no
//! terminal access - fully unit-testable.
//!
//! Supported:
//! - ATX headings (`# ...`) → theme heading (muted blue), bold;
//!   closing hashes (`## Title ##`) are stripped
//! - Setext headings (`Title` followed by `===` / `---`) → theme heading
//! - Bare section headers (`Root cause`, `Changes`, ...): a short plain
//!   line (≤ 48 chars, no trailing colon, no list marker, no backticks)
//!   preceded by a blank line (or start of text) and followed by a
//!   non-blank line → theme heading, bold
//! - Tag lines (`+ Thought · 8.9s`): leading `+` in amber bold, rest dim
//! - Unordered (`-`, `*`, `+`) and ordered (`1.`) lists: dim marker,
//!   styled body; nested lists keep their indent; task lists
//!   (`- [ ]` / `- [x]`) render as ☐ / ☑
//! - Pipe tables: header row in heading color bold, body rows aligned
//!   with two-space column gaps and the rule line's column alignment
//!   (`:---`, `:---:`, `---:`); the rule line and outer pipes are
//!   dropped (no box-drawing anywhere); tables without outer pipes
//!   are recognised too; escaped pipes (`\|`) stay inside a cell
//! - Fenced code blocks: fences hidden, body indented two spaces in
//!   theme code green, blank line before/after
//! - Horizontal rules (`---`, `***`, `___`): a dim `· · ·` separator
//! - Blockquotes (`> ...`): dim marker, styled body
//! - Inline: `**bold**` / `__bold__` (amber bold), `` `code` `` and
//!   double-backtick code spans (theme code green),
//!   `*em*` / `_em_` (theme emphasis amber italic), `~~struck~~`
//!   (body, crossed out), `[text](url)` and `![alt](url)`
//!   (theme primary, URL dropped), `<https://…>` / `<user@host>`
//!   autolinks and bare `https://…` / `www.…` URLs (theme primary),
//!   backslash escapes (`\*`, `\|`, …) render the escaped character
//!
//! All styling uses semantic [`Theme`] fields only; no literal colors
//! appear here. Raw HTML other than autolinks is left literal on
//! purpose: stripping `<…>` would mangle generics like `Vec<T>`.

use ratatui::{
    style::{Modifier, Style},
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

        // --- setext headings (`Title` + `===` / `---` on the next line) -----
        // Only for plain text lines: a heading, list item, blockquote or
        // tag line followed by `===`/`---` is not a setext heading.
        if setext_underline(&lines, i).is_some() && is_setext_text(trimmed) {
            out.push(Line::from(Span::styled(
                trimmed.to_string(),
                Style::default().fg(th.heading).bold(),
            )));
            i += 2;
            continue;
        }

        // --- horizontal rules (`---`, `***`, `___`) -------------------------
        // Rendered as a dim separator, not the raw syntax. A `---` line
        // after plain text was already consumed as a setext underline
        // above, so a `---` reaching here is a genuine rule.
        if is_hr(trimmed) {
            out.push(Line::from(Span::styled(
                "· · ·",
                Style::default().fg(th.dim),
            )));
            i += 1;
            continue;
        }

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
        // Nested lists keep their leading indent (top-level indent is 0,
        // so existing top-level rendering is unchanged).
        if let Some((marker, body)) = list_item(trimmed) {
            let indent = line.len() - line.trim_start().len();
            let mut spans = vec![Span::styled(
                format!("{}{}", " ".repeat(indent), marker),
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
/// A closing hash sequence (`## Title ##`) is stripped, per GFM.
fn atx_heading(trimmed: &str) -> Option<&str> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    let mut text = rest.trim();
    // Strip a closing sequence: hashes at end of line, preceded by a
    // space (`## Title ##` → `Title`). A `#` glued to the title
    // (`Title#`) is not a closing sequence and is kept.
    let stripped = text.trim_end_matches('#');
    if stripped != text && stripped.ends_with(' ') {
        text = stripped.trim_end();
    }
    Some(text)
}

/// Setext underline on the next line: one or more `=` (level 1) or
/// `-` (level 2), nothing else. Returns the underline kind, or `None`.
fn setext_underline(lines: &[&str], i: usize) -> Option<char> {
    let next = lines.get(i + 1)?.trim();
    if next.is_empty() {
        return None;
    }
    let kind = next.chars().next()?;
    if (kind == '=' || kind == '-') && next.chars().all(|c| c == kind) {
        Some(kind)
    } else {
        None
    }
}

/// Whether `trimmed` can be setext heading text: plain text only, not a
/// heading, list item, blockquote, tag line, table row, or another
/// underline/rule line.
fn is_setext_text(trimmed: &str) -> bool {
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with('>')
        || trimmed.starts_with("|")
        || trimmed.starts_with("+ ")
        || trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.chars().all(|c| c == '=' || c == '-')
    {
        return false;
    }
    // Ordered list markers are not setext text either.
    let mut digits = 0;
    for c in trimmed.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else {
            break;
        }
    }
    !(digits > 0 && trimmed[digits..].starts_with(". "))
}

/// Horizontal rule: three or more of the same `-`, `*`, or `_`,
/// optionally separated by spaces. A `---` line after plain text is
/// consumed as a setext underline before this is reached.
fn is_hr(trimmed: &str) -> bool {
    let chars: Vec<char> = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    chars.len() >= 3
        && chars.iter().all(|&c| c == chars[0])
        && (chars[0] == '-' || chars[0] == '*' || chars[0] == '_')
}

/// `- item` / `* item` / `+ item` (long only; short `+ ` lines are tags) /
/// `1. item` → `Some((marker, body))`. Ordered markers keep their number.
/// Task lists (`- [ ] item`, `- [x] item`) render as `☐ ` / `☑ `.
fn list_item(trimmed: &str) -> Option<(String, &str)> {
    for marker in ["- ", "* "] {
        if let Some(body) = trimmed.strip_prefix(marker) {
            if let Some((checkbox, rest)) = task_checkbox(body) {
                return Some((checkbox, rest));
            }
            return Some((marker.to_string(), body));
        }
    }
    // `+ ` lines longer than SHORT_LINE stay list items; shorter ones are tags.
    if trimmed.chars().count() > SHORT_LINE {
        if let Some(body) = trimmed.strip_prefix("+ ") {
            return Some(("+ ".to_string(), body));
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
        return Some((trimmed[..marker_len].to_string(), &trimmed[marker_len..]));
    }
    None
}

/// Task-list checkbox: `[ ] ` → `☐ `, `[x] ` / `[X] ` → `☑ `.
/// Anything else returns `None` (the body is a plain list item).
fn task_checkbox(body: &str) -> Option<(String, &str)> {
    if let Some(rest) = body.strip_prefix("[ ] ") {
        return Some(("☐ ".to_string(), rest));
    }
    if let Some(rest) = body
        .strip_prefix("[x] ")
        .or_else(|| body.strip_prefix("[X] "))
    {
        return Some(("☑ ".to_string(), rest));
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

/// Table start: this line contains a pipe and the next line is a rule
/// line (cells of `:---`, `:---:`, `---:` separated by pipes, outer
/// pipes optional). A rule line without any pipe is not a table rule —
/// that keeps `a | b` followed by a plain `---` from being misread,
/// since that `---` is a setext underline or a horizontal rule.
fn is_table_start(lines: &[&str], i: usize) -> bool {
    if i + 1 >= lines.len() {
        return false;
    }
    let first = lines[i].trim_start();
    let second = lines[i + 1].trim();
    if !first.contains('|') {
        return false;
    }
    is_rule_line(second)
}

fn is_rule_line(trimmed: &str) -> bool {
    if !trimmed.contains('|') {
        return false;
    }
    let mut inner = trimmed;
    if let Some(rest) = inner.strip_prefix('|') {
        inner = rest;
    }
    if let Some(rest) = inner.strip_suffix('|') {
        inner = rest;
    }
    // Every cell must be only colons and dashes, with at least one dash.
    !inner.is_empty()
        && inner.split('|').all(|cell| {
            let c = cell.trim();
            !c.is_empty() && c.chars().all(|ch| ch == ':' || ch == '-') && c.contains('-')
        })
}

/// Column alignment parsed from one rule cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

fn rule_alignments(line: &str) -> Vec<Align> {
    table_cells(line)
        .iter()
        .map(|cell| {
            let c = cell.trim();
            let left = c.starts_with(':');
            let right = c.ends_with(':');
            if left && right {
                Align::Center
            } else if right {
                Align::Right
            } else {
                Align::Left
            }
        })
        .collect()
}

/// Split a `| a | b |` row into trimmed cells, dropping the outer pipes.
/// An escaped pipe (`\|`) stays inside its cell.
fn table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'|') {
            chars.next();
            cur.push('|');
        } else if ch == '|' {
            cells.push(cur.trim().to_string());
            cur = String::new();
        } else {
            cur.push(ch);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

fn pad_cell(cell: &str, width: usize, align: Align) -> String {
    let len = cell.chars().count();
    if len >= width {
        return cell.to_string();
    }
    match align {
        Align::Left => format!("{cell:<width$}"),
        Align::Right => format!("{cell:>width$}"),
        Align::Center => {
            let pad = width - len;
            let left = pad / 2;
            let right = pad - left;
            format!("{}{cell}{}", " ".repeat(left), " ".repeat(right))
        }
    }
}

/// Render a pipe table starting at `i`; returns the lines and the index of
/// the first line after the table.
fn render_table(lines: &[&str], i: usize, th: &Theme) -> (Vec<Line<'static>>, usize) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut j = i;
    while j < lines.len() && lines[j].contains('|') {
        rows.push(table_cells(lines[j]));
        j += 1;
    }
    // rows[0] = header, rows[1] = rule (dropped), rows[2..] = body.
    // Alignment per column, from the rule line's cells (`:---` left,
    // `:---:` center, `---:` right). Colons survive cell trimming, so
    // parsing the raw rule line is exact.
    let rule_aligns = lines
        .get(i + 1)
        .map(|l| rule_alignments(l))
        .unwrap_or_default();
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
    // Missing columns default to left.
    let col_align: Vec<Align> = (0..cols)
        .map(|c| rule_aligns.get(c).copied().unwrap_or(Align::Left))
        .collect();
    let mut out = Vec::new();
    for (ri, row) in body_rows.iter().enumerate() {
        let padded: Vec<String> = (0..cols)
            .map(|c| {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                pad_cell(cell, widths[c], col_align[c])
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
        // Backslash escape: `\*`, `\|`, `\\`, … render the escaped
        // character, dropping the backslash. Checked first so an escaped
        // marker is never parsed as formatting. Does not apply inside
        // code spans (those consume raw text) or fenced blocks.
        if rest.starts_with('\\') && rest.len() > 1 {
            let next = rest[1..].chars().next().unwrap();
            if next.is_ascii_punctuation() {
                buf.push(next);
                rest = &rest[1 + next.len_utf8()..];
                continue;
            }
        }
        // `![alt](url)` → alt text in primary, URL and `!` dropped.
        // Checked before `[text](url)` so the `!` does not leak.
        if rest.starts_with("![") {
            if let Some((spans, remaining)) = try_link(&rest[1..], th) {
                flush(&mut buf, &mut out);
                out.extend(spans);
                rest = remaining;
                continue;
            }
        }
        // `[text](url)` → text in primary, URL dropped.
        if rest.starts_with('[') {
            if let Some((spans, remaining)) = try_link(rest, th) {
                flush(&mut buf, &mut out);
                out.extend(spans);
                rest = remaining;
                continue;
            }
        }
        // `<https://…>` / `<user@host>` autolinks → content in primary,
        // brackets dropped. Anything else in `<…>` (e.g. `Vec<T>`) is
        // left literal: stripping it would mangle generics.
        if rest.starts_with('<') {
            if let Some((url, remaining)) = try_autolink(rest) {
                flush(&mut buf, &mut out);
                out.push(Span::styled(
                    url.to_string(),
                    Style::default().fg(th.primary),
                ));
                rest = remaining;
                continue;
            }
        }
        // Bare URLs: `https://…`, `http://…`, `www.…` → primary, with
        // trailing sentence punctuation left as body text.
        if rest.starts_with("https://") || rest.starts_with("http://") || rest.starts_with("www.") {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '<' || c == '`' || c == '"' || c == '\'')
                .unwrap_or(rest.len());
            let raw = &rest[..end];
            let trimmed = raw
                .trim_end_matches(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']'));
            // A bare `www.` needs a real host after the prefix; a lone
            // `www.` (or `www` with no dot) is plain text, not a link.
            let www_ok =
                !rest.starts_with("www.") || (trimmed.len() > 4 && trimmed[4..].contains('.'));
            if !trimmed.is_empty() && www_ok {
                flush(&mut buf, &mut out);
                out.push(Span::styled(
                    trimmed.to_string(),
                    Style::default().fg(th.primary),
                ));
                // Trailing punctuation goes back into the body buffer.
                buf.push_str(&raw[trimmed.len()..]);
                rest = &rest[end..];
                continue;
            }
        }
        // `~~struck~~` → body, crossed out.
        if rest.starts_with("~~") {
            if let Some(end) = rest[2..].find("~~") {
                flush(&mut buf, &mut out);
                out.push(Span::styled(
                    rest[2..2 + end].to_string(),
                    Style::default()
                        .fg(th.body)
                        .add_modifier(Modifier::CROSSED_OUT),
                ));
                rest = &rest[2 + end + 2..];
                continue;
            }
            buf.push_str("~~");
            rest = &rest[2..];
            continue;
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
        // `__bold__` → amber bold. Checked before single-`_` emphasis,
        // which would otherwise mangle it into empty spans.
        if rest.starts_with("__") {
            if let Some(end) = rest[2..].find("__") {
                flush(&mut buf, &mut out);
                out.push(Span::styled(
                    rest[2..2 + end].to_string(),
                    Style::default().fg(th.emphasis).bold(),
                ));
                rest = &rest[2 + end + 2..];
                continue;
            }
            buf.push_str("__");
            rest = &rest[2..];
            continue;
        }
        // Double-backtick code spans: ``` ``code `x` `` ``` → theme code.
        if rest.starts_with("``") {
            if let Some(end) = rest[2..].find("``") {
                flush(&mut buf, &mut out);
                out.push(Span::styled(
                    rest[2..2 + end].to_string(),
                    Style::default().fg(th.code),
                ));
                rest = &rest[2 + end + 2..];
                continue;
            }
            buf.push_str("``");
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

/// Try to parse an autolink `<https://…>` or `<user@host>` at the start
/// of `rest`. Returns the link content (brackets dropped) and the
/// remaining text after `>`. Anything else in `<…>` returns `None` so
/// the caller leaves it literal (see the inline autolink comment).
fn try_autolink<'a>(rest: &'a str) -> Option<(&'a str, &'a str)> {
    let close = rest.find('>')?;
    let content = &rest[1..close];
    if content.is_empty() || content.contains(|c: char| c.is_whitespace()) {
        return None;
    }
    let is_url = content.starts_with("https://")
        || content.starts_with("http://")
        || content.starts_with("ftp://");
    let is_email = {
        let mut parts = content.split('@');
        let local = parts.next().unwrap_or("");
        let domain = parts.next().unwrap_or("");
        // Exactly one `@`, non-empty local part, domain with a dot.
        !local.is_empty() && !domain.is_empty() && parts.next().is_none() && domain.contains('.')
    };
    if is_url || is_email {
        Some((content, &rest[close + 1..]))
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::pantheon()
    }

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(input: &str) -> String {
        text(&render_markdown(input, &theme()))
    }

    #[test]
    fn strikethrough_drops_tildes_and_marks_crossed_out() {
        let lines = render_markdown("a ~~gone~~ b", &theme());
        assert_eq!(text(&lines), "a gone b");
        let span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "gone")
            .expect("struck span");
        assert!(
            span.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "span style: {:?}",
            span.style
        );
    }

    #[test]
    fn task_lists_render_checkboxes() {
        let t = render("- [ ] todo\n- [x] done\n- [X] also done\n- plain");
        assert!(t.contains("☐ todo"), "got: {t}");
        assert!(t.contains("☑ done"), "got: {t}");
        assert!(t.contains("☑ also done"), "got: {t}");
        assert!(t.contains("- plain"), "got: {t}");
        assert!(!t.contains("[ ]") && !t.contains("[x]"), "got: {t}");
    }

    #[test]
    fn autolink_angle_and_bare_url_drop_brackets_keep_url_primary() {
        let lines = render_markdown(
            "see <https://example.com/a> and https://example.com/b.",
            &theme(),
        );
        let t = text(&lines);
        assert_eq!(t, "see https://example.com/a and https://example.com/b.");
        let primary: Vec<&str> = lines[0]
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(theme().primary))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(primary, ["https://example.com/a", "https://example.com/b"]);
        let email = render("mail <user@example.com> ok");
        assert_eq!(email, "mail user@example.com ok");
    }

    #[test]
    fn generics_are_not_mangled_as_autolinks() {
        assert_eq!(render("use Vec<T> here"), "use Vec<T> here");
    }

    #[test]
    fn table_alignment_and_no_outer_pipes_and_escaped_pipe() {
        let t = render("| Name | Cost |\n| :--- | ---: |\n| ab | 5 |\n| abcdef | 42 |");
        let lines: Vec<&str> = t.lines().collect();
        // Header: left-aligned name column, right-aligned cost column.
        assert_eq!(lines[0], "Name    Cost");
        assert_eq!(lines[1], "ab         5");
        assert_eq!(lines[2], "abcdef    42");
        // Center alignment.
        let t = render("| A | B |\n| :---: | :--- |\n| x | y |");
        assert_eq!(t.lines().next().unwrap(), "A  B");
        // No outer pipes.
        let t = render("Name | Cost\n--- | ---:\nfoo | 7");
        assert!(t.contains("foo"), "got: {t}");
        assert!(!t.contains('|'), "got: {t}");
        // Escaped pipe stays inside a cell.
        let t = render("| A |\n| --- |\n| a \\| b |");
        assert!(t.contains("a | b"), "got: {t}");
    }

    #[test]
    fn nested_lists_keep_indent() {
        let t = render("- top\n  - nested\n    - deeper");
        assert!(t.contains("- top"), "got: {t}");
        assert!(t.contains("  - nested"), "got: {t}");
        assert!(t.contains("    - deeper"), "got: {t}");
    }

    #[test]
    fn setext_headings_and_horizontal_rule() {
        assert_eq!(render("Title\n===\nbody"), "Title\nbody");
        assert_eq!(render("Sub\n---\nbody"), "Sub\nbody");
        let lines = render_markdown("Title\n===\nbody", &theme());
        assert_eq!(lines[0].spans[0].style.fg, Some(theme().heading));
        // The blank input line stays a blank output line.
        assert_eq!(render("para\n\n---\nnext"), "para\n\n· · ·\nnext");
        assert_eq!(render("para\n\n***\nnext"), "para\n\n· · ·\nnext");
    }

    #[test]
    fn atx_closing_hashes_and_underscore_bold_and_image_and_double_code() {
        assert_eq!(render("## Title ##"), "Title");
        assert_eq!(render("__bold__ text"), "bold text");
        assert_eq!(render("![alt text](https://example.com/i.png)"), "alt text");
        assert_eq!(render("``code `x` ``"), "code `x` ");
        assert_eq!(render("\\*not em\\*"), "*not em*");
    }

    #[test]
    fn existing_features_still_render() {
        let t = render("# Head\n\n- item\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n\n```rust\nlet x = 1;\n```\n\n> quote\n\n**bold** and `code` and [link](https://example.com)");
        assert!(t.contains("Head"), "got: {t}");
        assert!(t.contains("- item"), "got: {t}");
        assert!(t.contains("  let x = 1;"), "got: {t}");
        assert!(!t.contains("```"), "got: {t}");
        assert!(t.contains("> quote"), "got: {t}");
        assert!(!t.contains("https://example.com"), "got: {t}");
        assert!(!t.contains('|'), "got: {t}");
    }
}
